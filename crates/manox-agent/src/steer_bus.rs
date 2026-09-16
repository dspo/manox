//! Host-side `AgentBus` + `Steer` tool — the TeamMember messaging channel.
//! Routes `Steer` messages between the facade, the singleton `Captain`, and
//! spawned member threads (real manox Threads). In-thread subagent
//! delegation moved to the dsh-isomorphic `subagent` module (delegation
//! tools + runtime + provider); the bus retains only the member operations,
//! which are pure gpui facade round trips (`BusOp`). Sits on the kernel's
//! intra-session `steer` (`HarnessHandle::steer`). TS Pi has no cross-session
//! agent bus — this is a manox host extension.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, Weak};

use manox_harness::steer_bus::{AgentId, BusOp, SteerPayload, SteerReason, ToSpec};
use manox_harness::tool::{AgentTool, AgentToolResult, ToolError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::thread_engine::BackendNotice;

// ── AgentBus ─────────────────────────────────────────────────────────────

/// The host-side agent bus. One per thread (Captain or member). Tracks the
/// member thread ids this thread spawned so Inject/Abort/cancel-fan-out can
/// address them.
pub struct AgentBus {
    pub(crate) owner_thread_id: String,
    pub(crate) notice_tx: mpsc::UnboundedSender<BackendNotice>,
    spawned_members: Mutex<HashSet<String>>,
    weak_self: Mutex<Weak<AgentBus>>,
}

/// Poison-tolerant lock: a holder that panicked must not take every later
/// dispatch down with a secondary panic.
fn locked<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
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
            spawned_members: Mutex::new(HashSet::new()),
            weak_self: Mutex::new(Weak::new()),
        });
        *bus.weak_self.lock().unwrap() = Arc::downgrade(&bus);
        bus
    }

    pub fn owner_thread_id(&self) -> &str {
        &self.owner_thread_id
    }

    /// The main Steer routing entry point. Subagent addresses are unknown
    /// here — the delegation tools own that surface; every non-member arm
    /// falls through to the permission-denied reply.
    pub async fn steer(
        &self,
        from: AgentId,
        to: ToSpec,
        reason: SteerReason,
        payload: SteerPayload,
    ) -> Result<AgentToolResult, ToolError> {
        let addr = &to.agent_address;
        match (&from, &reason) {
            // ── Dispatch: spawn TeamMember (real thread) ───────────────
            (AgentId::Captain, SteerReason::Dispatch)
                if to.spawn.as_deref() == Some("TeamMember") =>
            {
                self.dispatch_member(addr, &payload.text).await
            }

            // ── Inject to member thread ─────────────────────────────────
            (AgentId::Captain, SteerReason::Inject) if self.is_member(addr) => self
                .bus_request(BusOp::InjectMember {
                    thread_id: addr.to_string(),
                    payload: payload.text.clone(),
                })
                .await
                .map(|_| ack(addr, false, None)),

            // ── Abort member thread ─────────────────────────────────────
            (AgentId::Captain, SteerReason::Abort) if self.is_member(addr) => self
                .bus_request(BusOp::AbortMember {
                    thread_id: addr.to_string(),
                })
                .await
                .map(|_| ack(addr, false, None)),

            // ── Permission denied ───────────────────────────────────────
            _ => Err(ToolError::InvalidArguments(format!(
                "steer not allowed: from={from:?} to={addr} reason={reason:?} — subagent \
                 delegation uses its own tools; Steer only reaches TeamMember threads"
            ))),
        }
    }

    /// Check if `addr` is a known member thread id.
    fn is_member(&self, addr: &str) -> bool {
        locked(&self.spawned_members).contains(addr)
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
        locked(&self.spawned_members).insert(result.clone());
        Ok(ack(name, true, Some(result)))
    }

    /// Send a BusRequest to the facade (gpui main thread) and await reply.
    async fn bus_request(&self, op: BusOp) -> Result<String, ToolError> {
        let (tx, rx) = async_channel::bounded(1);
        self.notice_tx
            .send(BackendNotice::BusRequest {
                op,
                responder: Some(tx),
            })
            .map_err(|_| ToolError::ExecutionFailed("engine actor gone".into()))?;
        rx.recv()
            .await
            .map_err(|_| ToolError::ExecutionFailed("bus request dropped".into()))?
            .map_err(ToolError::ExecutionFailed)
    }

    /// Fire-and-forget user-cancel fan-out: one `BusOp::AbortMember` per
    /// spawned member thread. The replies are discarded — a cancel must
    /// never block the gpui thread on member round trips, and each member's
    /// facade-side handler recurses into that member's own derivatives.
    pub fn abort_all_members(&self) {
        let members: Vec<String> = locked(&self.spawned_members).iter().cloned().collect();
        for thread_id in members {
            let _ = self.notice_tx.send(BackendNotice::BusRequest {
                op: BusOp::AbortMember { thread_id },
                responder: None,
            });
        }
    }

    /// Test-only: register a member id as spawned (production inserts via
    /// the `dispatch_member` facade round trip).
    #[cfg(any(test, feature = "test-support"))]
    pub fn register_spawned_member_for_test(&self, thread_id: &str) {
        locked(&self.spawned_members).insert(thread_id.to_string());
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

// ── SteerTool ────────────────────────────────────────────────────────────

/// The model-facing Steer tool — TeamMember messaging + spawn. In-thread
/// subagent delegation lives in the per-definition delegation tools.
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
        "Message a TeamMember — a real manox Thread running beside this one. \
         Set `to.spawn` to \"TeamMember\" with `reason: \"Dispatch\"` to \
         create one (only the Captain may spawn; the reply carries its \
         thread id), `reason: \"Inject\"` to send it a message mid-run, or \
         `reason: \"Abort\"` to cancel it. In-thread subagent delegation \
         does not go through Steer — use its delegation tools."
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
                            "description": "Target address. For a spawn: the caller-chosen member name. For Inject/Abort: the system thread id (returned in the spawn ack). Reserved: 'Captain', 'User'."
                        },
                        "spawn": {
                            "type": "string",
                            "enum": ["TeamMember"],
                            "description": "Optional. \"TeamMember\" creates a real manox Thread (process: persisted, sidebar-visible, resumable, own Captain session + own bus). Only the Captain may set. Error if address exists."
                        }
                    },
                    "required": ["agent_address"]
                },
                "reason": {
                    "type": "string",
                    "enum": ["Dispatch", "Inject", "Abort"],
                    "description": "Dispatch starts a TeamMember, Inject messages an existing one, Abort cancels it."
                },
                "prompt": {
                    "type": "string",
                    "description": "Payload: task (Dispatch), message (Inject). Abort ignores it."
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
        _ctx: &dyn manox_harness::tool::ToolContext,
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

        if reason_str == "Complete" {
            return Err(ToolError::ExecutionFailed(
                "Complete is harness-emitted on termination, not callable as a tool".into(),
            ));
        }
        if spawn.is_some_and(|s| s != "TeamMember") {
            return Err(ToolError::InvalidArguments(
                "Steer only spawns TeamMember threads; subagent delegation uses its own tools"
                    .into(),
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
        }
    }

    fn payload(text: &str) -> SteerPayload {
        SteerPayload { text: text.into() }
    }

    #[tokio::test]
    async fn subagent_cannot_spawn() {
        let bus = bus();
        let err = bus
            .steer(
                AgentId::Subagent("w1".into()),
                to("m", Some("TeamMember")),
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
        assert!(err.to_string().contains("not allowed"), "{err}");
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

    /// The retired subagent spawn path (a non-TeamMember spawn name) now
    /// falls through to permission-denied: delegation tools own that
    /// surface, the bus only reaches member threads.
    #[tokio::test]
    async fn non_member_spawn_denied() {
        let bus = bus();
        let err = bus
            .steer(
                AgentId::Captain,
                to("w", Some("Sailor")),
                SteerReason::Dispatch,
                payload("x"),
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("delegation"),
            "the denial points at the delegation tools: {err}"
        );
    }

    /// The user-cancel fan-out fires one `AbortMember` per spawned member
    /// (fire-and-forget: replies discarded), and nothing else.
    #[test]
    fn abort_all_members_fires_one_abort_per_spawned_member() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = AgentBus::new("thread-1".into(), tx);
        bus.register_spawned_member_for_test("m1");
        bus.register_spawned_member_for_test("m2");
        bus.register_spawned_member_for_test("m3");

        bus.abort_all_members();

        let mut aborted: Vec<String> = Vec::new();
        while let Ok(notice) = rx.try_recv() {
            match notice {
                BackendNotice::BusRequest {
                    op: BusOp::AbortMember { thread_id },
                    ..
                } => aborted.push(thread_id),
                _ => panic!("unexpected non-AbortMember notice"),
            }
        }
        aborted.sort();
        assert_eq!(aborted, vec!["m1", "m2", "m3"]);
    }
}
