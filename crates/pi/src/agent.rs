// Agent — stateful wrapper around the agent loop.
//
// Owns the conversation transcript, manages event subscriptions, and exposes
// queueing APIs for steering and follow-up messages. The Agent wraps the raw
// `run_loop` / `run_loop_continue` functions with lifecycle management.

use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{EventSink, StreamFn, run_loop, run_loop_continue};
use crate::tool::{AgentToolResult, ToolContext};
use crate::types::{
    AfterToolCallFn, AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentState,
    BeforeProviderRequestFn, BeforeToolCallFn, CacheRetention, ContentBlock, Model,
};
use serde_json::Value as JsonValue;

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

/// Per-run observation hooks cloned into each turn's `AgentLoopConfig`.
///
/// Held as `Arc<dyn Fn>` so `create_loop_config` can produce a fresh `Box`
/// closure per run without owning the (un-`Clone`) originals. The harness
/// fills these from its registered `HookPoint`s.
pub type BeforeProviderRequestHook = Arc<dyn Fn(&AgentContext) + Send + Sync>;
pub type BeforeToolCallHook = Arc<dyn Fn(&str, &str, &JsonValue) -> Option<String> + Send + Sync>;
pub type AfterToolCallHook = Arc<dyn Fn(&AgentToolResult) -> AgentToolResult + Send + Sync>;

#[derive(Default)]
pub struct LoopHooks {
    pub before_provider_request: Option<BeforeProviderRequestHook>,
    pub before_tool_call: Option<BeforeToolCallHook>,
    pub after_tool_call: Option<AfterToolCallHook>,
}

/// The Agent wraps the raw agent loop with state management, event
/// subscription, and message queuing (steering / follow-up).
pub struct Agent {
    state: AgentState,
    /// Mid-turn steering queue, drained by the loop's `get_steering_messages`
    /// callback. Shared via `Arc<Mutex<..>>` so the closure cloned into the
    /// loop config can drain it from within the spawned run while the Agent
    /// still receives `steer()` calls from the outside.
    steering_queue: Arc<Mutex<PendingMessageQueue>>,
    /// Post-stop follow-up queue, drained by the loop's
    /// `get_follow_up_messages` callback to resume a run that would otherwise
    /// have ended.
    follow_up_queue: Arc<Mutex<PendingMessageQueue>>,
    active_run: Option<CancellationToken>,
    stream_fn: Arc<dyn StreamFn>,
    sink: SubscriberSink,
    /// Tools mounted on the agent and forwarded into each turn's context.
    tools: Arc<[Box<dyn crate::tool::AgentTool>]>,
    /// Session-scoped execution context for tool calls. Backs the real
    /// `ToolContext` (env + cwd + tool state) so tools reach the filesystem
    /// and shell instead of panicking.
    tool_ctx: Arc<dyn ToolContext>,
    /// Session identifier forwarded to providers that support session-based
    /// caching (`prompt_cache_key`).
    session_id: Option<String>,
    /// Prompt cache retention preference forwarded to providers.
    cache_retention: CacheRetention,
    /// Observation hooks forwarded into each turn's loop config. The harness
    /// fills these so its registered `HookPoint`s fire inside the loop.
    loop_hooks: LoopHooks,
}

impl Agent {
    /// Create a new agent with the given system prompt and model.
    ///
    /// `tool_ctx` backs all tool execution; pass a real `ToolContext`
    /// (e.g. `LocalToolContext`) so fs/shell tools work instead of panicking.
    pub fn new(
        system_prompt: impl Into<String>,
        model: Model,
        stream_fn: Arc<dyn StreamFn>,
        tool_ctx: Arc<dyn ToolContext>,
    ) -> Self {
        Agent {
            state: AgentState::new(system_prompt, model),
            steering_queue: Arc::new(Mutex::new(PendingMessageQueue::new(QueueMode::OneAtATime))),
            follow_up_queue: Arc::new(Mutex::new(PendingMessageQueue::new(QueueMode::OneAtATime))),
            active_run: None,
            stream_fn,
            sink: SubscriberSink::new(),
            tools: Arc::from(Vec::new()),
            tool_ctx,
            session_id: None,
            cache_retention: CacheRetention::default(),
            loop_hooks: LoopHooks::default(),
        }
    }

