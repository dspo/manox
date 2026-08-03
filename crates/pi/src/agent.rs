// Agent — stateful wrapper around the agent loop.
//
// Owns the conversation transcript, manages event subscriptions, and exposes
// queueing APIs for steering and follow-up messages. The Agent wraps the raw
// `run_loop` / `run_loop_continue` functions with lifecycle management.
//
// State is event-reduced: as the loop emits, each event is first applied to
// `AgentState` (the transcript grows exclusively through `MessageEnd`) and
// then dispatched to subscribed listeners, awaited in registration order.
// A slow listener therefore backpressures the loop itself.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::agent_loop::{EventSink, StreamFn, StreamResolver, run_loop, run_loop_continue};
use crate::tool::{AgentToolResult, ToolContext};
use crate::types::{
    AfterToolCallFn, AgentContext, AgentEvent, AgentLoopConfig, AgentMessage, AgentState,
    BeforeProviderRequestFn, BeforeToolCallFn, CacheRetention, ContentBlock, Model, PrepareTurnFn,
    TurnUpdate,
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

    fn set_mode(&mut self, mode: QueueMode) {
        self.mode = mode;
    }

    fn len(&self) -> usize {
        self.messages.len()
    }

    fn clear(&mut self) {
        self.messages.clear();
    }
}

/// A listener invoked for every run event.
///
/// Receives the event and the active run's cancellation token. Listeners are
/// awaited in registration order before the loop advances past the event, so
/// they are part of the run's settlement: `agent_end` does not make the agent
/// idle until its listeners have completed.
pub type EventMiddleware = Arc<
    dyn Fn(
            AgentEvent,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), anyhow::Error>> + Send>>
        + Send
        + Sync,
>;

/// A listener invoked for every run event.
///
/// Receives the event and the active run's cancellation token. Listeners are
/// awaited in registration order before the loop advances past the event, so
/// they are part of the run's settlement: `agent_end` does not make the agent
/// idle until its listeners have completed.
pub type AgentListener = Arc<
    dyn Fn(AgentEvent, CancellationToken) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync,
>;

/// A registered listener's removal handle; unsubscribes on drop.
pub struct Subscription {
    id: u64,
    listeners: Arc<Mutex<Vec<(u64, AgentListener)>>>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.listeners
            .lock()
            .unwrap()
            .retain(|(id, _)| *id != self.id);
    }
}

/// The sink handed to the loop during a run.
///
/// Events travel a bounded channel to the reducing side of
/// [`Agent::run_with_lifecycle`]. Capacity one lets the loop run a single
/// event ahead of the reducer; each emission awaits an acknowledgement fired
/// only after the event has been reduced and its listeners awaited, so the
/// loop's next step observes listener side effects — the same ordering TS
/// Pi's awaited `emit` provides.
struct ChannelSink {
    tx: mpsc::Sender<(AgentEvent, tokio::sync::oneshot::Sender<()>)>,
}

#[async_trait::async_trait]
impl EventSink for ChannelSink {
    async fn emit(&self, event: AgentEvent) -> Result<(), anyhow::Error> {
        let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
        // A closed channel means the reducer is gone, which only happens once
        // the run has already settled — nothing left to deliver to.
        if self.tx.send((event, ack_tx)).await.is_ok() {
            let _ = ack_rx.await;
        }
        Ok(())
    }
}

/// The registered state of a run in flight.
///
/// `finish_tx` flips to `true` after the run's final events (including their
/// listeners) have settled and runtime-owned state has been cleared — the
/// point at which [`Agent::wait_for_idle`] resolves.
struct ActiveRun {
    token: CancellationToken,
    finish_tx: watch::Sender<bool>,
}

/// Per-run observation hooks cloned into each turn's `AgentLoopConfig`.
///
/// Held as `Arc<dyn Fn>` so `create_loop_config` can produce a fresh `Box`
/// closure per run without owning the (un-`Clone`) originals. The harness
/// fills these from its registered `HookPoint`s.
pub type BeforeProviderRequestHook = Arc<dyn Fn(&AgentContext) -> AgentContext + Send + Sync>;
pub type BeforeToolCallHook = Arc<dyn Fn(&str, &str, &JsonValue) -> Option<String> + Send + Sync>;
pub type AfterToolCallHook = Arc<dyn Fn(&AgentToolResult) -> AgentToolResult + Send + Sync>;
pub type PrepareTurnHook = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Option<TurnUpdate>, anyhow::Error>> + Send>,
        > + Send
        + Sync,
>;

