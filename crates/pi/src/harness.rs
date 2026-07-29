// AgentHarness — orchestration layer.
//
// Wraps the agent loop with session persistence, hooks, compaction
// integration, and phase management. This is the primary public API
// for consumers of the harness.

use crate::session::{Session, SessionStorage};
use crate::types::{AgentMessage, Model};

/// The phases the harness can be in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentHarnessPhase {
    /// No active operation.
    Idle,
    /// Processing a user turn.
    Turn,
    /// Running compaction.
    Compaction,
    /// Generating a branch summary.
    BranchSummary,
    /// Retrying a failed operation.
    Retry,
}

/// The orchestration layer wrapping the agent loop.
pub struct AgentHarness<S: SessionStorage> {
    _session: Session<S>,
    model: Model,
    phase: AgentHarnessPhase,
}

impl<S: SessionStorage> AgentHarness<S> {
    pub fn new(session: Session<S>, model: Model) -> Self {
        AgentHarness {
            _session: session,
            model,
            phase: AgentHarnessPhase::Idle,
        }
    }

    pub fn phase(&self) -> AgentHarnessPhase {
        self.phase
    }

    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Send a user prompt and run the agent loop.
    pub async fn prompt(
        &mut self,
        _text: &str,
    ) -> Result<AgentMessage, anyhow::Error> {
        // Placeholder — full implementation in Phase 4.
        anyhow::bail!("not yet implemented")
    }
}