//! Agent-routing vocabulary shared by the Steer bus.
//!
//! The `TeamMember` flow (Steer `spawn="TeamMember"`) creates real threads
//! that coordinate through the Steer bus; this module holds the shared
//! attribution primitives (`LEADER_NAME`, `author_for`, [`PeerMessage`]) used
//! by the bus's member-message path. Task state is the model's plan
//! (`UpdatePlan`), not a separate task tool.

use crate::message::MessageAuthor;

/// The leader's member name (matches the main thread's `agent_label`).
pub const LEADER_NAME: &str = "lead";

/// Attribution for a routing name: the leader resolves to the main agent,
/// every other name is a named agent (thread / manifest definition).
pub fn author_for(name: &str) -> MessageAuthor {
    MessageAuthor::from_routing(name)
}

/// A peer-to-peer message between agents, delivered over the Steer bus.
#[derive(Debug, Clone)]
pub struct PeerMessage {
    pub from: String,
    pub content: String,
}
