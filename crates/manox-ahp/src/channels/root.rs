//! The root channel (`ahp-root://`): agent catalogue, terminal catalogue, host
//! configuration — the state every client needs before it can talk to anything.

use ahp_types::state::{RootState, TerminalInfo};

/// The root channel URI (literal, per spec).
pub const URI: &str = "ahp-root://";

/// An empty root state — for tests and for a backend that serves no agents yet.
pub fn empty() -> RootState {
    RootState {
        agents: Vec::new(),
        active_sessions: None,
        terminals: None,
        config: None,
        meta: None,
    }
}

/// A root state with an agent catalogue and, optionally, a terminal catalogue.
pub fn with_agents(
    agents: Vec<ahp_types::state::AgentInfo>,
    terminals: Option<Vec<TerminalInfo>>,
) -> RootState {
    RootState {
        agents,
        active_sessions: None,
        terminals,
        config: None,
        meta: Some(crate::ext::declaration_meta()),
    }
}
