//! Host-side `AgentBus` + `Steer` tool — the unified inter-agent messaging
//! and spawn mechanism. Routes `Steer` messages between `User`, the
//! singleton `Captain`, and dynamically-spawned `Subagent` instances. Sits
//! on the kernel's intra-session `steer` (`HarnessHandle::steer`). TS Pi
//! has no cross-session agent bus — this is a manox host extension.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Monotonic dispatch generation; a settled run only removes its own
    /// entry so a same-address re-dispatch is not orphaned by the old run.
    pub run: u64,
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
    run_seq: AtomicU64,
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
            run_seq: AtomicU64::new(0),
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
            // Cancel-only: the run task's select loop observes the token,
            // aborts the child session and settles silently (no SteerDelivered
            // here — Complete emission lives solely in the settle path, so an
            // explicit abort neither double-reports nor revives the Captain).
            (AgentId::Captain, SteerReason::Abort, None)
                if self.live_subagents.lock().unwrap().contains_key(addr) =>
            {
                let live = self.live_subagents.lock().unwrap().get(addr).cloned();
                if let Some(live) = live {
                    live.cancel.cancel();
                    live.handle.abort();
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

        // Reserved addresses would collide with routing: an address of
        // "Captain"/"User" would shadow the self-reject / user Inject arms.
        if addr == "Captain" || addr == "User" {
            return Err(ToolError::InvalidArguments(format!(
                "cannot use reserved address {addr}"
            )));
        }
        // Existence check: re-dispatching a live address would overwrite the
        // first run and orphan it (un-Abort/Inject-able). Matches the schema's
        // "Error if address exists" contract.
        if self.live_subagents.lock().unwrap().contains_key(&addr) {
            return Err(ToolError::InvalidArguments(format!(
                "agent {addr} already exists"
            )));
        }
        let run = self.run_seq.fetch_add(1, Ordering::SeqCst);
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
                run,
            },
        );
        // Spawn the run task.
        let notice_tx = self.notice_tx.clone();
        let weak = self.weak_self.lock().unwrap().clone();
        let addr_clone = addr.clone();
        let label = addr.clone();
        let full_prompt = format!(
            "{prompt}\n\nWhen done, end your turn with a concise summary: \
             what you changed (files + intent), what you ran (commands + \
             outcomes), and the final result."
        );
        let tool_ctx2 = tool_ctx.clone();

        // session_dir (TempDir) must outlive the spawned task — the session
        // JSONL file lives inside it; dropping it mid-run deletes the dir.
        let run_cancel = task_cancel.clone();
        let run_handle = handle.clone();
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
                    // Abort / TaskStop / thread-deletion cancel: stop the child
                    // instead of letting it burn tokens to natural completion.
                    _ = run_cancel.cancelled() => {
                        drop(prompt_fut);
                        run_handle.abort();
                        break Err(anyhow::anyhow!("aborted"));
                    }
                }
            };
            let aborted = run_cancel.is_cancelled();
            // Clean up the worktree; a kept (non-pristine) worktree's note must
            // ride the Complete payload so the caller can find the edits.
            let kept = match worktree {
                Some(wt) => wt.clean_up(&*tool_ctx2).await,
                None => None,
            };
            let kept_note = kept
                .as_ref()
                .map(|k| format!("\n\n[worktree kept: {k}]"))
                .unwrap_or_default();
            if aborted {
                task.set_terminal_status(TaskStatus::Stopped);
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
                        latest_activity: Some("aborted".into()),
                        status: crate::thread::ToolCallStatus::Cancelled,
                    },
                )));
                // Silent settle on explicit abort: no SteerDelivered, so the
                // Captain is not revived for a cancellation it requested.
            } else {
                match result {
                    Ok(messages) => {
                        let content = format!(
                            "{}{kept_note}",
                            truncate_final(&extract_final_text(&messages))
                        );
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
                                text: format!("subagent failed: {e}{kept_note}"),
                            },
                        });
                    }
                }
            }
            // Remove only our own run (identity check) so an older settle can't
            // orphan a same-address re-dispatch.
            if let Some(bus) = weak.upgrade() {
                let mut map = bus.live_subagents.lock().unwrap();
                if map.get(&addr_clone).is_some_and(|l| l.run == run) {
                    map.remove(&addr_clone);
                }
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

/// Cap a subagent's final summary before it rides the Complete payload into the
/// caller's next context window (the retired execute_inner truncated at the
/// same order of magnitude).
const FINAL_MAX_BYTES: usize = 128 * 1024;
const FINAL_MAX_LINES: usize = 2000;
fn truncate_final(text: &str) -> String {
    let by_lines: String = text
        .lines()
        .take(FINAL_MAX_LINES)
        .collect::<Vec<_>>()
        .join("\n");
    if by_lines.len() > FINAL_MAX_BYTES {
        by_lines.chars().take(FINAL_MAX_BYTES).collect()
    } else {
        by_lines
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bus() -> Arc<AgentBus> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        AgentBus::new("thread-1".into(), tx)
    }

    fn to(addr: &str, spawn: Option<&str>) -> ToSpec {
        ToSpec {
            agent_address: addr.into(),
            spawn: spawn.map(String::from),
            isolation: None,
        }
    }

    fn payload(text: &str) -> SteerPayload {
        SteerPayload { text: text.into() }
    }

    #[test]
    fn truncate_final_caps_lines_and_bytes() {
        let many_lines: String = (0..2500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = truncate_final(&many_lines);
        assert_eq!(out.lines().count(), FINAL_MAX_LINES);

        let big = "x".repeat(FINAL_MAX_BYTES + 10);
        assert!(truncate_final(&big).len() <= FINAL_MAX_BYTES);
    }

    #[test]
    fn first_line_skips_blank_lead() {
        assert_eq!(
            first_line("\n  \n  do the thing\nmore"),
            Some("do the thing".into())
        );
        assert_eq!(first_line("   "), None);
    }

    #[tokio::test]
    async fn subagent_cannot_spawn() {
        let bus = bus();
        let err = bus
            .steer(
                AgentId::Subagent("w1".into()),
                to("w2", Some("Sailor")),
                SteerReason::Dispatch,
                payload("x"),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
    }

    #[tokio::test]
    async fn captain_cannot_inject_self() {
        let bus = bus();
        let err = bus
            .steer(
                AgentId::Captain,
                to("Captain", None),
                SteerReason::Inject,
                payload("x"),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot steer"), "{err}");
    }

    #[tokio::test]
    async fn abort_unknown_address_denied() {
        let bus = bus();
        let err = bus
            .steer(
                AgentId::Captain,
                to("ghost", None),
                SteerReason::Abort,
                payload("x"),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not allowed"), "{err}");
    }
}
