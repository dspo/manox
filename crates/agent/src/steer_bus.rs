//! Host-side `AgentBus` + `Steer` tool — the unified inter-agent messaging
//! and spawn mechanism. Routes `Steer` messages between `User`, the
//! singleton `Captain`, and dynamically-spawned `Subagent` instances. Sits
//! on the kernel's intra-session `steer` (`HarnessHandle::steer`). TS Pi
//! has no cross-session agent bus — this is a manox host extension.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

use pi::ext_point_agent::AgentDef;
use pi::harness::HarnessHandle;
use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use pi_extensions::steer_bus::{AgentId, SteerPayload, SteerReason, ToSpec};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::thread_engine::BackendNotice;

// ── AgentBus ─────────────────────────────────────────────────────────────

/// A live in-thread subagent coroutine (transient, not a manox Thread).
pub struct LiveSubagent {
    pub handle: HarnessHandle,
    pub def: AgentDef,
    pub parent: AgentId,
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
// Fields are read in Phase D (steer implementation); Phase A is skeleton.
#[allow(dead_code)]
pub struct AgentBus {
    owner_thread_id: String,
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    live_subagents: Mutex<BTreeMap<String, LiveSubagent>>,
    spawned_members: Mutex<HashSet<String>>,
    parent_route: Mutex<Option<ParentRoute>>,
    task_list: Mutex<Option<Arc<Mutex<crate::team::TaskList>>>>,
    captain_handle: Mutex<Option<HarnessHandle>>,
}

impl AgentBus {
    pub fn new(owner_thread_id: String, notice_tx: mpsc::UnboundedSender<BackendNotice>) -> Self {
        Self {
            owner_thread_id,
            notice_tx,
            live_subagents: Mutex::new(BTreeMap::new()),
            spawned_members: Mutex::new(HashSet::new()),
            parent_route: Mutex::new(None),
            task_list: Mutex::new(None),
            captain_handle: Mutex::new(None),
        }
    }

    /// Late-bind the Captain's session handle (session is built after
    /// `build_tools`, so the handle isn't available at construction).
    pub fn bind_captain(&self, handle: HarnessHandle) {
        *self.captain_handle.lock().unwrap() = Some(handle);
    }

    /// Inject parent routing into a member thread's bus.
    pub fn set_parent_route(&self, route: ParentRoute) {
        *self.parent_route.lock().unwrap() = Some(route);
    }

    /// Inject the shared TaskList (Arc-cloned from the parent bus).
    pub fn set_task_list(&self, list: Arc<Mutex<crate::team::TaskList>>) {
        *self.task_list.lock().unwrap() = Some(list);
    }

    /// The main Steer routing entry point.
    /// Phase A: stub — full implementation in Phase D.
    pub async fn steer(
        &self,
        from: AgentId,
        to: ToSpec,
        reason: SteerReason,
        payload: SteerPayload,
    ) -> Result<AgentToolResult, ToolError> {
        let _ = (from, to, reason, payload);
        Err(ToolError::ExecutionFailed(
            "AgentBus::steer not implemented yet (Phase A skeleton)".into(),
        ))
    }
}

// ── SteerTool ────────────────────────────────────────────────────────────

/// The model-facing Steer tool — unified inter-agent messaging + spawn.
/// Holds an `Arc<AgentBus>` + the caller's `AgentId` (the tool is
/// constructed per-thread: `from=Captain` for the Captain's toolset,
/// `from=Subagent(addr)` for a subagent's toolset — the matrix limits
/// the subagent to `Inject` only).
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
        // Parse params.
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

        // Complete is harness-only — reject.
        if reason_str == "Complete" {
            return Err(ToolError::ExecutionFailed(
                "Complete is harness-emitted on termination, not callable as a tool".into(),
            ));
        }

        // Reserved-name check (first).
        if spawn == Some("Captain") || spawn == Some("User") {
            return Err(ToolError::InvalidArguments(
                "cannot spawn User or Captain".into(),
            ));
        }

        // Permission: only Captain may spawn.
        if spawn.is_some() && self.from != AgentId::Captain {
            return Err(ToolError::InvalidArguments(
                "only the Captain may spawn".into(),
            ));
        }

        // TeamMember stub (Phase D wires the BusRequest round-trip).
        if spawn == Some("TeamMember") {
            return Err(ToolError::ExecutionFailed(
                "TeamMember spawn not wired yet".into(),
            ));
        }

        // Build the typed request and delegate to the bus.
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