#[derive(Default)]
pub struct LoopHooks {
    pub before_provider_request: Option<BeforeProviderRequestHook>,
    pub before_tool_call: Option<BeforeToolCallHook>,
    pub after_tool_call: Option<AfterToolCallHook>,
    /// Refreshes the loop context before the next turn of the same run — the
    /// TS `prepareNextTurn` seam for applying runtime mutations (model,
    /// thinking level) queued mid-run.
    pub prepare_next_turn: Option<PrepareTurnHook>,
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
    /// The active run's registration, shared with [`RunHandle`] so `abort` and
    /// `wait_for_idle` work without an `&mut self` borrow on the Agent.
    active_run: Arc<Mutex<Option<Arc<ActiveRun>>>>,
    /// Subscribed event listeners, in registration order. Shared with
    /// [`RunHandle`] so listeners can be added mid-run.
    listeners: Arc<Mutex<Vec<(u64, AgentListener)>>>,
    /// Event middleware, run after reduction and before listeners, in
    /// registration order. Shared with [`RunHandle`] so a harness can attach
    /// its persistence middleware before a run starts.
    middlewares: Arc<Mutex<Vec<EventMiddleware>>>,
    /// Next listener registration id.
    next_listener_id: Arc<AtomicU64>,
    stream_fn: Arc<dyn StreamFn>,
    /// Per-model provider runtime resolution, when the consumer plugs one in;
    /// without it every turn uses [`Self::stream_fn`].
    stream_resolver: Option<StreamResolver>,
    /// Tools mounted on the agent and forwarded into each turn's context.
    tools: Arc<[Arc<dyn crate::tool::AgentTool>]>,
    /// Session-scoped execution context for tool calls. Backs the real
    /// `ToolContext` (env + cwd + tool state) so tools reach the filesystem
    /// and shell instead of panicking.
    tool_ctx: Arc<dyn ToolContext>,
    /// Session identifier forwarded to providers that support session-based
    /// caching (`prompt_cache_key`).
    session_id: Option<String>,
    /// Prompt cache retention preference forwarded to providers.
    cache_retention: CacheRetention,
    /// Per-request provider options from the harness turn snapshot,
    /// forwarded into each turn's context.
    stream_options: crate::types::StreamOptions,
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
            active_run: Arc::new(Mutex::new(None)),
            listeners: Arc::new(Mutex::new(Vec::new())),
            middlewares: Arc::new(Mutex::new(Vec::new())),
            next_listener_id: Arc::new(AtomicU64::new(1)),
            stream_fn,
            stream_resolver: None,
            tools: Arc::from(Vec::new()),
            tool_ctx,
            session_id: None,
            cache_retention: CacheRetention::default(),
            stream_options: crate::types::StreamOptions::default(),
            loop_hooks: LoopHooks::default(),
        }
    }

    /// The session-scoped tool execution context.
    pub fn tool_context(&self) -> &Arc<dyn ToolContext> {
        &self.tool_ctx
    }

    /// Mount tools on the agent. They are forwarded into each turn's context
    /// so the provider sees them and `execute_tool_calls` can dispatch.
    pub fn with_tools(mut self, tools: Arc<[Arc<dyn crate::tool::AgentTool>]>) -> Self {
        self.tools = tools;
        self
    }

    /// Replace the mounted tools.
    pub fn set_tools(&mut self, tools: Arc<[Arc<dyn crate::tool::AgentTool>]>) {
        self.tools = tools;
    }

    /// The tools currently forwarded into each turn's context.
    pub fn tools(&self) -> &[Arc<dyn crate::tool::AgentTool>] {
        &self.tools
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

    /// Set the per-request provider options forwarded into every turn's
    /// context.
    pub fn set_stream_options(&mut self, options: crate::types::StreamOptions) {
        self.stream_options = options;
    }

    /// The per-request provider options.
    pub fn stream_options(&self) -> &crate::types::StreamOptions {
        &self.stream_options
    }

    /// Set the per-run observation hooks forwarded into the loop config.
    pub fn set_loop_hooks(&mut self, hooks: LoopHooks) {
        self.loop_hooks = hooks;
    }

    /// Replace the session-scoped tool execution context (env + cwd + tool
    /// state), so tools run against the session's project directory rather
    /// than the process cwd.
    pub fn set_tool_ctx(&mut self, tool_ctx: Arc<dyn ToolContext>) {
        self.tool_ctx = tool_ctx;
    }

    /// Plug in per-model provider runtime resolution. Every turn resolves its
    /// stream function from the current model, so a mid-run model change
    /// switches protocol/endpoint/credentials for the next provider call.
    pub fn set_stream_resolver(&mut self, resolver: StreamResolver) {
        self.stream_resolver = Some(resolver);
    }

    /// Replace the stream function used for provider calls.
    pub fn set_stream_fn(&mut self, stream_fn: Arc<dyn StreamFn>) {
        self.stream_fn = stream_fn;
    }

    /// Set the reasoning tier forwarded into each turn's context. `None`
    /// means the provider default — the `"off"` tier a session path carries
    /// never reaches the provider.
    pub fn set_thinking_level(&mut self, thinking_level: Option<String>) {
        self.state.thinking_level = thinking_level;
    }

    /// Replace the system prompt the next turn's context snapshot carries.
    pub fn set_system_prompt(&mut self, system_prompt: impl Into<String>) {
        self.state.system_prompt = system_prompt.into();
    }

    /// Replace the model the next turn runs against.
    pub fn set_model(&mut self, model: crate::types::Model) {
        self.state.model = model;
    }

    /// Current agent state.
    pub fn state(&self) -> &AgentState {
        &self.state
    }

    /// Register an event middleware, run after reduction and before
    /// listeners. An `Err` aborts the run.
    pub fn add_middleware(&self, middleware: EventMiddleware) {
        self.middlewares.lock().unwrap().push(middleware);
    }

    /// Register a listener for run events.
    ///
    /// The listener is awaited for every event — including mid-run — until the
    /// returned [`Subscription`] is dropped. Listeners run after the event has
    /// been reduced into [`AgentState`], so a listener always observes the
    /// post-event state.
    pub fn subscribe(&self, listener: AgentListener) -> Subscription {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().unwrap().push((id, listener));
        Subscription {
            id,
            listeners: Arc::clone(&self.listeners),
        }
    }

    /// Resolve once the active run has fully settled: its final event's
    /// listeners have completed and runtime-owned state has been cleared.
    /// Returns immediately when no run is in flight.
    pub async fn wait_for_idle(&self) {
        let mut finished = match self.active_run.lock().unwrap().as_ref() {
            Some(active) => active.finish_tx.subscribe(),
            None => return,
        };
        if *finished.borrow_and_update() {
            return;
        }
        let _ = finished.changed().await;
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

    /// Remove every queued steering and follow-up message.
    pub fn clear_all_queues(&self) {
        self.steering_queue.lock().unwrap().clear();
        self.follow_up_queue.lock().unwrap().clear();
    }

    /// The steering queue drain mode.
    pub fn steering_mode(&self) -> QueueMode {
        self.steering_queue.lock().unwrap().mode
    }

    /// Change the steering queue drain mode.
    pub fn set_steering_mode(&self, mode: QueueMode) {
        self.steering_queue.lock().unwrap().set_mode(mode);
    }

    /// The follow-up queue drain mode.
    pub fn follow_up_mode(&self) -> QueueMode {
        self.follow_up_queue.lock().unwrap().mode
    }

    /// Change the follow-up queue drain mode.
    pub fn set_follow_up_mode(&self, mode: QueueMode) {
        self.follow_up_queue.lock().unwrap().set_mode(mode);
    }

    /// Number of queued steering messages.
    pub fn queued_steering_count(&self) -> usize {
        self.steering_queue.lock().unwrap().len()
    }

    /// Number of queued follow-up messages.
    pub fn queued_follow_up_count(&self) -> usize {
        self.follow_up_queue.lock().unwrap().len()
    }

    /// Whether either queue has pending messages.
    pub fn has_queued_messages(&self) -> bool {
        self.steering_queue.lock().unwrap().has_items()
            || self.follow_up_queue.lock().unwrap().has_items()
    }

    /// Abort the current run, if one is active.
    ///
    /// Cancellation is cooperative: the loop notices the token and winds the
    /// run down through its normal terminal events. The run's registration
    /// stays until settlement, so `wait_for_idle` still tracks the aborting
    /// run to its end.
    pub fn abort(&self) {
        if let Some(active) = self.active_run.lock().unwrap().as_ref() {
            active.token.cancel();
        }
    }

    /// Whether a run is currently in flight.
    fn is_running(&self) -> bool {
        self.active_run.lock().unwrap().is_some()
    }

    /// A decoupled handle for mid-run control.
    ///
    /// `prompt`/`continue_` take `&mut self` for the whole run, so the Agent's
    /// own `steer`/`follow_up`/`abort` cannot be called while a run is in
    /// flight. The handle shares the same `Arc`-backed queues, listener
    /// registry, and run slot, exposing `&self` methods callable from another
    /// task during the run.
    pub fn run_handle(&self) -> RunHandle {
        RunHandle {
            steering_queue: Arc::clone(&self.steering_queue),
            follow_up_queue: Arc::clone(&self.follow_up_queue),
            active_run: Arc::clone(&self.active_run),
            listeners: Arc::clone(&self.listeners),
            next_listener_id: Arc::clone(&self.next_listener_id),
        }
    }

    /// Clear transcript and run-state leftovers, keeping the steering and
    /// follow-up queues. Queued messages are user input, not transcript
    /// state, so transcript rebuilds (compaction, session restore) must not
    /// drop them.
    pub fn clear_transcript_state(&mut self) {
        self.state.messages.clear();
        self.state.is_streaming = false;
        self.state.streaming_message = None;
        self.state.pending_tool_calls.clear();
        self.state.error_message = None;
    }

    /// Reset the agent's transcript and queues.
    pub fn reset(&mut self) {
        self.clear_transcript_state();
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
        if self.is_running() {
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

    /// Start a new prompt from a batch of messages, appended to the
    /// transcript in order.
    pub async fn prompt_messages(
        &mut self,
        messages: &[AgentMessage],
    ) -> Result<Vec<AgentMessage>, anyhow::Error> {
        if self.is_running() {
            anyhow::bail!("Agent is already processing a prompt.");
        }
        self.run_prompt_messages(messages).await
    }

    /// Continue from the current transcript.
    pub async fn continue_(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        if self.is_running() {
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
            stream_options: self.stream_options.clone(),
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
        let prepare_next_turn = self.loop_hooks.prepare_next_turn.as_ref().map(|h| {
            let h = Arc::clone(h);
            Box::new(move || h()) as PrepareTurnFn
        });
        AgentLoopConfig {
            get_steering_messages: Some(Box::new(move || steering.lock().unwrap().drain())),
            get_follow_up_messages: Some(Box::new(move || follow_up.lock().unwrap().drain())),
            prepare_next_turn,
            stream_resolver: self.stream_resolver.clone(),
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
        let msgs = messages.to_vec();
        let mut context = self.create_context_snapshot();
        let config = self.create_loop_config();
        let stream_fn = Arc::clone(&self.stream_fn);
        let tool_ctx = Arc::clone(&self.tool_ctx);

        self.run_with_lifecycle(|signal, sink| async move {
            run_loop(
                &msgs,
                &mut context,
                &config,
                Some(signal),
                stream_fn,
                &*tool_ctx,
                &sink,
            )
            .await
        })
        .await
    }

    async fn run_continuation(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        let mut context = self.create_context_snapshot();
        let config = self.create_loop_config();
        let stream_fn = Arc::clone(&self.stream_fn);
        let tool_ctx = Arc::clone(&self.tool_ctx);

        self.run_with_lifecycle(|signal, sink| async move {
            run_loop_continue(
                &mut context,
                &config,
                Some(signal),
                stream_fn,
                &*tool_ctx,
                &sink,
            )
            .await
        })
        .await
    }

    /// Drive one run to settlement.
    ///
    /// The loop future emits into a bounded channel; this side reduces each
    /// event into the agent state and awaits listeners before the loop
    /// advances. Because the transcript accumulates through `MessageEnd`
    /// reduction alone, `state.messages` is current throughout the run — not
    /// only after it — and matches what the loop's own context built.
    async fn run_with_lifecycle<F, Fut>(
        &mut self,
        executor: F,
    ) -> Result<Vec<AgentMessage>, anyhow::Error>
    where
        F: FnOnce(CancellationToken, ChannelSink) -> Fut,
        Fut: Future<Output = Result<Vec<AgentMessage>, anyhow::Error>>,
    {
        if self.is_running() {
            anyhow::bail!("Agent is already processing.");
        }

        let token = CancellationToken::new();
        let (finish_tx, _) = watch::channel(false);
        *self.active_run.lock().unwrap() = Some(Arc::new(ActiveRun {
            token: token.clone(),
            finish_tx,
        }));
        self.state.is_streaming = true;
        self.state.streaming_message = None;
        self.state.error_message = None;

        let (tx, mut rx) = mpsc::channel::<(AgentEvent, tokio::sync::oneshot::Sender<()>)>(1);
        let mut run = Box::pin(executor(token.clone(), ChannelSink { tx }));
        let result = loop {
            tokio::select! {
                biased;
                ev = rx.recv() => match ev {
                    Some((ev, ack)) => {
                        if let Err(e) = self.process_event(ev, &token).await {
                            let _ = ack.send(());
                            break Err(e);
                        }
                        // The loop's emit awaits this acknowledgement, so the
                        // next loop step sees the listener side effects.
                        let _ = ack.send(());
                    }
                    // The sender lives inside the run future, so the channel
                    // can only close after the run completed — which the other
                    // branch observes first. This arm never fires.
                    None => unreachable!("event channel closed before the run completed"),
                },
                r = &mut run => break r,
            }
        };
        drop(run);
        // Settle events the loop emitted just before finishing, so `agent_end`
        // and its listeners are part of the run.
        while let Ok((ev, ack)) = rx.try_recv() {
            if let Err(e) = self.process_event(ev, &token).await {
                let _ = ack.send(());
                return Err(e);
            }
            let _ = ack.send(());
        }

        self.state.is_streaming = false;
        self.state.streaming_message = None;
        self.state.pending_tool_calls.clear();
        if let Some(active) = self.active_run.lock().unwrap().take() {
            let _ = active.finish_tx.send(true);
        }

        result
    }

    /// Reduce one loop event into the agent state, run middlewares, then
    /// dispatch it to subscribed listeners in registration order. A middleware
    /// error aborts the run.
    async fn process_event(
        &mut self,
        event: AgentEvent,
        token: &CancellationToken,
    ) -> Result<(), anyhow::Error> {
        match &event {
            AgentEvent::MessageStart { message } | AgentEvent::MessageUpdate { message, .. } => {
                self.state.streaming_message = Some((**message).clone());
            }
            AgentEvent::MessageEnd { message } => {
                self.state.streaming_message = None;
                self.state.messages.push((**message).clone());
            }
            AgentEvent::ToolExecutionStart { tool_call_id, .. }
                if !self.state.pending_tool_calls.contains(tool_call_id) =>
            {
                self.state.pending_tool_calls.push(tool_call_id.clone());
            }
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                self.state
                    .pending_tool_calls
                    .retain(|id| id != tool_call_id);
            }
            AgentEvent::TurnEnd { message, .. } => {
                if let AgentMessage::Assistant {
                    error_message: Some(error),
                    ..
                } = &**message
                {
                    self.state.error_message = Some(error.clone());
                }
            }
            AgentEvent::AgentEnd { .. } => {
                self.state.streaming_message = None;
            }
            _ => {}
        }

        let middlewares: Vec<EventMiddleware> =
            self.middlewares.lock().unwrap().iter().cloned().collect();
        for middleware in middlewares {
            middleware(event.clone()).await?;
        }

        let listeners: Vec<AgentListener> = self
            .listeners
            .lock()
            .unwrap()
            .iter()
            .map(|(_, listener)| Arc::clone(listener))
            .collect();
        for listener in listeners {
            listener(event.clone(), token.clone()).await;
        }
        Ok(())
    }
}

/// Decoupled, cloneable handle for mid-run control of an [`Agent`].
///
/// Shares the agent's steering/follow-up queues and cancel slot via `Arc`, so
/// `steer`/`follow_up`/`abort` work from another task while `prompt` holds the
/// exclusive borrow on the Agent.
#[derive(Clone)]
pub struct RunHandle {
    steering_queue: Arc<Mutex<PendingMessageQueue>>,
    follow_up_queue: Arc<Mutex<PendingMessageQueue>>,
    active_run: Arc<Mutex<Option<Arc<ActiveRun>>>>,
    listeners: Arc<Mutex<Vec<(u64, AgentListener)>>>,
    next_listener_id: Arc<AtomicU64>,
}

impl RunHandle {
    /// Queue a steering message injected into the current or next turn.
    pub fn steer(&self, message: AgentMessage) {
        self.steering_queue.lock().unwrap().enqueue(message);
    }

    /// Queue a follow-up message that resumes a run that would otherwise stop.
    pub fn follow_up(&self, message: AgentMessage) {
        self.follow_up_queue.lock().unwrap().enqueue(message);
    }

    /// Cancel the active run, if one is in flight.
    pub fn abort(&self) {
        if let Some(active) = self.active_run.lock().unwrap().as_ref() {
            active.token.cancel();
        }
    }

    /// Drop every queued steering and follow-up message.
    pub fn clear_queues(&self) {
        self.steering_queue.lock().unwrap().clear();
        self.follow_up_queue.lock().unwrap().clear();
    }

    /// Number of queued steering messages.
    pub fn queued_steering_count(&self) -> usize {
        self.steering_queue.lock().unwrap().len()
    }

    /// Number of queued follow-up messages.
    pub fn queued_follow_up_count(&self) -> usize {
        self.follow_up_queue.lock().unwrap().len()
    }

    /// Resolve once the active run has fully settled, like
    /// [`Agent::wait_for_idle`]. Returns immediately when no run is in flight.
    pub async fn wait_for_idle(&self) {
        let mut finished = match self.active_run.lock().unwrap().as_ref() {
            Some(active) => active.finish_tx.subscribe(),
            None => return,
        };
        if *finished.borrow_and_update() {
            return;
        }
        let _ = finished.changed().await;
    }

    /// Register a listener for run events, like [`Agent::subscribe`]; usable
    /// while a run is in flight.
    pub fn subscribe(&self, listener: AgentListener) -> Subscription {
        let id = self.next_listener_id.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().unwrap().push((id, listener));
        Subscription {
            id,
            listeners: Arc::clone(&self.listeners),
        }
    }

    /// Whether either queue has pending messages.
    pub fn has_queued_messages(&self) -> bool {
        self.steering_queue.lock().unwrap().has_items()
            || self.follow_up_queue.lock().unwrap().has_items()
    }

    /// Remove all queued steering messages.
    pub fn clear_steering_queue(&self) {
        self.steering_queue.lock().unwrap().clear();
    }

    /// Remove all queued follow-up messages.
    pub fn clear_follow_up_queue(&self) {
        self.follow_up_queue.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::ExecutionEnv;
    use crate::tool::ToolState;
    use crate::types::{AssistantMessageEvent, StopReason, ThinkingKind, Usage};
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
            _signal: CancellationToken,
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
                raw_stop_reason: None,
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
            api: "test".into(),
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
        let agent = Agent::new(
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
        agent.steer(AgentMessage::user("queued"));
        agent.reset();
        assert!(agent.state().messages.is_empty());
        assert!(!agent.has_queued_messages());
    }

    #[tokio::test]
    async fn clear_transcript_state_keeps_queued_messages() {
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
            test_tool_ctx(),
        );

        let _ = agent.prompt("Hello").await;
        agent.steer(AgentMessage::user("queued"));
        agent.clear_transcript_state();
        assert!(agent.state().messages.is_empty());
        assert!(agent.has_queued_messages());
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

    #[tokio::test]
    async fn steer_via_run_handle_shares_steering_queue() {
        // F3: RunHandle must share the same steering queue the loop drains,
        // so a message enqueued through the handle before a run is injected.
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
            test_tool_ctx(),
        );
        let handle = agent.run_handle();
        handle.steer(AgentMessage::user("STEER-HANDLE"));

        let messages = agent.prompt("hi").await.unwrap();
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, AgentMessage::User { content, .. }
                    if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "STEER-HANDLE")))),
            "handle-steered message must appear in the run's new messages"
        );
        assert!(
            !agent.has_queued_messages(),
            "shared steering queue must be drained after the run"
        );
    }

    /// Stream fn that blocks until the run is cancelled, then surfaces an
    /// `Aborted` terminal — lets an abort test exercise a run in flight.
    struct BlockingStreamFn;

    #[async_trait::async_trait]
    impl StreamFn for BlockingStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            signal.cancelled().await;
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "aborted".into(),
                    signature: None,
                }],
                model: "test".into(),
                provider: "test".into(),
                api: "test".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(StopReason::Aborted),
                usage: Box::new(Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }

    #[tokio::test]
    async fn abort_via_run_handle_terminates_run_in_flight() {
        // F3: RunHandle::abort must cancel a run in flight via the shared
        // cancel slot, without needing an &mut self on the Agent (which prompt
        // holds for the whole run).
        //
        // `prompt`'s future is not `Send` (the loop's pinned executor future
        // borrows `&mut AgentContext`), so drive it on a `LocalSet` instead of
        // a multi-threaded `tokio::spawn`.
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(BlockingStreamFn),
            test_tool_ctx(),
        );
        let handle = agent.run_handle();

        let result = tokio::task::LocalSet::new()
            .run_until(async move {
                let join = tokio::task::spawn_local(async move { agent.prompt("hi").await });
                // Let the run register its cancel token in the shared slot.
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                handle.abort();
                tokio::time::timeout(std::time::Duration::from_secs(2), join)
                    .await
                    .expect("abort did not terminate the run within timeout")
                    .expect("spawned task panicked")
            })
            .await;
        assert!(
            result.is_ok(),
            "aborted run must complete with Ok, got: {:?}",
            result.err()
        );
    }

    // ── Reducer / subscription / lifecycle ──────────────────────────────────

    /// Stream fn that forwards start + deltas through the channel before
    /// returning the completed assistant message, like a real provider.
    struct StreamingStreamFn;

    fn streaming_partial(text: &str) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text: text.into(),
                signature: None,
            }],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        }
    }

    #[async_trait::async_trait]
    impl StreamFn for StreamingStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            let _ = event_tx
                .send(AgentEvent::MessageStart {
                    message: Box::new(streaming_partial("")),
                })
                .await;
            for chunk in ["Hello", " world"] {
                let _ = event_tx
                    .send(AgentEvent::MessageUpdate {
                        message: Box::new(streaming_partial(chunk)),
                        assistant_message_event: AssistantMessageEvent::TextDelta {
                            content_index: 0,
                            delta: chunk.into(),
                        },
                    })
                    .await;
            }
            Ok(streaming_partial("Hello world"))
        }
    }

    fn event_name(event: &AgentEvent) -> &'static str {
        match event {
            AgentEvent::AgentStart => "agent_start",
            AgentEvent::TurnStart => "turn_start",
            AgentEvent::MessageStart { .. } => "message_start",
            AgentEvent::MessageUpdate { .. } => "message_update",
            AgentEvent::MessageEnd { .. } => "message_end",
            AgentEvent::ToolExecutionStart { .. } => "tool_execution_start",
            AgentEvent::ToolExecutionUpdate { .. } => "tool_execution_update",
            AgentEvent::ToolExecutionEnd { .. } => "tool_execution_end",
            AgentEvent::TurnEnd { .. } => "turn_end",
            AgentEvent::Retry { .. } => "retry",
            AgentEvent::AgentEnd { .. } => "agent_end",
        }
    }

    fn recording_listener(log: Arc<Mutex<Vec<&'static str>>>) -> AgentListener {
        Arc::new(move |event, _token| {
            let log = Arc::clone(&log);
            Box::pin(async move {
                log.lock().unwrap().push(event_name(&event));
            })
        })
    }

    #[tokio::test]
    async fn listeners_observe_events_in_wire_order() {
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(StreamingStreamFn),
            test_tool_ctx(),
        );
        let log = Arc::new(Mutex::new(Vec::new()));
        let _sub = agent.subscribe(recording_listener(Arc::clone(&log)));

        agent.prompt("hi").await.unwrap();

        assert_eq!(
            *log.lock().unwrap(),
            vec![
                "agent_start",
                "turn_start",
                "message_start",
                "message_end",
                "message_start",
                "message_update",
                "message_update",
                "message_end",
                "turn_end",
                "agent_end",
            ]
        );
    }

    #[tokio::test]
    async fn dropped_subscription_stops_delivery() {
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(StreamingStreamFn),
            test_tool_ctx(),
        );
        let log = Arc::new(Mutex::new(Vec::new()));
        let sub = agent.subscribe(recording_listener(Arc::clone(&log)));
        drop(sub);

        agent.prompt("hi").await.unwrap();
        assert!(
            log.lock().unwrap().is_empty(),
            "a dropped subscription must receive nothing"
        );

        // A fresh subscription still receives the next run's events.
        let _sub = agent.subscribe(recording_listener(Arc::clone(&log)));
        agent.prompt("again").await.unwrap();
        assert!(!log.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn transcript_grows_via_message_end_and_matches_new_messages() {
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(StreamingStreamFn),
            test_tool_ctx(),
        );

        let new_messages = agent.prompt("hi").await.unwrap();
        assert_eq!(new_messages.len(), 2, "user prompt + assistant response");
        assert_eq!(agent.state().messages.len(), new_messages.len());
        assert!(
            agent.state().streaming_message.is_none(),
            "streaming message clears when the run settles"
        );
        assert!(!agent.state().is_streaming);
        assert!(agent.state().pending_tool_calls.is_empty());
    }

    #[tokio::test]
    async fn wait_for_idle_resolves_after_agent_end_listeners() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

        /// Streams like [`StreamingStreamFn`] and signals once the provider
        /// call has begun — at which point the run's lifecycle registration
        /// is already visible to `wait_for_idle`.
        struct SignalingStreamFn {
            started_tx: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        }

        #[async_trait::async_trait]
        impl StreamFn for SignalingStreamFn {
            async fn stream(
                &self,
                _context: &AgentContext,
                _signal: CancellationToken,
                event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                if let Some(tx) = self.started_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                let _ = event_tx
                    .send(AgentEvent::MessageStart {
                        message: Box::new(streaming_partial("")),
                    })
                    .await;
                Ok(streaming_partial("Hello"))
            }
        }

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(SignalingStreamFn {
                started_tx: Mutex::new(Some(started_tx)),
            }),
            test_tool_ctx(),
        );
        let agent_end_seen = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&agent_end_seen);
        let _sub = agent.subscribe(Arc::new(move |event, _token| {
            let seen = Arc::clone(&seen);
            Box::pin(async move {
                if matches!(event, AgentEvent::AgentEnd { .. }) {
                    // Listeners are awaited: a slow agent_end listener holds
                    // the run open past the loop's own completion.
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    seen.store(true, AtomicOrdering::SeqCst);
                }
            })
        }));
        let handle = agent.run_handle();

        // No run in flight: resolves immediately.
        handle.wait_for_idle().await;

        tokio::task::LocalSet::new()
            .run_until(async move {
                let join = tokio::task::spawn_local(async move { agent.prompt("hi").await });
                // Registration precedes the loop's first provider call, so a
                // started stream implies `wait_for_idle` sees the active run.
                started_rx.await.expect("stream fn never started");
                handle.wait_for_idle().await;
                assert!(
                    agent_end_seen.load(AtomicOrdering::SeqCst),
                    "wait_for_idle must not resolve before agent_end listeners settle"
                );
                join.await.expect("prompt task panicked").unwrap();
            })
            .await;
    }

    /// Stream fn whose provider call fails outright; the loop materializes a
    /// terminal `Error` assistant message.
    struct FailingStreamFn;

    #[async_trait::async_trait]
    impl StreamFn for FailingStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            Err(anyhow::anyhow!("provider exploded"))
        }
    }

    #[tokio::test]
    async fn error_message_survives_run_settlement() {
        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(FailingStreamFn),
            test_tool_ctx(),
        );

        agent.prompt("hi").await.unwrap();

        let state = agent.state();
        assert!(!state.is_streaming);
        assert!(state.streaming_message.is_none());
        let error = state
            .error_message
            .as_deref()
            .expect("a failed turn leaves its error on the state");
        assert!(
            error.contains("provider exploded"),
            "unexpected error message: {error}"
        );
        assert!(
            state.messages.iter().any(|m| matches!(
                m,
                AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                }
            )),
            "the terminal Error assistant message must be in the transcript"
        );
    }

    /// Stream fn replaying a scripted sequence of assistant messages, one per
    /// provider call.
    struct ScriptedStreamFn {
        scripts: Mutex<std::collections::VecDeque<AgentMessage>>,
    }

    impl ScriptedStreamFn {
        fn new(messages: Vec<AgentMessage>) -> Self {
            ScriptedStreamFn {
                scripts: Mutex::new(messages.into()),
            }
        }
    }

    #[async_trait::async_trait]
    impl StreamFn for ScriptedStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            self.scripts
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow::anyhow!("script exhausted"))
        }
    }

    struct EchoTool;

    #[async_trait::async_trait]
    impl crate::tool::AgentTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes input"
        }
        fn parameters_schema(&self) -> JsonValue {
            serde_json::json!({
                "type": "object",
                "properties": { "message": { "type": "string" } },
                "required": ["message"]
            })
        }
        async fn execute(
            &self,
            _tool_call_id: &str,
            params: JsonValue,
            _signal: CancellationToken,
            _ctx: &dyn ToolContext,
        ) -> Result<AgentToolResult, crate::tool::ToolError> {
            Ok(AgentToolResult::text(
                params["message"].as_str().unwrap_or("no message"),
            ))
        }
    }

    #[tokio::test]
    async fn tool_execution_events_bracket_execution_and_pending_settles() {
        let tool_call_msg = AgentMessage::Assistant {
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "echo".into(),
                input: serde_json::json!({"message": "hello"}),
                thought_signature: None,
            }],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::ToolUse),
            usage: Box::new(Usage::default()),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        let text_msg = streaming_partial("done");

        let mut agent = Agent::new(
            "You are a test assistant.",
            test_model(),
            Arc::new(ScriptedStreamFn::new(vec![tool_call_msg, text_msg])),
            test_tool_ctx(),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        let log = Arc::new(Mutex::new(Vec::new()));
        let _sub = agent.subscribe(recording_listener(Arc::clone(&log)));

        agent.prompt("hi").await.unwrap();

        let events = log.lock().unwrap();
        let start = events
            .iter()
            .position(|e| *e == "tool_execution_start")
            .expect("tool_execution_start missing");
        let end = events
            .iter()
            .position(|e| *e == "tool_execution_end")
            .expect("tool_execution_end missing");
        assert!(start < end, "start must precede end: {events:?}");
        assert!(
            agent.state().pending_tool_calls.is_empty(),
            "the pending set empties once every call settles"
        );
        assert!(
            agent
                .state()
                .messages
                .iter()
                .any(|m| matches!(m, AgentMessage::ToolResult { .. })),
            "the tool result message must be in the transcript"
        );
    }
}
