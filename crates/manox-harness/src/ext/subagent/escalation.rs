//! The per-run child authority seam: one hook the host installs on the
//! spawn provider to re-arm the inherited enforcing tools (Bash and the fs
//! fence) for EVERY child run with that run's own escalation state.
//!
//! The child snapshot is assembled once per parent session and shared by
//! every concurrent child, so a grant cell or an approved-roots store living
//! on the snapshot would let one child's approved escalation widen another
//! child's calls (or the parent's own). The seam exists so the per-run state
//! attaches at the start boundary — where the run identity is known — and
//! dies with the run's tools.
//!
//! A host without a parent approval channel installs nothing: the snapshot
//! runs as-is and every escalation request fails closed at the delegation
//! boundary gate.

use std::sync::Arc;

use crate::core::tool::AgentTool;

/// Re-arms one child run's enforcing tools with that run's isolated
/// escalation state (grant cell, approver, approved-roots overlay). The
/// returned set replaces the tool selection for this run only; the shared
/// snapshot the provider holds is never mutated.
pub trait ChildAuthority: Send + Sync {
    /// `run_id` is the run (and child session) id; `kind`/`label` are the
    /// definition name and the dispatch label — the attribution a card on
    /// the parent session carries.
    fn arm(
        &self,
        run_id: &str,
        kind: &str,
        label: &str,
        tools: Vec<Arc<dyn AgentTool>>,
    ) -> Vec<Arc<dyn AgentTool>>;
}
