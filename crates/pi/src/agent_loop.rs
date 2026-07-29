// Agent loop — the pure dual-loop state machine.
//
// This is the heart of the harness. It takes a stream function, a context,
// and a config, and drives the conversation through turns of LLM streaming
// and tool execution until the agent naturally stops or is aborted.

use tokio_util::sync::CancellationToken;
use crate::types::{AgentMessage, AgentEvent, AgentContext, AgentLoopConfig};

/// Run the agent loop to completion.
///
/// Returns all new messages produced during this run. The `context.messages`
/// is mutated in-place to include the new messages.
pub async fn run_loop(
    context: &mut AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    config: &AgentLoopConfig,
    _signal: Option<CancellationToken>,
    _stream_fn: &dyn StreamFn,
) -> Vec<AgentMessage> {
    // Placeholder — full implementation in Phase 2.
    let _ = (context, new_messages, config);
    Vec::new()
}

/// The function that streams an assistant response from an LLM.
///
/// Takes a context and returns a stream of events. The harness doesn't know
/// or care which provider is on the other end.
#[async_trait::async_trait]
pub trait StreamFn: Send + Sync {
    async fn stream(
        &self,
        context: &AgentContext,
        _signal: CancellationToken,
    ) -> Result<Vec<AgentEvent>, anyhow::Error>;
}

/// Emit an event to all registered listeners.
pub trait EventSink: Send {
    fn emit(&self, event: AgentEvent);
}