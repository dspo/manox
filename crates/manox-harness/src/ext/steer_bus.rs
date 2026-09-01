//! Type definitions for the Steer agent bus — the unified inter-agent
//! messaging primitive. These types live in pi-extensions (the extension
//! layer) so both manox hosts share them without duplicating. The
//! `AgentBus` implementation (routing, spawn, completion) lives in the
//! host (`agent` crate) because it touches host-owned primitives.

use serde::{Deserialize, Serialize};

/// The identity of the agent sending or being addressed by a Steer.
/// `Captain` is per-thread (each thread's root agent); a member thread's
/// root agent is also `Captain` (of that thread). `Subagent` carries the
/// in-thread coroutine's address. Members (real threads) are addressed by
/// their thread id via `ToSpec::agent_address`, not by an `AgentId`
/// variant.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentId {
    User,
    Captain,
    Subagent(String),
}

/// Why a Steer is being sent. `Complete` is harness-only (emitted on
/// subagent termination); the model-facing tool exposes only
/// `{Dispatch, Inject, Abort}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SteerReason {
    Dispatch,
    Complete,
    Inject,
    Abort,
}

/// The target specification: an address (subagent coroutine or thread id)
/// plus an optional spawn request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToSpec {
    pub agent_address: String,
    pub spawn: Option<String>,
    pub isolation: Option<String>,
    /// Wall-clock budget for a spawned subagent coroutine, in milliseconds.
    /// `None` leaves the run unbounded; the bus rejects budgets below its
    /// minimum and budgets attached to non-coroutine spawns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// Idle budget for a spawned subagent coroutine, in milliseconds: the
    /// longest stretch with no child event and no tool in flight before the
    /// run is terminated as stalled. `None` leaves stall detection
    /// report-only. Same minimum and spawn restrictions as `timeout_ms`.
    /// Enforcement granularity is the watchdog tick (~5s), not the exact
    /// millisecond value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout_ms: Option<u64>,
}

/// The armed time budgets of one subagent dispatch, bundled so the bus and
/// its run task thread them as a single value. `None` on an axis leaves the
/// run unbounded there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DispatchBudgets {
    /// Wall-clock budget (ms): the run is terminated at this elapsed time.
    pub timeout_ms: Option<u64>,
    /// Idle budget (ms): the run is terminated as stalled once it goes this
    /// long with no child event and no tool in flight. `None` leaves stall
    /// detection report-only.
    pub idle_timeout_ms: Option<u64>,
}

/// The message payload. v1 carries text only (no images).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteerPayload {
    pub text: String,
}

/// One member operation a `BusRequest` asks the facade (gpui main thread)
/// to execute. The bus (tokio) sends these via `BackendNotice::BusRequest`
/// because `Entity<Thread>` creation, `thread.update` injection, and
/// thread-level abort are all pure-gpui operations.
#[derive(Debug, Clone)]
pub enum BusOp {
    SpawnMember { name: String, prompt: String },
    InjectMember { thread_id: String, payload: String },
    AbortMember { thread_id: String },
}