    /// Mount tools on the agent. They are forwarded into each turn's context
    /// so the provider sees them and `execute_tool_calls` can dispatch.
    pub fn with_tools(mut self, tools: Arc<[Box<dyn crate::tool::AgentTool>]>) -> Self {
        self.tools = tools;
        self
    }

    /// Replace the mounted tools.
    pub fn set_tools(&mut self, tools: Arc<[Box<dyn crate::tool::AgentTool>]>) {
        self.tools = tools;
    }

    /// Set the session identifier forwarded to providers for cache-aware
    /// backends.
    pub fn set_session_id(&mut self, session_id: Option<String>) {
        self.session_id = session_id;
    }

    /// Set the prompt cache retention preference forwarded to providers.
    pub fn set_cache_retention(&mut self, retention: CacheRetention) {
        self.cache_retention = retention;
    }

    /// Set the per-run observation hooks forwarded into the loop config.
    pub fn set_loop_hooks(&mut self, hooks: LoopHooks) {
        self.loop_hooks = hooks;
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
        self.steering_queue.lock().unwrap().enqueue(message);
    }

    /// Queue a message to run only after the agent would otherwise stop.
    pub fn follow_up(&mut self, message: AgentMessage) {
        self.follow_up_queue.lock().unwrap().enqueue(message);
    }

    /// Remove all queued steering messages.
    pub fn clear_steering_queue(&mut self) {
        self.steering_queue.lock().unwrap().clear();
    }

    /// Remove all queued follow-up messages.
    pub fn clear_follow_up_queue(&mut self) {
        self.follow_up_queue.lock().unwrap().clear();
    }

    /// Whether either queue has pending messages.
    pub fn has_queued_messages(&self) -> bool {
        self.steering_queue.lock().unwrap().has_items()
            || self.follow_up_queue.lock().unwrap().has_items()
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
        self.steering_queue.lock().unwrap().clear();
        self.follow_up_queue.lock().unwrap().clear();
    }

    /// Replace the entire transcript with new messages.
    ///
    /// Used by the harness during compaction to swap in the compacted
    /// conversation.
    pub fn replace_transcript(&mut self, messages: Vec<AgentMessage>) {
        self.state.messages = messages;
    }

    /// Start a new prompt from text.
    pub async fn prompt(&mut self, text: &str) -> Result<Vec<AgentMessage>, anyhow::Error> {
        if self.active_run.is_some() {
            anyhow::bail!("Agent is already processing a prompt.");
        }

        let content = vec![ContentBlock::Text {
            text: text.to_string(),
            signature: None,
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
                let steering = self.steering_queue.lock().unwrap().drain();
                if !steering.is_empty() {
                    return self.run_prompt_messages(&steering).await;
                }
                let follow_up = self.follow_up_queue.lock().unwrap().drain();
                if !follow_up.is_empty() {
                    return self.run_prompt_messages(&follow_up).await;
                }
                anyhow::bail!("Cannot continue from message role: assistant");
            }
            Some(_) => self.run_continuation().await,
        }
    }

    /// Build the current context snapshot for the loop.
    fn create_context_snapshot(&self) -> AgentContext {
        AgentContext {
            system_prompt: self.state.system_prompt.clone(),
            messages: self.state.messages.clone(),
            tools: Arc::clone(&self.tools),
            model: self.state.model.clone(),
            thinking_level: self.state.thinking_level.clone(),
            cache_retention: self.cache_retention,
            session_id: self.session_id.clone(),
            metadata: Default::default(),
        }
    }

