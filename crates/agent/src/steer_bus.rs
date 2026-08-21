//! Host-side `AgentBus` + `Steer` tool — the unified inter-agent messaging
//! and spawn mechanism. Routes `Steer` messages between `User`, the
//! singleton `Captain`, and dynamically-spawned `Subagent` instances. Sits
//! on the kernel's intra-session `steer` (`HarnessHandle::steer`). TS Pi
//! has no cross-session agent bus — this is a manox host extension.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, Weak};

use pi::harness::HarnessHandle;
use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use pi::types::{AgentMessage, ContentBlock};
use pi_extensions::agents::SubagentTool;
use pi_extensions::steer_bus::{AgentId, BusOp, SteerPayload, SteerReason, ToSpec};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::background_task::{self, TaskKind, TaskStatus};
use crate::thread::ThreadEvent;
use crate::thread_engine::BackendNotice;

// ── AgentBus ─────────────────────────────────────────────────────────────

/// A live in-thread subagent coroutine (transient, not a manox Thread).
/// The `handle` allows Inject/Abort; the `cancel` token propagates
/// parent-turn abort to the spawned task.
#[derive(Clone)]
pub struct LiveSubagent {
    pub handle: HarnessHandle,
    pub cancel: CancellationToken,
}

/// Parent routing info injected into a member thread's bus so the member
/// can address its parent Captain by thread id.
#[derive(Clone)]
pub struct ParentRoute {
    pub parent_thread_id: String,
    pub parent_notice_tx: mpsc::UnboundedSender<BackendNotice>,
}

/// The host-side agent bus. One per thread (Captain or member). The
/// Captain's bus tracks live subagents + spawned member thread-ids; a
/// member's bus carries a `parent_route` to address its parent.
pub struct AgentBus {
    owner_thread_id: String,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    live_subagents: Mutex<BTreeMap<String, LiveSubagent>>,
    spawned_members: Mutex<HashSet<String>>,
    parent_route: Mutex<Option<ParentRoute>>,
    task_list: Mutex<Option<Arc<Mutex<crate::team::TaskList>>>>,
    captain_handle: Mutex<Option<HarnessHandle>>,
    weak_self: Mutex<Weak<AgentBus>>,
    subagent_tool: Mutex<Option<Arc<SubagentTool>>>,
    tool_ctx: Mutex<Option<Arc<dyn ToolContext>>>,
}

impl AgentBus {
    /// Construct the bus and return it wrapped in an `Arc`. The `weak_self`
    /// field is set internally so `steer()` can create child `SteerTool`
    /// instances that share this bus.
    pub fn new(
        owner_thread_id: String,
        notice_tx: mpsc::UnboundedSender<BackendNotice>,
    ) -> Arc<Self> {
        let bus = Arc::new(Self {
            owner_thread_id,
            notice_tx,
            live_subagents: Mutex::new(BTreeMap::new()),
            spawned_members: Mutex::new(HashSet::new()),
            parent_route: Mutex::new(None),
            task_list: Mutex::new(None),
            captain_handle: Mutex::new(None),
            weak_self: Mutex::new(Weak::new()),
            subagent_tool: Mutex::new(None),
            tool_ctx: Mutex::new(None),
        });
        *bus.weak_self.lock().unwrap() = Arc::downgrade(&bus);
        bus
    }

    /// Late-bind the Captain's session handle.
    pub fn bind_captain(&self, handle: HarnessHandle) {
        *self.captain_handle.lock().unwrap() = Some(handle);
    }

    pub fn set_parent_route(&self, route: ParentRoute) {
        *self.parent_route.lock().unwrap() = Some(route);
    }

    pub fn set_task_list(&self, list: Arc<Mutex<crate::team::TaskList>>) {
        *self.task_list.lock().unwrap() = Some(list);
    }

    pub fn set_subagent_tool(&self, tool: Arc<SubagentTool>) {
        *self.subagent_tool.lock().unwrap() = Some(tool);
    }

    pub fn set_tool_ctx(&self, ctx: Arc<dyn ToolContext>) {
        *self.tool_ctx.lock().unwrap() = Some(ctx);
    }

