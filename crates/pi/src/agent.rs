// Agent — stateful wrapper around the agent loop.
//
// Owns the conversation transcript, manages event subscriptions, and exposes
// queueing APIs for steering and follow-up messages.

use tokio_util::sync::CancellationToken;
use crate::types::{AgentState, AgentMessage, Model};

/// The Agent wraps the raw agent loop with state management, event
/// subscription, and message queuing.
pub struct Agent {
    state: AgentState,
    steering_queue: Vec<AgentMessage>,
    follow_up_queue: Vec<AgentMessage>,
    active_run: Option<CancellationToken>,
}

impl Agent {
    pub fn new(system_prompt: impl Into<String>, model: Model) -> Self {
        Agent {
            state: AgentState::new(system_prompt, model),
            steering_queue: Vec::new(),
            follow_up_queue: Vec::new(),
            active_run: None,
        }
    }

    pub fn state(&self) -> &AgentState {
        &self.state
    }

    pub fn steer(&mut self, _message: AgentMessage) {
        // Placeholder — full implementation in Phase 2.
    }

    pub fn follow_up(&mut self, _message: AgentMessage) {
        // Placeholder — full implementation in Phase 2.
    }

    pub fn abort(&mut self) {
        if let Some(token) = self.active_run.take() {
            token.cancel();
        }
    }

    pub fn reset(&mut self) {
        self.state.messages.clear();
        self.steering_queue.clear();
        self.follow_up_queue.clear();
    }

    pub async fn prompt(&mut self, _text: &str) -> Result<Vec<AgentMessage>, anyhow::Error> {
        // Placeholder — full implementation in Phase 2.
        Ok(Vec::new())
    }
}