    /// Build the loop config from the current agent state.
    ///
    /// The steering and follow-up queues are handed to the loop as draining
    /// callbacks so messages queued mid-run (via `steer`) or after a natural
    /// stop (via `follow_up`) are injected by the loop itself rather than
    /// requiring the caller to manually resume.
    fn create_loop_config(&self) -> AgentLoopConfig {
        let steering = Arc::clone(&self.steering_queue);
        let follow_up = Arc::clone(&self.follow_up_queue);
        let before_provider = self.loop_hooks.before_provider_request.as_ref().map(|h| {
            let h = Arc::clone(h);
            Box::new(move |ctx: &AgentContext| h(ctx)) as BeforeProviderRequestFn
        });
        let before_tool = self.loop_hooks.before_tool_call.as_ref().map(|h| {
            let h = Arc::clone(h);
            Box::new(move |id: &str, name: &str, args: &JsonValue| h(id, name, args))
                as BeforeToolCallFn
        });
        let after_tool = self.loop_hooks.after_tool_call.as_ref().map(|h| {
            let h = Arc::clone(h);
            Box::new(move |r: &AgentToolResult| h(r)) as AfterToolCallFn
        });
        AgentLoopConfig {
            get_steering_messages: Some(Box::new(move || steering.lock().unwrap().drain())),
            get_follow_up_messages: Some(Box::new(move || follow_up.lock().unwrap().drain())),
            prepare_next_turn: None,
            should_stop_after_turn: None,
            before_tool_call: before_tool,
            after_tool_call: after_tool,
            before_provider_request: before_provider,
            sequential_tool_execution: false,
            max_turns: None,
        }
    }

    async fn run_prompt_messages(
        &mut self,
        messages: &[AgentMessage],
    ) -> Result<Vec<AgentMessage>, anyhow::Error> {
        let owned_messages = messages.to_vec();
        let (messages, context) = self
            .run_with_lifecycle(|signal, agent| {
                let mut context = agent.create_context_snapshot();
                let config = agent.create_loop_config();
                let stream_fn = Arc::clone(&agent.stream_fn);
                let tool_ctx = Arc::clone(&agent.tool_ctx);
                let sink = agent.sink.clone();
                let msgs = owned_messages.clone();

                Box::pin(async move {
                    let msgs = run_loop(
                        &msgs,
                        &mut context,
                        &config,
                        Some(signal),
                        stream_fn,
                        &*tool_ctx,
                        &sink,
                    )
                    .await?;
                    Ok((msgs, context))
                })
            })
            .await?;
        self.state.messages = context.messages;
        Ok(messages)
    }

    async fn run_continuation(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        let (messages, context) = self
            .run_with_lifecycle(|signal, agent| {
                let mut context = agent.create_context_snapshot();
                let config = agent.create_loop_config();
                let stream_fn = Arc::clone(&agent.stream_fn);
                let tool_ctx = Arc::clone(&agent.tool_ctx);
                let sink = agent.sink.clone();

                Box::pin(async move {
                    let msgs = run_loop_continue(
                        &mut context,
                        &config,
                        Some(signal),
                        stream_fn,
                        &*tool_ctx,
                        &sink,
                    )
                    .await?;
                    Ok((msgs, context))
                })
            })
            .await?;
        self.state.messages = context.messages;
        Ok(messages)
    }

    async fn run_with_lifecycle<F>(
        &mut self,
        executor: F,
    ) -> Result<(Vec<AgentMessage>, AgentContext), anyhow::Error>
    where
        F: for<'a> FnOnce(
            CancellationToken,
            &'a mut Agent,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<(Vec<AgentMessage>, AgentContext), anyhow::Error>,
                    > + 'a,
            >,
        >,
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
    use crate::env::ExecutionEnv;
    use crate::tool::ToolState;
    use crate::types::{StopReason, ThinkingKind, Usage};
    use std::path::{Path, PathBuf};