    /// The main Steer routing entry point.
    pub async fn steer(
        &self,
        from: AgentId,
        to: ToSpec,
        reason: SteerReason,
        payload: SteerPayload,
    ) -> Result<AgentToolResult, ToolError> {
        let addr = &to.agent_address;
        let spawn = to.spawn.as_deref();
        let isolation = to.isolation.as_deref();

        match (&from, &reason, spawn) {
            // ── Dispatch: spawn subagent (capability def name) ─────────
            (AgentId::Captain, SteerReason::Dispatch, Some(spawn_type))
                if spawn_type != "TeamMember" =>
            {
                self.dispatch_subagent(addr, spawn_type, isolation, &payload.text)
                    .await
            }

            // ── Dispatch: spawn TeamMember (real thread) ───────────────
            (AgentId::Captain, SteerReason::Dispatch, Some("TeamMember")) => {
                self.dispatch_member(addr, &payload.text).await
            }

            // ── Inject to existing subagent ─────────────────────────────
            (AgentId::Captain, SteerReason::Inject, None) if addr == "Captain" => Err(
                ToolError::InvalidArguments("cannot steer User or self".into()),
            ),
            (AgentId::Captain, SteerReason::Inject, None)
                if self.live_subagents.lock().unwrap().contains_key(addr) =>
            {
                let sub = self.live_subagents.lock().unwrap().get(addr).cloned();
                if let Some(live) = sub {
                    live.handle.steer(AgentMessage::user(payload.text.clone()));
                    return Ok(ack(addr, false, None));
                }
                Err(ToolError::ExecutionFailed(format!("agent {addr} gone")))
            }

            // ── Inject to member thread ─────────────────────────────────
            (AgentId::Captain, SteerReason::Inject, None) if self.is_member(addr) => self
                .bus_request(BusOp::InjectMember {
                    thread_id: addr.to_string(),
                    payload: payload.text.clone(),
                })
                .await
                .map(|_| ack(addr, false, None)),

            // ── Inject from subagent to parent Captain ──────────────────
            (AgentId::Subagent(_), SteerReason::Inject, None) if addr == "Captain" => {
                let _ = self.notice_tx.send(BackendNotice::SteerDelivered {
                    from: from.clone(),
                    reason: SteerReason::Inject,
                    payload,
                });
                Ok(ack(addr, false, None))
            }

            // ── Abort subagent ──────────────────────────────────────────
            (AgentId::Captain, SteerReason::Abort, None)
                if self.live_subagents.lock().unwrap().contains_key(addr) =>
            {
                let mut map = self.live_subagents.lock().unwrap();
                if let Some(live) = map.remove(addr) {
                    live.cancel.cancel();
                    drop(map);
                    let _ = self.notice_tx.send(BackendNotice::SteerDelivered {
                        from: AgentId::Subagent(format!("subagent {addr}")),
                        reason: SteerReason::Complete,
                        payload: SteerPayload {
                            text: "subagent aborted".into(),
                        },
                    });
                    Ok(ack(addr, false, None))
                } else {
                    Err(ToolError::ExecutionFailed(format!("agent {addr} gone")))
                }
            }

            // ── Abort member thread ─────────────────────────────────────
            (AgentId::Captain, SteerReason::Abort, None) if self.is_member(addr) => self
                .bus_request(BusOp::AbortMember {
                    thread_id: addr.to_string(),
                })
                .await
                .map(|_| ack(addr, false, None)),

            // ── Permission denied ───────────────────────────────────────
            _ => Err(ToolError::InvalidArguments(format!(
                "steer not allowed: from={from:?} to={addr} reason={reason:?} spawn={spawn:?}"
            ))),
        }
    }

    /// Check if `addr` is a known member thread id.
    fn is_member(&self, addr: &str) -> bool {
        self.spawned_members.lock().unwrap().contains(addr)
    }

