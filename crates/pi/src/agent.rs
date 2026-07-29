// Agent — stateful wrapper around the agent loop.
//
// Owns the conversation transcript, manages event subscriptions, and exposes
// queueing APIs for steering and follow-up messages. The Agent wraps the raw
// `run_loop` / `run_loop_continue` functions with lifecycle management.

use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{run_loop, run_loop_continue, StreamFn, EventSink};
use crate::types::{AgentState, AgentMessage, AgentEvent, AgentContext, AgentLoopConfig, Model, ContentBlock};

/// Controls how queued messages are drained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueMode {
    /// Drain all queued messages at once.
    All,
    /// Drain one message at a time.
    OneAtATime,
}

/// Pending message queue with a drain mode.
struct PendingMessageQueue {
    messages: Vec<AgentMessage>,
    mode: QueueMode,
}

impl PendingMessageQueue {
    fn new(mode: QueueMode) -> Self {
        PendingMessageQueue {
            messages: Vec::new(),
            mode,
        }
    }

    fn enqueue(&mut self, message: AgentMessage) {
        self.messages.push(message);
    }

    fn has_items(&self) -> bool {
        !self.messages.is_empty()
    }

    fn drain(&mut self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => {
                let drained = self.messages.clone();
                self.messages.clear();
                drained
            }
            QueueMode::OneAtATime => {
                if self.messages.is_empty() {
                    return Vec::new();
                }
                vec![self.messages.remove(0)]
            }
        }
    }

    fn clear(&mut self) {
        self.messages.clear();
    }
}

/// A sink that collects events and forwards them to subscribers.
struct SubscriberSink {
    events: Arc<Mutex<Vec<AgentEvent>>>,
}