    struct TestEnv;

    #[async_trait::async_trait]
    impl ExecutionEnv for TestEnv {
        fn cwd(&self) -> &Path {
            Path::new("/test")
        }
        async fn absolute_path(&self, path: &Path) -> Result<PathBuf, crate::env::FileError> {
            Ok(path.to_path_buf())
        }
        fn join_path(&self, parts: &[&str]) -> PathBuf {
            parts.iter().collect()
        }
        async fn read_file(
            &self,
            _path: &Path,
            _offset: Option<usize>,
            _limit: Option<usize>,
        ) -> Result<String, crate::env::FileError> {
            Ok(String::new())
        }
        async fn write_file(
            &self,
            _path: &Path,
            _content: &str,
        ) -> Result<(), crate::env::FileError> {
            Ok(())
        }
        async fn exists(&self, _path: &Path) -> Result<bool, crate::env::FileError> {
            Ok(false)
        }
        async fn file_info(
            &self,
            _path: &Path,
        ) -> Result<crate::env::FileInfo, crate::env::FileError> {
            unreachable!("TestEnv fs ops are not exercised by agent tests")
        }
        async fn list_dir(
            &self,
            _path: &Path,
        ) -> Result<Vec<crate::env::FileInfo>, crate::env::FileError> {
            Ok(vec![])
        }
        async fn create_dir(&self, _path: &Path) -> Result<(), crate::env::FileError> {
            Ok(())
        }
        async fn remove(&self, _path: &Path) -> Result<(), crate::env::FileError> {
            Ok(())
        }
        async fn exec(
            &self,
            _command: &str,
            _timeout: std::time::Duration,
        ) -> Result<crate::env::CommandResult, crate::env::ExecutionError> {
            Ok(crate::env::CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            })
        }
    }

    struct TestToolContext {
        state: ToolState,
    }

    impl ToolContext for TestToolContext {
        fn env(&self) -> &dyn ExecutionEnv {
            &TestEnv
        }
        fn cwd(&self) -> &Path {
            Path::new("/test")
        }
        fn tool_state(&self) -> &ToolState {
            &self.state
        }
    }

    fn test_tool_ctx() -> Arc<dyn ToolContext> {
        Arc::new(TestToolContext {
            state: ToolState::new(),
        })
    }

    struct TestStreamFn;

    #[async_trait::async_trait]
    impl StreamFn for TestStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "Test response".into(),
                    signature: None,
                }],
                model: "test".into(),
                provider: "test".into(),
                api: "test".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                stop_reason: Some(StopReason::Stop),
                usage: Box::new(Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }

    fn test_model() -> Model {
        Model {
            provider: "test".into(),
            id: "test".into(),
            context_window: 100_000,
            max_tokens: 8_192,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        }
    }

    #[tokio::test]
    async fn test_agent_prompt() {
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
            test_tool_ctx(),
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
            test_tool_ctx(),
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
            test_tool_ctx(),
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
            test_tool_ctx(),
        );
        agent.steer(AgentMessage::user("steering message"));
        assert!(agent.has_queued_messages());
        agent.clear_steering_queue();
        assert!(!agent.has_queued_messages());
    }

    #[tokio::test]
    async fn steering_queued_before_run_is_injected_by_loop() {
        // #367: a steering message queued before the run starts must be
        // drained by the loop's `get_steering_messages` callback and injected
        // into the transcript between the user prompt and the assistant
        // response — not left sitting in the queue.
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
            test_tool_ctx(),
        );
        agent.steer(AgentMessage::user("STEER"));

        let messages = agent.prompt("hi").await.unwrap();
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, AgentMessage::User { content, .. }
                    if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "STEER")))),
            "steering message must appear in the run's new messages"
        );
        assert!(
            !agent.has_queued_messages(),
            "steering queue must be drained after the run"
        );
    }
}