    /// Dispatch a subagent coroutine: spawn session, register bg task,
    /// run in tokio, emit SteerDelivered on completion.
    async fn dispatch_subagent(
        &self,
        addr: &str,
        spawn_type: &str,
        isolation: Option<&str>,
        prompt: &str,
    ) -> Result<AgentToolResult, ToolError> {
        let addr = addr.to_string();
        let spawn_type = spawn_type.to_string();
        let isolation = isolation.map(String::from);
        let prompt = prompt.to_string();
        let subagent_tool = self
            .subagent_tool
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| ToolError::ExecutionFailed("subagent tool not configured".into()))?;
        let tool_ctx = self
            .tool_ctx
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| ToolError::ExecutionFailed("tool context not configured".into()))?;

        // Create child SteerTool (limited: from=Subagent, Inject-only).
        let bus_arc = self
            .weak_self
            .lock()
            .unwrap()
            .upgrade()
            .ok_or_else(|| ToolError::ExecutionFailed("bus gone".into()))?;
        let child_steer = Arc::new(SteerTool::new(bus_arc, AgentId::Subagent(addr.to_string())));

        // Spawn the session.
        let (mut session, session_dir, worktree) = subagent_tool
            .spawn_subagent_session(
                &spawn_type,
                isolation.as_deref(),
                &*tool_ctx,
                vec![child_steer],
            )
            .await?;
        let handle = session.handle();

        // Register background task for the sidebar card.
        let task_cancel = CancellationToken::new();
        let description = first_line(&prompt).unwrap_or_else(|| format!("Subagent {spawn_type}"));
        let (task_id, task) = background_task::register(
            TaskKind::Subagent,
            self.owner_thread_id.clone(),
            description,
            task_cancel.clone(),
        );
        let _ = self.notice_tx.send(BackendNotice::Event(Box::new(
            ThreadEvent::BackgroundTaskUpdated {
                snapshot: task.snapshot(&task_id),
            },
        )));
        // Emit SubagentProgress so the "智能体" tab shows the subagent.
        let _ = self.notice_tx.send(BackendNotice::Event(Box::new(
            ThreadEvent::SubagentProgress {
                id: addr.clone(),
                subagent_type: spawn_type.clone(),
                tool_uses: 0,
                token_usage: crate::language_model::TokenUsage::default(),
                latest_activity: Some(
                    first_line(&prompt).unwrap_or_else(|| format!("Subagent {spawn_type}")),
                ),
                status: crate::thread::ToolCallStatus::Running,
            },
        )));

        // Track in live_subagents for Inject/Abort.
        self.live_subagents.lock().unwrap().insert(
            addr.to_string(),
            LiveSubagent {
                handle: handle.clone(),
                cancel: task_cancel.clone(),
            },
        );

        // Spawn the run task.
        let notice_tx = self.notice_tx.clone();
        let weak = self.weak_self.lock().unwrap().clone();
        let addr_clone = addr.clone();
        let label = format!("subagent {addr}");
        let full_prompt = format!(
            "{prompt}\n\nWhen done, end your turn with a concise summary: \
             what you changed (files + intent), what you ran (commands + \
             outcomes), and the final result."
        );
        let tool_ctx2 = tool_ctx.clone();

        // session_dir (TempDir) must outlive the spawned task — the session
        // JSONL file lives inside it; dropping it mid-run deletes the dir.
        crate::runtime::handle().spawn(async move {
            let _session_dir = session_dir; // hold TempDir alive for session lifetime
            // Bridge the child session's streamed events to the workspace so
            // the sub-agent panel + rail show live dynamics (the retired
            // JSON bridge's successor). The subscription must outlive the loop.
            let (ev_tx, mut ev_rx) =
                tokio::sync::mpsc::unbounded_channel::<pi::types::AgentEvent>();
            let _subscription = session.subscribe(Arc::new(move |event, _cancel| {
                let _ = ev_tx.send(event);
                Box::pin(async move {})
            }));
            let mut prompt_fut = Box::pin(session.prompt(&full_prompt));
            let result = loop {
                tokio::select! {
                    r = prompt_fut.as_mut() => break r,
                    Some(event) = ev_rx.recv() => {
                        for ev in crate::pi_engine::adapt::child_events_of(&addr_clone, &event) {
                            let _ = notice_tx.send(BackendNotice::Event(Box::new(ev)));
                        }
                    }
                }
            };
            // Clean up worktree.
            if let Some(wt) = worktree {
                let _ = wt.clean_up(&*tool_ctx2).await;
            }
            match result {
                Ok(messages) => {
                    let content = extract_final_text(&messages);
                    task.set_terminal_status(TaskStatus::Completed);
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(
                        ThreadEvent::BackgroundTaskUpdated {
                            snapshot: task.snapshot(&task_id),
                        },
                    )));
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(
                        ThreadEvent::SubagentProgress {
                            id: addr_clone.clone(),
                            subagent_type: spawn_type.clone(),
                            tool_uses: 0,
                            token_usage: crate::language_model::TokenUsage::default(),
                            latest_activity: Some(content.clone()),
                            status: crate::thread::ToolCallStatus::Success,
                        },
                    )));
                    if !content.is_empty() {
                        let _ = notice_tx.send(BackendNotice::SteerDelivered {
                            from: AgentId::Subagent(label),
                            reason: SteerReason::Complete,
                            payload: SteerPayload { text: content },
                        });
                    }
                }
                Err(e) => {
                    task.set_terminal_status(TaskStatus::Failed);
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(
                        ThreadEvent::BackgroundTaskUpdated {
                            snapshot: task.snapshot(&task_id),
                        },
                    )));
                    let _ = notice_tx.send(BackendNotice::Event(Box::new(
                        ThreadEvent::SubagentProgress {
                            id: addr_clone.clone(),
                            subagent_type: spawn_type.clone(),
                            tool_uses: 0,
                            token_usage: crate::language_model::TokenUsage::default(),
                            latest_activity: Some(
                                format!("failed: {e}").chars().take(80).collect(),
                            ),
                            status: crate::thread::ToolCallStatus::Error,
                        },
                    )));
                    let _ = notice_tx.send(BackendNotice::SteerDelivered {
                        from: AgentId::Subagent(label),
                        reason: SteerReason::Complete,
                        payload: SteerPayload {
                            text: format!("subagent failed: {e}"),
                        },
                    });
                }
            }
            // Remove from live_subagents.
            if let Some(bus) = weak.upgrade() {
                bus.live_subagents.lock().unwrap().remove(&addr_clone);
            }
        });

        Ok(ack(&addr, true, None))
    }

    /// Dispatch a TeamMember (real thread): send BusRequest to facade.
    async fn dispatch_member(
        &self,
        name: &str,
        prompt: &str,
    ) -> Result<AgentToolResult, ToolError> {
        let result = self
            .bus_request(BusOp::SpawnMember {
                name: name.to_string(),
                prompt: prompt.to_string(),
            })
            .await?;
        // result is the thread id.
        self.spawned_members.lock().unwrap().insert(result.clone());
        Ok(ack(name, true, Some(result)))
    }

    /// Send a BusRequest to the facade (gpui main thread) and await reply.
    async fn bus_request(&self, op: BusOp) -> Result<String, ToolError> {
        let (tx, rx) = async_channel::bounded(1);
        self.notice_tx
            .send(BackendNotice::BusRequest { op, responder: tx })
            .map_err(|_| ToolError::ExecutionFailed("engine actor gone".into()))?;
        rx.recv()
            .await
            .map_err(|_| ToolError::ExecutionFailed("bus request dropped".into()))?
            .map_err(ToolError::ExecutionFailed)
    }
}

