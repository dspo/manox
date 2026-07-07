//! Snapshot types returned by the debug Harness. Serialized to JSON and handed
//! back to MCP tool callers (an external agent) or asserted on in `cargo test`.

use serde::Serialize;

/// A persisted thread's display metadata, mirrored from `ThreadSummary`.
#[derive(Serialize, Clone)]
pub struct ThreadInfo {
    pub id: String,
    pub title: String,
}

/// A canonical `Message` reduced to its role + flattened text, the shape an
/// agent caller inspects to verify conversation state.
#[derive(Serialize, Clone)]
pub struct MessageSnapshot {
    pub role: String,
    pub text: String,
}

/// Outcome of awaiting a thread's idle state.
#[derive(Serialize, Clone, Copy, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum IdleState {
    /// The thread is no longer running a turn.
    Idle,
    /// The deadline elapsed while the thread was still running.
    StillRunning,
}