impl SubscriberSink {
    fn new() -> Self {
        SubscriberSink {
            events: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn drain(&self) -> Vec<AgentEvent> {
        let mut events = self.events.lock().unwrap();
        std::mem::take(&mut *events)
    }
}

impl EventSink for SubscriberSink {
    fn emit(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// The Agent wraps the raw agent loop with state management, event
/// subscription, and message queuing (steering / follow-up).
pub struct Agent {
    state: AgentState,
    steering_queue: PendingMessageQueue,
    follow_up_queue: PendingMessageQueue,
    active_run: Option<CancellationToken>,
    stream_fn: Arc<dyn StreamFn>,
    sink: SubscriberSink,
}

impl Agent {
    /// Create a new agent with the given system prompt and model.
    pub fn new(
        system_prompt: impl Into<String>,
        model: Model,
        stream_fn: Arc<dyn StreamFn>,
    ) -> Self {
        Agent {
            state: AgentState::new(system_prompt, model),
            steering_queue: PendingMessageQueue::new(QueueMode::OneAtATime),
            follow_up_queue: PendingMessageQueue::new(QueueMode::OneAtATime),
            active_run: None,
            stream_fn,
            sink: SubscriberSink::new(),
        }
    }

    /// Current agent state.
    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Drain all events that have been emitted since the last drain.
    pub fn drain_events(&self) -> Vec<AgentEvent> {
        self.sink.drain()
    }

    /// Queue a message to be injected after the current assistant turn finishes.
    pub fn steer(&mut self, message: AgentMessage) {
        self.steering_queue.enqueue(message);
    }

    /// Queue a message to run only after the agent would otherwise stop.
    pub fn follow_up(&mut self, message: AgentMessage) {
        self.follow_up_queue.enqueue(message);
    }

    /// Remove all queued steering messages.
    pub fn clear_steering_queue(&mut self) {
        self.steering_queue.clear();
    }

    /// Remove all queued follow-up messages.
    pub fn clear_follow_up_queue(&mut self) {
        self.follow_up_queue.clear();
    }

    /// Whether either queue has pending messages.
    pub fn has_queued_messages(&self) -> bool {
        self.steering_queue.has_items() || self.follow_up_queue.has_items()
    }

    /// Abort the current run, if one is active.
    pub fn abort(&mut self) {
        if let Some(token) = self.active_run.take() {
            token.cancel();
        }
    }

    /// Reset the agent's transcript and queues.
    pub fn reset(&mut self) {
        self.state.messages.clear();
        self.state.is_streaming = false;
        self.state.streaming_message = None;
        self.state.pending_tool_calls.clear();
        self.state.error_message = None;
        self.steering_queue.clear();
        self.follow_up_queue.clear();
    }

    /// Start a new prompt from text.
    pub async fn prompt(&mut self, text: &str) -> Result<Vec<AgentMessage>, anyhow::Error> {
        if self.active_run.is_some() {
            anyhow::bail!("Agent is already processing a prompt.");
        }

        let content = vec![ContentBlock::Text {
            text: text.to_string(),
        }];
        let user_message = AgentMessage::User {
            content,
            timestamp: chrono::Utc::now(),
        };
        self.run_prompt_messages(&[user_message]).await
    }

    /// Continue from the current transcript.
    pub async fn continue_(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        if self.active_run.is_some() {
            anyhow::bail!("Agent is already processing.");
        }

        let last = self.state.messages.last().cloned();
        match last {
            None => anyhow::bail!("No messages to continue from"),
            Some(AgentMessage::Assistant { .. }) => {
                // Try draining steering/follow-up queues first.
                let steering = self.steering_queue.drain();
                if !steering.is_empty() {
                    return self.run_prompt_messages(&steering).await;
                }
                let follow_up = self.follow_up_queue.drain();
                if !follow_up.is_empty() {
                    return self.run_prompt_messages(&follow_up).await;
                }
                anyhow::bail!("Cannot continue from message role: assistant");
            }
            Some(_) => {
                self.run_continuation().await
            }
        }
    }

    /// Build the current context snapshot for the loop.
    fn create_context_snapshot(&self) -> AgentContext {
        AgentContext {
            system_prompt: self.state.system_prompt.clone(),
            messages: self.state.messages.clone(),
            tools: Vec::new(), // tools are set externally
            model: self.state.model.clone(),
            thinking_level: self.state.thinking_level.clone(),
            metadata: Default::default(),
        }
    }

    /// Build the loop config from the current agent state.
    fn create_loop_config(&self) -> AgentLoopConfig {
        AgentLoopConfig {
            get_steering_messages: None,
            get_follow_up_messages: None,
            prepare_next_turn: None,
            should_stop_after_turn: None,
            before_tool_call: None,
            after_tool_call: None,
            sequential_tool_execution: false,
            max_turns: None,
        }
    }

    async fn run_prompt_messages(
        &mut self,
        messages: &[AgentMessage],
    ) -> Result<Vec<AgentMessage>, anyhow::Error> {
        let owned_messages = messages.to_vec();
        let (messages, context) = self.run_with_lifecycle(|signal, agent| {
            let mut context = agent.create_context_snapshot();
            let config = agent.create_loop_config();
            let stream_fn = Arc::clone(&agent.stream_fn);
            let sink = agent.sink.clone();
            let msgs = owned_messages.clone();

            Box::pin(async move {
                let msgs = run_loop(&msgs, &mut context, &config, Some(signal), stream_fn.as_ref(), &sink).await?;
                Ok((msgs, context))
            })
        }).await?;
        self.state.messages = context.messages;
        Ok(messages)
    }

    async fn run_continuation(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        let (messages, context) = self.run_with_lifecycle(|signal, agent| {
            let mut context = agent.create_context_snapshot();
            let config = agent.create_loop_config();
            let stream_fn = Arc::clone(&agent.stream_fn);
            let sink = agent.sink.clone();

            Box::pin(async move {
                let msgs = run_loop_continue(&mut context, &config, Some(signal), stream_fn.as_ref(), &sink).await?;
                Ok((msgs, context))
            })
        }).await?;
        self.state.messages = context.messages;
        Ok(messages)
    }

    async fn run_with_lifecycle<F>(
        &mut self,
        executor: F,
    ) -> Result<(Vec<AgentMessage>, AgentContext), anyhow::Error>
    where
        F: for<'a> FnOnce(CancellationToken, &'a mut Agent) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(Vec<AgentMessage>, AgentContext), anyhow::Error>> + 'a>>,
    {
        if self.active_run.is_some() {
            anyhow::bail!("Agent is already processing.");
        }

        let token = CancellationToken::new();
        self.active_run = Some(token.clone());
        self.state.is_streaming = true;
        self.state.streaming_message = None;
        self.state.error_message = None;

        let result = executor(token.clone(), self).await;

        self.state.is_streaming = false;
        self.state.streaming_message = None;
        self.state.error_message = None;
        self.active_run = None;

        result
    }
}

impl Clone for SubscriberSink {
    fn clone(&self) -> Self {
        SubscriberSink {
            events: Arc::clone(&self.events),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{StopReason, Usage};

    struct TestStreamFn;

    #[async_trait::async_trait]
    impl StreamFn for TestStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            _on_event: &(dyn Fn(AgentEvent) + Send + Sync),
        ) -> Result<AgentMessage, anyhow::Error> {
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "Test response".into(),
                }],
                model: "test".into(),
                provider: "test".into(),
                stop_reason: Some(StopReason::EndTurn),
                usage: Usage::default(),
                timestamp: chrono::Utc::now(),
            })
        }
    }

    fn test_model() -> Model {
        Model {
            provider: "test".into(),
            id: "test".into(),
            context_window: 100_000,
            supports_thinking: false,
            metadata: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_agent_prompt() {
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        let result = agent.prompt("Hello").await;
        assert!(result.is_ok());
        let messages = result.unwrap();
        assert!(!messages.is_empty());
        assert_eq!(agent.state().messages.len(), 2); // user + assistant
    }

    #[tokio::test]
    async fn test_agent_abort() {
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        agent.abort();
        // Should not panic.
    }

    #[tokio::test]
    async fn test_agent_reset() {
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        let _ = agent.prompt("Hello").await;
        agent.reset();
        assert!(agent.state().messages.is_empty());
        assert!(!agent.has_queued_messages());
    }

    #[tokio::test]
    async fn test_agent_steer() {
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        agent.steer(AgentMessage::user("steering message"));
        assert!(agent.has_queued_messages());
        agent.clear_steering_queue();
        assert!(!agent.has_queued_messages());
    }
}