/// Build a JSON ack tool_result.
fn ack(addr: &str, spawned: bool, thread_id: Option<String>) -> AgentToolResult {
    AgentToolResult::text(
        serde_json::json!({
            "delivered": true,
            "agent_address": addr,
            "spawned": spawned,
            "thread_id": thread_id,
        })
        .to_string(),
    )
}

/// The first non-empty line of a prompt, for the background-task card title.
fn first_line(prompt: &str) -> Option<String> {
    prompt
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Extract the final assistant text from a session's message list.
fn extract_final_text(messages: &[AgentMessage]) -> String {
    for msg in messages.iter().rev() {
        if let AgentMessage::Assistant { content, .. } = msg {
            let text: String = content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

// ── SteerTool ────────────────────────────────────────────────────────────

/// The model-facing Steer tool — unified inter-agent messaging + spawn.
pub struct SteerTool {
    bus: Arc<AgentBus>,
    from: AgentId,
}

impl SteerTool {
    pub fn new(bus: Arc<AgentBus>, from: AgentId) -> Self {
        Self { bus, from }
    }
}

#[async_trait::async_trait]
impl AgentTool for SteerTool {
    fn name(&self) -> &str {
        "Steer"
    }

    fn description(&self) -> &str {
        "Send a typed message (Steer) to another agent. Use `to.agent_address` \
         to address a subagent (in-thread coroutine) or a TeamMember thread id; \
         set `to.spawn` to a capability def name (e.g. 'Sailor','Explore') to \
         create an in-thread subagent, or 'TeamMember' to create a real manox \
         Thread (process). Only the Captain may spawn. `reason` is Dispatch \
         (start a task), Inject (mid-run message), or Abort (cancel). Complete \
         is harness-emitted on subagent termination — not callable here."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn requires_approval(&self, _params: &serde_json::Value) -> bool {
        false
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "to": {
                    "type": "object",
                    "properties": {
                        "agent_address": {
                            "type": "string",
                            "description": "Target address. For a subagent: the caller-chosen address (in-thread coroutine). For a TeamMember: the system thread id (returned in the spawn ack). Reserved: 'Captain', 'User'."
                        },
                        "spawn": {
                            "type": "string",
                            "description": "Optional. A capability def name (e.g. 'Sailor','Explore') creates an in-thread subagent coroutine; 'TeamMember' creates a real manox Thread (process: persisted, sidebar-visible, resumable, own Captain session + own bus). Only the Captain may set. Error if address exists. Cannot be 'Captain' or 'User'."
                        }
                    },
                    "required": ["agent_address"]
                },
                "reason": {
                    "type": "string",
                    "enum": ["Dispatch", "Inject", "Abort"],
                    "description": "Complete is harness-emitted on subagent termination, not callable here."
                },
                "prompt": {
                    "type": "string",
                    "description": "Payload: task (Dispatch), message (Inject). Abort ignores it."
                },
                "isolation": {
                    "type": "string",
                    "enum": ["worktree"],
                    "description": "Optional throwaway git worktree for a spawned subagent. Only with to.spawn = a capability def (not TeamMember)."
                }
            },
            "required": ["to", "reason", "prompt"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let to = params
            .get("to")
            .ok_or_else(|| ToolError::InvalidArguments("'to' is required".into()))?;
        let agent_address = to
            .get("agent_address")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("'to.agent_address' is required".into()))?;
        let spawn = to.get("spawn").and_then(|v| v.as_str());
        let reason_str = params
            .get("reason")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("'reason' is required".into()))?;
        let prompt = params
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("'prompt' is required".into()))?;
        let isolation = params.get("isolation").and_then(|v| v.as_str());

        if reason_str == "Complete" {
            return Err(ToolError::ExecutionFailed(
                "Complete is harness-emitted on termination, not callable as a tool".into(),
            ));
        }
        if spawn == Some("Captain") || spawn == Some("User") {
            return Err(ToolError::InvalidArguments(
                "cannot spawn User or Captain".into(),
            ));
        }
        if spawn.is_some() && self.from != AgentId::Captain {
            return Err(ToolError::InvalidArguments(
                "only the Captain may spawn".into(),
            ));
        }

        let to_spec = ToSpec {
            agent_address: agent_address.to_string(),
            spawn: spawn.map(String::from),
            isolation: isolation.map(String::from),
        };
        let reason = match reason_str {
            "Dispatch" => SteerReason::Dispatch,
            "Inject" => SteerReason::Inject,
            "Abort" => SteerReason::Abort,
            _ => {
                return Err(ToolError::InvalidArguments(format!(
                    "unknown reason: {reason_str}"
                )));
            }
        };
        let payload = SteerPayload {
            text: prompt.to_string(),
        };

        self.bus
            .steer(self.from.clone(), to_spec, reason, payload)
            .await
    }
}
