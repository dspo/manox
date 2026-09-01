//! Agent-routing vocabulary that outlived the retired `Entity<Team>`
//! roster machinery (deleted — see git history).
//!
//! The live `TeamMember` flow (Steer `spawn="TeamMember"`) creates real
//! threads that coordinate through the Steer bus; what remains here is the
//! shared attribution primitives (`LEADER_NAME`, `author_for`,
//! [`PeerMessage`]) used by the bus's member-message path.
//!
//! Task tools (TaskCreate/TaskList/TaskUpdate/TaskGet) were removed in the
//! tools-optimization cycle — they were retired with the Steer-based team
//! architecture and UpdatePlan provides a strictly better alternative.

// Task tools module removed in the tools-optimization cycle.
// pub mod tools;

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
