// AgentHarness — orchestration layer.
//
// Wraps the agent loop with session persistence, hooks, compaction
// integration, and phase management. This is the primary public API
// for consumers of the harness.

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{
    AfterToolCallHook, Agent, BeforeProviderRequestHook, BeforeToolCallHook, LoopHooks, RunHandle,
};
use crate::agent_loop::StreamFn;
use crate::compaction::{self, CompactionPreparation, CompactionResult, CompactionSettings};
use crate::env::{ExecutionEnv, TokioExecutionEnv};
use crate::provider::retry;
use crate::session::{CompactionAuthorship, Session, SessionStorage, SessionTreeEntry};
use crate::tool::{AgentToolResult, LocalToolContext, ToolState};
use crate::types::{
    AgentContext, AgentMessage, CacheRetention, ContentBlock, Model, StopReason, Usage,
};
use serde::Serialize;
use serde_json::Value as JsonValue;

/// The phases the harness can be in.
///
/// Structured operations (prompt, compact, retry) require the harness to
/// be in the Idle phase. Turn transitions happen internally.
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

/// Agent-level auto-retry policy for transient provider failures, mirroring
/// the TS `settings.retry` (`enabled` / `maxRetries` / `baseDelayMs`). The
/// initial call never counts as a retry; the per-attempt delay is
/// `baseDelayMs * 2^(attempt-1)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrySettings {
    pub enabled: bool,
    /// Max retry attempts (0 = no retries).
    pub max_retries: u32,
    /// Base delay in ms for the first retry.
    pub base_delay_ms: u64,
}

impl Default for RetrySettings {
    fn default() -> Self {
        RetrySettings {
            enabled: true,
            max_retries: 3,
            base_delay_ms: 2_000,
        }
    }
}

/// A retry lifecycle event, mirroring the TS `auto_retry_start` /
/// `auto_retry_end` session events.
#[derive(Debug, Clone)]
pub enum RetryEvent {
    /// A retry was scheduled: attempt `attempt` (1-indexed) retries the
    /// failed turn after `delay`, up to `max_attempts`.
    Start {
        attempt: u32,
        max_attempts: u32,
        delay: std::time::Duration,
        error_message: String,
    },
    /// The retry lifecycle ended: `success` when a retry turn completed,
    /// otherwise the failure that exhausted the budget (or a cancellation)
    /// as `final_error`.
    End {
        success: bool,
        attempt: u32,
        final_error: Option<String>,
    },
}

/// Hook points that consumers can register handlers for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookPoint {
    /// Before the agent starts processing a turn.
    BeforeAgentStart,
    /// Before the context is sent to the provider.
    BeforeProviderRequest,
    /// When a tool call is about to execute.
    ToolCall,
    /// After a tool result is received.
    ToolResult,
    /// Before the session is compacted.
    SessionBeforeCompact,
    /// After the session is compacted.
    SessionAfterCompact,
}

/// A hook handler receives context about the event and can mutate it.
pub type HookHandler = Arc<dyn Fn(HookContext) -> HookContext + Send + Sync>;

/// The typed `before_agent_start` hook event, mirroring the TS
/// `BeforeAgentStartEvent`. Serialized into [`HookContext::data`] so a handler
/// receives the TS-shaped fields rather than an ad-hoc payload. TS also
/// carries `images` and `resources`; neither exists on the Rust harness (no
/// image prompt input, no resource registry), so the payload omits them.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BeforeAgentStartEvent<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub prompt: &'a str,
    pub system_prompt: &'a str,
}

/// A before-compact hook's full compaction result, mirroring the TS
/// `CompactResult`. Persisted verbatim (`fromHook = true`) — the harness does
/// not fall back to its own cut analysis on any field, so a TS hook returning
/// a `CompactResult` migrates without behavioral drift. `summary`,
/// `tokens_before`, and `retained_tail` are required (the hook owns the full
/// result); an empty summary is refused before persisting. A hook that wants
/// to keep the harness-computed tail passes through `preparation.retained_tail`
/// — supplying `vec![]` deliberately erases it. The remaining optional fields
/// (`firstKeptEntryId`, `usage`, `details`) are persisted as supplied — `None`
/// means absent, not "compute for me".
///
/// `Default` is intentionally not derived: it would let a summary-only override
/// silently erase the retained tail and report `tokens_before: 0`, which is
/// exactly the partial-override footgun the full-result contract rules out.
/// Every field must be set explicitly at the construction site.
#[derive(Debug, Clone)]
pub struct BeforeCompactOverride {
    pub summary: String,
    pub first_kept_entry_id: Option<String>,
    pub tokens_before: u64,
    pub usage: Option<Usage>,
    pub retained_tail: Vec<AgentMessage>,
    pub details: Option<JsonValue>,
}

/// The typed `session_before_compact` hook event, mirroring the TS
/// `SessionBeforeCompactEvent`. Serialized into [`HookContext::data`] so a
/// handler receives the TS-shaped `preparation` and `branchEntries` rather
/// than an ad-hoc payload. The TS `signal: AbortSignal` has no Rust sync-hook
/// equivalent — the hook is a synchronous closure with nothing to observe —
/// and cancellation is expressed the other direction via the result's
/// `cancel` field ([`HookContext::with_cancel_compaction`]).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionBeforeCompactEvent<'a> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub preparation: &'a CompactionPreparation,
    pub branch_entries: &'a [SessionTreeEntry],
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom_instructions: Option<&'a str>,
}

/// Context passed to hook handlers.
///
/// Handlers return a (possibly mutated) copy; the harness threads selected
/// fields back into the loop. `agent_context` feeds the provider request,
/// `block_reason` gates a tool call, `tool_result` patches a tool result,
/// `cancel_compaction`/`compact_override` steer the compaction flow, and
/// `inject_messages`/`system_prompt_override` carry the `before_agent_start`
/// effects.
#[derive(Debug, Clone)]
pub struct HookContext {
    /// The hook point being triggered.
    pub hook: HookPoint,
    /// The current agent context (messages, model, etc.).
    pub agent_context: Option<AgentContext>,
    /// Arbitrary data attached to the hook event.
    pub data: serde_json::Value,
    /// `Some(reason)` at the `ToolCall` point blocks the call before it runs.
    pub block_reason: Option<String>,
    /// A replacement `AgentToolResult` at the `ToolResult` point; when set it
    /// supplants the result the tool produced.
    pub tool_result: Option<AgentToolResult>,
    /// At the `SessionBeforeCompact` point, aborts the compaction without
    /// persisting anything.
    pub cancel_compaction: bool,
    /// At the `SessionBeforeCompact` point, supplies the summary directly so
    /// the harness skips the summarization model call.
    pub compact_override: Option<BeforeCompactOverride>,
    /// At the `BeforeAgentStart` point, extra messages appended to the prompt
    /// batch after the user message; they enter the transcript and session
    /// like any prompt message.
    pub inject_messages: Vec<AgentMessage>,
    /// At the `BeforeAgentStart` point, the system prompt the run's initial
    /// context carries. Only that first context sees the override — steering
    /// and follow-up turns snapshot the agent's configured prompt again.
    pub system_prompt_override: Option<String>,
}

impl HookContext {
    pub fn new(hook: HookPoint) -> Self {
        HookContext {
            hook,
            agent_context: None,
            data: serde_json::Value::Null,
            block_reason: None,
            tool_result: None,
            cancel_compaction: false,
            compact_override: None,
            inject_messages: Vec::new(),
            system_prompt_override: None,
        }
    }

    pub fn with_context(mut self, ctx: AgentContext) -> Self {
        self.agent_context = Some(ctx);
        self
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = data;
        self
    }

    pub fn with_tool_result(mut self, result: AgentToolResult) -> Self {
        self.tool_result = Some(result);
        self
    }

    pub fn with_block_reason(mut self, reason: impl Into<String>) -> Self {
        self.block_reason = Some(reason.into());
        self
    }

    /// At `SessionBeforeCompact`, cancel the compaction without persisting.
    pub fn with_cancel_compaction(mut self) -> Self {
        self.cancel_compaction = true;
        self
    }

    /// At `SessionBeforeCompact`, supply the summary directly and skip the
    /// summarization model call. The persisted entry carries `fromHook`.
    pub fn with_compact_override(mut self, override_: BeforeCompactOverride) -> Self {
        self.compact_override = Some(override_);
        self
    }

    /// At `BeforeAgentStart`, append extra messages to the prompt batch,
    /// after the user message.
    pub fn with_inject_messages(mut self, messages: Vec<AgentMessage>) -> Self {
        self.inject_messages = messages;
        self
    }

    /// At `BeforeAgentStart`, replace the system prompt the run's initial
    /// context carries. The override does not stick: the next turn snapshots
    /// the agent's configured prompt again.
    pub fn with_system_prompt(mut self, system_prompt: impl Into<String>) -> Self {
        self.system_prompt_override = Some(system_prompt.into());
        self
    }
}

/// Consumer-plugged registry lookup resolving a session-carried model
/// reference into a concrete model. Returning `None` leaves the
/// construction-time model in place on restore.
pub type ModelResolver =
    Arc<dyn Fn(&crate::session::SessionModelRef) -> Option<Model> + Send + Sync>;

/// The orchestration layer wrapping the agent loop.
pub struct AgentHarness<S: SessionStorage> {
    agent: Agent,
    session: Session<S>,
    model: Model,
    phase: AgentHarnessPhase,
    compaction_settings: CompactionSettings,
    /// Timestamp of the latest compaction. Usage recorded at or before it
    /// measured a different message prefix, so it never anchors token
    /// estimates for the rewritten transcript. Recovered from the persisted
    /// session by [`AgentHarness::recover_boundary`].
    last_compaction_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Registered hook handlers, grouped by point. Shared via
    /// `Arc<Mutex<..>>` so the closures cloned into the agent's loop config
    /// read the live registration list — `on()` calls after construction
    /// still fire.
    hooks: Arc<Mutex<Vec<(HookPoint, HookHandler)>>>,
    /// The streaming function used for the summarization call in `compact()`.
    stream_fn: Arc<dyn StreamFn>,
    /// Entry ids aligned by index with the agent transcript. `None` marks a
    /// synthetic carrier (a compaction summary message, or a message folded
    /// into a compaction entry's retained tail on restore) that has no
    /// standalone `Message` entry. Drives the real `first_kept_entry_id`
    /// recorded by `compact()`.
    message_entry_ids: Vec<Option<String>>,
    /// The one-shot budget for overflow recovery: set when an overflow turn
    /// was compacted and retried, cleared by a new user prompt or any
    /// non-error assistant message. Mirrors the TS
    /// `_overflowRecoveryAttempted` flag — a context that stays oversized
    /// after one compact-and-retry surfaces its error instead of looping.
    overflow_recovery_attempted: bool,
    /// Auto-retry policy for transient provider failures.
    retry_settings: RetrySettings,
    /// Attempts used by the current auto-retry lifecycle; reset by any
    /// non-error assistant message. Mirrors TS `_retryAttempt`.
    retry_attempt: u32,
    /// Cancels the in-flight retry backoff sleep; [`AgentHarness::abort`]
    /// fires it. A fresh token arms each retry.
    retry_cancel: CancellationToken,
    /// Observer for the auto-retry lifecycle, mirroring TS `_emit` of
    /// `auto_retry_start` / `auto_retry_end`.
    retry_observer: Option<Arc<dyn Fn(RetryEvent) + Send + Sync>>,
    /// Every mounted tool, including ones the active selection currently
    /// hides from the model. The agent receives only the active subset.
    all_tools: Arc<[Arc<dyn crate::tool::AgentTool>]>,
    /// The tool subset the model sees, from the latest explicit selection
    /// (persisted as an `active_tools_change` entry); `None` mounts the full
    /// set.
    active_tool_names: Option<Vec<String>>,
    /// Resolves the model reference a restored session path carries. The
    /// crate stays registry-free, so the consumer plugs in its registry;
    /// without one a restore keeps the construction-time model.
    model_resolver: Option<ModelResolver>,
}

impl<S: SessionStorage> AgentHarness<S> {
    /// Create a new harness with the given session, model, and stream function.
    pub fn new(
        session: Session<S>,
        system_prompt: impl Into<String>,
        model: Model,
        stream_fn: Arc<dyn StreamFn>,
    ) -> Self {
        // Build the session-scoped execution context once and share it across
        // every turn: a real env + cwd + ToolState so fs/shell tools work
        // instead of panicking on `env()`.
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let env: Arc<dyn ExecutionEnv> = Arc::new(TokioExecutionEnv::new(cwd.clone()));
        let tool_state = Arc::new(ToolState::new());
        let tool_ctx: Arc<dyn crate::tool::ToolContext> =
            Arc::new(LocalToolContext::new(env, cwd, tool_state));
        let hooks: Arc<Mutex<Vec<(HookPoint, HookHandler)>>> = Arc::new(Mutex::new(Vec::new()));
        let mut agent = Agent::new(
            system_prompt,
            model.clone(),
            Arc::clone(&stream_fn),
            tool_ctx,
        );
        agent.set_loop_hooks(build_loop_hooks(Arc::clone(&hooks)));
        AgentHarness {
            agent,
            session,
            model,
            phase: AgentHarnessPhase::Idle,
            compaction_settings: CompactionSettings::default(),
            last_compaction_at: None,
            hooks,
            stream_fn,
            message_entry_ids: Vec::new(),
            overflow_recovery_attempted: false,
            retry_settings: RetrySettings::default(),
            retry_attempt: 0,
            retry_cancel: CancellationToken::new(),
            retry_observer: None,
            all_tools: Arc::from(Vec::new()),
            active_tool_names: None,
            model_resolver: None,
        }
    }

    /// Mount tools on the underlying agent.
    ///
    /// The harness keeps the full set; the agent receives only the active
    /// subset (all of them until [`AgentHarness::set_active_tools`] narrows
    /// the selection).
    pub fn with_tools(mut self, tools: Arc<[Arc<dyn crate::tool::AgentTool>]>) -> Self {
        self.all_tools = tools;
        self.apply_active_tools();
        self
    }

    /// Plug in the registry lookup used to resolve the model reference a
    /// restored session carries. Without a resolver, restore keeps the
    /// construction-time model.
    pub fn with_model_resolver(
        mut self,
        resolver: impl Fn(&crate::session::SessionModelRef) -> Option<Model> + Send + Sync + 'static,
    ) -> Self {
        self.model_resolver = Some(Arc::new(resolver));
        self
    }

    /// Current phase.
    pub fn phase(&self) -> AgentHarnessPhase {
        self.phase
    }

    /// The current model.
    pub fn model(&self) -> &Model {
        &self.model
    }

    /// Access the underlying session storage.
    pub fn session(&self) -> &Session<S> {
        &self.session
    }

    /// Access the agent.
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// Mutable access to the agent.
    pub fn agent_mut(&mut self) -> &mut Agent {
        &mut self.agent
    }

    /// Decoupled handle for mid-run control (steer/follow_up/abort).
    pub fn run_handle(&self) -> RunHandle {
        self.agent.run_handle()
    }

    /// Current compaction settings.
    pub fn compaction_settings(&self) -> &CompactionSettings {
        &self.compaction_settings
    }

    /// Update compaction settings.
    pub fn set_compaction_settings(&mut self, settings: CompactionSettings) {
        self.compaction_settings = settings;
    }

    /// Current auto-retry policy.
    pub fn retry_settings(&self) -> &RetrySettings {
        &self.retry_settings
    }

    /// Update the auto-retry policy.
    pub fn set_retry_settings(&mut self, settings: RetrySettings) {
        self.retry_settings = settings;
    }

    /// Attempts used by the current auto-retry lifecycle.
    pub fn retry_attempt(&self) -> u32 {
        self.retry_attempt
    }

    /// Observe the auto-retry lifecycle (`auto_retry_start`/`auto_retry_end`).
    pub fn on_auto_retry(&mut self, observer: impl Fn(RetryEvent) + Send + Sync + 'static) {
        self.retry_observer = Some(Arc::new(observer));
    }

    /// The active tool subset; `None` when the full mounted set is in play.
    pub fn active_tool_names(&self) -> Option<&[String]> {
        self.active_tool_names.as_deref()
    }

    /// Switch the model the next turn runs against, persisting a
    /// `model_change` entry so a later restore projects the same choice.
    pub async fn set_model(&mut self, model: Model) -> Result<(), anyhow::Error> {
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!(
                "Cannot set model while harness is in {:?} phase",
                self.phase
            );
        }
        self.session
            .append_model_change(&model.provider, &model.id)
            .await?;
        self.agent.set_model(model.clone());
        self.model = model;
        Ok(())
    }

    /// Narrow the tools the model sees to a subset of the mounted set,
    /// persisting an `active_tools_change` entry so a later restore projects
    /// the same selection.
    pub async fn set_active_tools(
        &mut self,
        active_tool_names: Vec<String>,
    ) -> Result<(), anyhow::Error> {
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!(
                "Cannot set active tools while harness is in {:?} phase",
                self.phase
            );
        }
        for name in &active_tool_names {
            if !self.all_tools.iter().any(|t| t.name() == name) {
                anyhow::bail!("Cannot activate unknown tool: {name}");
            }
        }
        self.session
            .append_active_tools_change(&active_tool_names)
            .await?;
        self.active_tool_names = Some(active_tool_names);
        self.apply_active_tools();
        Ok(())
    }

    /// Re-derive the agent's tool list from the mounted set and the active
    /// selection. The full set mounts when no selection ever narrowed it.
    fn apply_active_tools(&mut self) {
        let tools: Vec<Arc<dyn crate::tool::AgentTool>> = match &self.active_tool_names {
            Some(names) => self
                .all_tools
                .iter()
                .filter(|t| names.iter().any(|n| n == t.name()))
                .cloned()
                .collect(),
            None => self.all_tools.to_vec(),
        };
        self.agent.set_tools(tools.into());
    }

    /// Register a hook handler.
    pub fn on(&mut self, hook: HookPoint, handler: HookHandler) {
        self.hooks.lock().unwrap().push((hook, handler));
    }

    /// Run all registered hooks for a given point.
    fn run_hooks(&self, hook: HookPoint, mut ctx: HookContext) -> HookContext {
        let hooks = self.hooks.lock().unwrap();
        for (point, handler) in hooks.iter() {
            if *point == hook {
                ctx = handler(ctx);
            }
        }
        ctx
    }

    /// Send a user prompt and run the agent loop.
    ///
    /// Returns the messages produced during this turn. The messages are
    /// also persisted to the session.
    ///
    /// A turn ending in a transient provider failure is retried in place
    /// after an exponential backoff (`RetrySettings`, default 3 attempts at
    /// 2s doubling); a context-overflow error from the current model is
    /// instead compacted and retried once (`run_overflow_recovery`). Both
    /// drop the failed turn's terminal message from the transcript — it
    /// stays persisted — so the retry runs against the same context that
    /// preceded the failure. A settled turn whose context crossed the
    /// compaction threshold is compacted for the next turn without a retry;
    /// a failed maintenance compaction never turns the settled turn into an
    /// error. Messages queued during the run or any maintenance step are
    /// delivered before the call returns.
    pub async fn prompt(&mut self, text: &str) -> Result<Vec<AgentMessage>, anyhow::Error> {
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!("Cannot prompt while harness is in {:?} phase", self.phase);
        }

        self.phase = AgentHarnessPhase::Turn;
        // A new user message rearms the one-shot overflow recovery.
        self.overflow_recovery_attempted = false;

        // Run before-agent-start hooks. Their result steers the run: injected
        // messages extend the prompt batch after the user message, and a
        // system prompt override reaches the run's initial context only.
        let hook_ctx = self.run_hooks(
            HookPoint::BeforeAgentStart,
            HookContext::new(HookPoint::BeforeAgentStart).with_data(
                serde_json::to_value(BeforeAgentStartEvent {
                    kind: "before_agent_start",
                    prompt: text,
                    system_prompt: &self.agent.state().system_prompt,
                })
                .expect("BeforeAgentStartEvent serializes"),
            ),
        );

        let user_message = AgentMessage::User {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                signature: None,
            }],
            timestamp: chrono::Utc::now(),
        };
        let mut batch = vec![user_message];
        batch.extend(hook_ctx.inject_messages);

        let prior_system_prompt = self.agent.state().system_prompt.clone();
        if let Some(override_) = &hook_ctx.system_prompt_override {
            self.agent.set_system_prompt(override_.clone());
        }
        let result = self.agent.prompt_messages(&batch).await;
        if hook_ctx.system_prompt_override.is_some() {
            self.agent.set_system_prompt(prior_system_prompt);
        }

        match result {
            Ok(messages) => {
                self.phase = AgentHarnessPhase::Idle;
                self.persist_turn_messages(&messages).await?;
                self.note_run_outcome(&messages);

                let mut all_messages = messages;
                all_messages.extend(self.settle_after_run().await?);
                Ok(all_messages)
            }
            Err(e) => {
                self.phase = AgentHarnessPhase::Idle;
                Err(e)
            }
        }
    }

    /// Continue from the current transcript.
    ///
    /// The same post-run maintenance as [`AgentHarness::prompt`] applies:
    /// transient provider errors are auto-retried after a backoff, an
    /// overflow terminal from the current model is compacted and retried
    /// once per error episode, a settled turn over the compaction threshold
    /// is compacted for the next turn without a retry, and queued messages
    /// are delivered before the call returns.
    pub async fn continue_(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!("Cannot continue while harness is in {:?} phase", self.phase);
        }

        self.phase = AgentHarnessPhase::Turn;
        let result = self.agent.continue_().await;

        match result {
            Ok(messages) => {
                self.phase = AgentHarnessPhase::Idle;
                self.persist_turn_messages(&messages).await?;
                self.note_run_outcome(&messages);

                let mut all_messages = messages;
                all_messages.extend(self.settle_after_run().await?);
                Ok(all_messages)
            }
            Err(e) => {
                self.phase = AgentHarnessPhase::Idle;
                Err(e)
            }
        }
    }

    /// Persist a finished run's messages to the session, tracking the entry
    /// id of each so [`AgentHarness::compact`] can record the real first-kept
    /// entry. A mid-batch persistence failure reverts the transcript to the
    /// persisted session before the error surfaces.
    async fn persist_turn_messages(
        &mut self,
        messages: &[AgentMessage],
    ) -> Result<(), anyhow::Error> {
        for msg in messages {
            match self.session.append_message(msg.clone()).await {
                Ok(id) => self.message_entry_ids.push(Some(id)),
                Err(e) => return Err(self.revert_transcript_after_persist_failure(e).await),
            }
        }
        Ok(())
    }

    /// Rearm the one-shot overflow recovery and close out an in-progress
    /// retry lifecycle when a run produced any non-error assistant message —
    /// both budgets apply per error episode, and a completed assistant reply
    /// ends the episode.
    fn note_run_outcome(&mut self, messages: &[AgentMessage]) {
        let succeeded = messages.iter().any(|m| {
            matches!(
                m,
                AgentMessage::Assistant { stop_reason, .. } if *stop_reason != Some(StopReason::Error)
            )
        });
        if succeeded {
            self.overflow_recovery_attempted = false;
            if self.retry_attempt > 0 {
                let attempt = self.retry_attempt;
                self.retry_attempt = 0;
                self.emit_retry(RetryEvent::End {
                    success: true,
                    attempt,
                    final_error: None,
                });
            }
        }
    }

    /// Drive the overflow → compact → retry loop after a finished run.
    ///
    /// Each iteration inspects the transcript's last message; a retryable
    /// overflow runs one compact-and-retry and the loop re-examines the
    /// retry's outcome, so a second overflow (recovery budget spent) or a
    /// clean reply ends the loop. Returns every message the retry turns
    /// produced, oldest first.
    async fn run_overflow_recovery(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        let mut produced = Vec::new();
        while let Some(retry_messages) = self.recover_overflow_once().await? {
            self.persist_turn_messages(&retry_messages).await?;
            self.note_run_outcome(&retry_messages);
            produced.extend(retry_messages);
        }
        Ok(produced)
    }

    /// Post-run maintenance shared by `prompt` and `continue_`, mirroring the
    /// TS `while (handlePostAgentRun()) continue()` loop: agent-level
    /// auto-retry of retryable provider errors first, then overflow
    /// compact-and-retry, then threshold compaction. Any messages still
    /// queued — from `agent_end` listeners or while a maintenance step ran —
    /// are delivered by one continuation, and that continuation's own
    /// outcome settles through another iteration. A settled turn with no
    /// pending work exits the loop.
    async fn settle_after_run(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        let mut produced = Vec::new();
        loop {
            if self.prepare_auto_retry().await? {
                produced.extend(self.run_continuation_turn().await?);
                continue;
            }
            // An in-progress retry lifecycle closes out when the latest
            // failure is not retryable or the budget is spent — TS emits
            // `auto_retry_end` here, before the compaction path.
            if self.retry_attempt > 0
                && let Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    error_message,
                    ..
                }) = self.agent.state().messages.last()
            {
                let attempt = self.retry_attempt;
                self.retry_attempt = 0;
                self.emit_retry(RetryEvent::End {
                    success: false,
                    attempt,
                    final_error: error_message.clone(),
                });
            }
            produced.extend(self.run_overflow_recovery().await?);
            produced.extend(self.run_threshold_compaction().await?);
            if !self.agent.has_queued_messages() {
                return Ok(produced);
            }
            // Deliver messages queued while the run or a maintenance step
            // was in flight. One continuation drains them; its own outcome
            // settles through the next iteration.
            produced.extend(self.run_continuation_turn().await?);
        }
    }

    /// Threshold compaction after a settled run: the turn already completed,
    /// so the conversation is compacted for the next turn and never
    /// retried. A failed maintenance compaction only logs: the settled turn
    /// keeps its result, and the next turn's settle re-attempts the
    /// compaction. Queued-message delivery happens in the outer
    /// [`AgentHarness::settle_after_run`] loop, not here.
    async fn run_threshold_compaction(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        // A user-aborted turn stays exactly where the user stopped it.
        if matches!(
            self.agent.state().messages.last(),
            Some(AgentMessage::Assistant {
                stop_reason: Some(StopReason::Aborted),
                ..
            })
        ) {
            return Ok(Vec::new());
        }
        if !self.needs_compaction() {
            return Ok(Vec::new());
        }
        match self.compact(None).await {
            Ok(_) => {}
            Err(e) if e.downcast_ref::<compaction::NothingToCompact>().is_some() => {}
            Err(e) => {
                tracing::warn!("threshold compaction failed: {e:#}");
            }
        }
        Ok(Vec::new())
    }

    /// Start the auto-retry of a retryable provider error when the transcript
    /// ends on one and the retry budget is open: increment the attempt
    /// counter, emit `auto_retry_start`, drop the failed turn's terminal
    /// message from the transcript (the session keeps it), and sleep the
    /// exponential backoff. Returns whether the caller should run the retry
    /// turn. An abort during the backoff closes the lifecycle with
    /// `Retry cancelled` and returns false, mirroring TS `_prepareRetry`.
    /// Context overflow is never retried here — the overflow path owns it.
    async fn prepare_auto_retry(&mut self) -> Result<bool, anyhow::Error> {
        let settings = self.retry_settings;
        if !settings.enabled || self.retry_attempt >= settings.max_retries {
            return Ok(false);
        }
        let Some(message) = self.agent.state().messages.last() else {
            return Ok(false);
        };
        if crate::provider::overflow::is_context_overflow(message, self.model.context_window as u64)
            || !retry::is_retryable_assistant_error(message)
        {
            return Ok(false);
        }
        let AgentMessage::Assistant { error_message, .. } = message else {
            return Ok(false);
        };

        self.retry_attempt += 1;
        let delay = std::time::Duration::from_millis(
            settings.base_delay_ms * 2u64.pow(self.retry_attempt - 1),
        );
        self.emit_retry(RetryEvent::Start {
            attempt: self.retry_attempt,
            max_attempts: settings.max_retries,
            delay,
            error_message: error_message
                .clone()
                .unwrap_or_else(|| "Unknown error".into()),
        });

        // The failed turn's terminal message must not reach the retry's
        // context; the session keeps it for history.
        let mut messages = self.agent.state().messages.clone();
        messages.pop();
        self.agent.replace_transcript(messages);
        self.message_entry_ids.pop();

        let cancel = CancellationToken::new();
        self.retry_cancel = cancel.clone();
        let slept = tokio::select! {
            _ = cancel.cancelled() => false,
            _ = tokio::time::sleep(delay) => true,
        };
        if !slept {
            let attempt = self.retry_attempt;
            self.retry_attempt = 0;
            self.emit_retry(RetryEvent::End {
                success: false,
                attempt,
                final_error: Some("Retry cancelled".into()),
            });
        }
        Ok(slept)
    }

    /// One continuation turn over the current transcript — the retry turn
    /// after a dropped error message, or a drain of messages queued by
    /// `agent_end` listeners or a maintenance step. Messages persist and
    /// settle like any other turn.
    async fn run_continuation_turn(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        self.phase = AgentHarnessPhase::Turn;
        let result = self.agent.continue_().await;
        self.phase = AgentHarnessPhase::Idle;
        let messages = result?;
        self.persist_turn_messages(&messages).await?;
        self.note_run_outcome(&messages);
        Ok(messages)
    }

    fn emit_retry(&self, event: RetryEvent) {
        if let Some(observer) = &self.retry_observer {
            observer(event);
        }
    }

    /// One overflow recovery step over the finished run.
    ///
    /// Returns `None` when the transcript does not end in a recoverable
    /// overflow — including the terminal states TS settles on: a context
    /// compaction cannot shrink further ([`compaction::NothingToCompact`]),
    /// or the recovery budget for this error episode is already spent.
    /// Otherwise drops the failed turn's terminal message from the
    /// transcript (it stays persisted, mirroring TS), compacts, runs one
    /// retry turn, and returns the retry's messages.
    async fn recover_overflow_once(&mut self) -> Result<Option<Vec<AgentMessage>>, anyhow::Error> {
        let Some(will_retry) = self.overflow_will_retry() else {
            return Ok(None);
        };

        if !will_retry {
            // A completed answer that silently exceeded the window: compact
            // for the next turn. The turn itself cannot be retried — the
            // transcript ends on its assistant reply, which `continue_`
            // refuses.
            return match self.compact(None).await {
                Ok(_) => Ok(None),
                Err(e) if e.downcast_ref::<compaction::NothingToCompact>().is_some() => Ok(None),
                Err(e) => Err(e),
            };
        }

        if self.overflow_recovery_attempted {
            return Ok(None);
        }
        self.overflow_recovery_attempted = true;

        // The failed turn's terminal message must not reach the retry's
        // context; the session keeps it for history.
        let mut messages = self.agent.state().messages.clone();
        debug_assert!(
            matches!(messages.last(), Some(AgentMessage::Assistant { .. })),
            "overflow check passed on the last message"
        );
        messages.pop();
        self.agent.replace_transcript(messages);
        self.message_entry_ids.pop();

        match self.compact(None).await {
            Ok(_) => {}
            Err(e) if e.downcast_ref::<compaction::NothingToCompact>().is_some() => {
                return Ok(None);
            }
            Err(e) => return Err(e),
        }

        self.phase = AgentHarnessPhase::Turn;
        let result = self.agent.continue_().await;
        self.phase = AgentHarnessPhase::Idle;
        Ok(Some(result?))
    }

    /// Whether the transcript's last message is a context overflow from the
    /// current model, and whether the turn may be retried after compaction.
    ///
    /// The guards mirror TS: an aborted message never triggers recovery, an
    /// error attributed to a different model is not this model's overflow,
    /// and a message recorded at or before the latest compaction measured a
    /// context that no longer exists. A completed (`Stop`) answer compacts
    /// but cannot retry.
    fn overflow_will_retry(&self) -> Option<bool> {
        let message = self.agent.state().messages.last()?;
        let AgentMessage::Assistant {
            stop_reason,
            provider,
            model,
            timestamp,
            ..
        } = message
        else {
            return None;
        };
        if *stop_reason == Some(StopReason::Aborted) {
            return None;
        }
        if provider != &self.model.provider || model != &self.model.id {
            return None;
        }
        if self.last_compaction_at.is_some_and(|at| timestamp <= &at) {
            return None;
        }
        if crate::provider::overflow::is_context_overflow(message, self.model.context_window as u64)
        {
            Some(*stop_reason != Some(StopReason::Stop))
        } else {
            None
        }
    }

    /// Abort the current agent run and any in-flight retry backoff.
    pub fn abort(&mut self) {
        self.retry_cancel.cancel();
        self.agent.abort();
        self.phase = AgentHarnessPhase::Idle;
    }

    /// Reset the agent's transcript and queues.
    pub fn reset(&mut self) {
        self.agent.reset();
        self.message_entry_ids.clear();
        self.phase = AgentHarnessPhase::Idle;
    }

    /// Load the compaction boundary from the persisted session.
    ///
    /// A harness constructed over an existing session must call this before
    /// relying on [`AgentHarness::needs_compaction`]; a fresh session holds
    /// no compaction entries and the boundary stays unset. Only the boundary
    /// is recovered — [`AgentHarness::restore`] rebuilds the transcript too.
    pub async fn recover_boundary(&mut self) -> Result<(), anyhow::Error> {
        self.last_compaction_at = self.session.latest_compaction_timestamp().await?;
        Ok(())
    }

    /// Rebuild the agent transcript from the persisted session.
    ///
    /// Every message-producing entry variant on the active path projects into
    /// the transcript: messages verbatim, custom messages as `Custom`,
    /// branch/compaction summaries as their tagged user-text carriers. The
    /// kept segment behind a compaction boundary is reconstructed by walking
    /// the tree from its `first_kept_entry_id`, never from the boundary
    /// itself. The reasoning tier the path carries is applied to the agent,
    /// the active tool selection narrows the mounted set, and the model the
    /// path carries is applied when a model resolver is plugged in —
    /// resolving it needs the provider registry, which lives at the facade
    /// layer. Restore never appends entries: the session already records
    /// these choices. The compaction boundary used by token estimation is
    /// recovered alongside.
    pub async fn restore(&mut self) -> Result<(), anyhow::Error> {
        let context = self.session.build_session_context().await?;
        self.agent.clear_transcript_state();
        self.agent.replace_transcript(context.messages);
        self.agent.set_thinking_level(context.thinking_level);
        self.active_tool_names = context.active_tool_names;
        self.apply_active_tools();
        if let (Some(resolver), Some(model_ref)) = (&self.model_resolver, &context.model)
            && let Some(model) = resolver(model_ref)
        {
            self.agent.set_model(model.clone());
            self.model = model;
        }
        self.message_entry_ids = context.message_entry_ids;
        self.recover_boundary().await?;
        Ok(())
    }

    /// Reconcile the agent with the session after a turn's persistence
    /// failed partway. The session is the durable record, so the transcript
    /// is rebuilt from it: both views then hold exactly the persisted prefix,
    /// which is also what any later [`AgentHarness::restore`] produces.
    async fn revert_transcript_after_persist_failure(
        &mut self,
        persist_error: anyhow::Error,
    ) -> anyhow::Error {
        match self.restore().await {
            Ok(()) => anyhow::anyhow!(
                "failed to persist messages: {persist_error:#}; \
                 transcript reverted to the persisted session"
            ),
            Err(revert_error) => anyhow::anyhow!(
                "failed to persist messages: {persist_error:#}; \
                 reverting the transcript to the persisted session also failed: {revert_error:#}"
            ),
        }
    }

    /// Check whether compaction is needed based on current context size.
    pub fn needs_compaction(&self) -> bool {
        let tokens = self.estimate_current_tokens();
        compaction::should_compact(
            tokens,
            self.model.context_window as u64,
            &self.compaction_settings,
        )
    }

    /// Estimate the current token usage of the conversation.
    ///
    /// An assistant usage block only anchors the estimate when it was
    /// recorded after the latest compaction; anything older measured the
    /// pre-compaction prefix and the whole transcript falls back to the
    /// character heuristic.
    fn estimate_current_tokens(&self) -> u64 {
        let messages = &self.agent.state().messages;
        let estimate = compaction::estimate_context_tokens(messages);
        let stale_anchor = match (estimate.last_usage_index, self.last_compaction_at) {
            (Some(i), Some(at)) => messages[i].timestamp() <= at,
            _ => false,
        };
        if stale_anchor {
            messages.iter().map(compaction::estimate_tokens).sum()
        } else {
            estimate.tokens
        }
    }

    /// Run compaction on the current conversation.
    ///
    /// Finds the cut point, asks the model (via the harness stream function)
    /// to summarize the compacted prefix, and persists a `Compaction` entry
    /// carrying the real first-kept entry id, the pre-compaction token count,
    /// and the summarization usage. The retained tail is an in-memory flow
    /// value only: the session reconstructs it by walking the tree from the
    /// first-kept entry id. The agent transcript is rewritten to the summary
    /// message plus the retained tail. A
    /// `session_before_compact` hook receives the typed [`CompactionPreparation`]
    /// and the session branch entries, and may cancel or supply a full
    /// [`BeforeCompactOverride`] persisted verbatim. `custom_instructions`
    /// mirrors the TS `compact(customInstructions?)` argument and is surfaced
    /// on the hook event.
    ///
    /// A transcript whose summarizable range is empty — everything fits in the
    /// keep-recent window, or is already folded into the latest boundary's
    /// summary — is refused with [`compaction::NothingToCompact`] before the
    /// phase changes, any hook fires, or the model is called; the session and
    /// transcript stay untouched.
    ///
    /// Only the transcript is rebuilt; the steering and follow-up queues are
    /// user input and survive compaction. A full clear of both transcript
    /// and queues is [`AgentHarness::reset`].
    pub async fn compact(
        &mut self,
        custom_instructions: Option<&str>,
    ) -> Result<CompactionResult, anyhow::Error> {
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!("Cannot compact while harness is in {:?} phase", self.phase);
        }
        if self.agent.state().messages.is_empty() {
            anyhow::bail!("Cannot compact an empty transcript");
        }

        // The session branch the harness is compacting — the same entries TS
        // exposes as `branchEntries` on the `session_before_compact` event:
        // the full path to the root, across compaction boundaries.
        let branch_entries = self.session.get_branch().await?;

        let messages = self.agent.state().messages.clone();
        let tokens_before = compaction::estimate_context_tokens(&messages).tokens;
        let cut_point =
            compaction::find_cut_point(&messages, self.compaction_settings.keep_recent_tokens);
        let kept = &messages[cut_point..];
        let first_kept_entry_id = self.message_entry_ids.get(cut_point).cloned().flatten();

        // The preparation doubles as the emptiness guard: an empty
        // summarizable range is refused here — before the phase change, the
        // hook, and the model call — mirroring TS, where `prepareCompaction`
        // returning `undefined` ends the attempt with "Nothing to compact".
        let preparation = match compaction::build_preparation(
            &branch_entries,
            &messages,
            cut_point,
            first_kept_entry_id.clone(),
            tokens_before,
            &self.compaction_settings,
        ) {
            Some(p) => p,
            None => return Err(compaction::NothingToCompact.into()),
        };

        self.phase = AgentHarnessPhase::Compaction;

        // The hook fires after the cut analysis — mirroring TS, which prepares
        // the compaction then emits the event with `preparation` +
        // `branchEntries` — so the handler decides on the specific content.
        // The typed event carries the full TS `CompactionPreparation`
        // (split-turn is always false here) plus the session branch and custom
        // instructions, rather than a trimmed ad-hoc payload.
        let event = SessionBeforeCompactEvent {
            kind: "session_before_compact",
            preparation: &preparation,
            branch_entries: &branch_entries,
            custom_instructions,
        };
        let hook_ctx = self.run_hooks(
            HookPoint::SessionBeforeCompact,
            HookContext::new(HookPoint::SessionBeforeCompact)
                .with_data(serde_json::to_value(&event).unwrap_or(serde_json::Value::Null)),
        );
        if hook_ctx.cancel_compaction {
            self.phase = AgentHarnessPhase::Idle;
            anyhow::bail!("compaction cancelled by before-compact hook");
        }

        // Resolve the compaction result. A hook override supplies a full
        // TS-shaped `CompactResult`, persisted verbatim (`fromHook = true`) —
        // no field falls back to the harness's cut analysis. Otherwise the
        // harness summarizes the prefix itself, consuming the preparation
        // (previous summary folded into the prompt, file ops folded into the
        // summary text and `details`). An empty summary never persists — it
        // would discard the compacted history; the model path bails inside
        // `summarize_via_model`, the hook path is refused here.
        let (
            summary_text,
            first_kept_entry_id,
            tokens_before,
            usage,
            details,
            retained_tail,
            from_hook,
        ) = match hook_ctx.compact_override {
            Some(o) => {
                if o.summary.trim().is_empty() {
                    self.phase = AgentHarnessPhase::Idle;
                    anyhow::bail!("before-compact hook supplied an empty summary");
                }
                let retained_tail = o.retained_tail;
                (
                    o.summary,
                    o.first_kept_entry_id,
                    o.tokens_before,
                    o.usage,
                    o.details,
                    retained_tail,
                    true,
                )
            }
            None => {
                let (summary, usage, details) = self
                    .summarize_via_model(&preparation, custom_instructions)
                    .await?;
                (
                    summary,
                    first_kept_entry_id,
                    tokens_before,
                    usage,
                    details,
                    kept.to_vec(),
                    false,
                )
            }
        };

        let authorship = CompactionAuthorship {
            details: details.clone(),
            from_hook,
        };

        // Persist the boundary first — the session is the durable record and
        // a failure here leaves the agent transcript untouched. The retained
        // tail persists with it, so a restore reads the same messages the
        // transcript is about to be rebuilt from.
        let boundary = match self
            .session
            .append_compaction(
                &summary_text,
                first_kept_entry_id.clone(),
                tokens_before,
                usage.clone(),
                authorship,
                Some(retained_tail.clone()),
            )
            .await
        {
            Ok((_id, timestamp)) => timestamp,
            Err(e) => {
                self.phase = AgentHarnessPhase::Idle;
                return Err(e);
            }
        };

        // Rebuild the transcript: summary as context + the retained tail. The
        // summary message carries the boundary instant, so a transcript
        // rebuilt from storage equals this one exactly.
        let mut new_messages = Vec::with_capacity(retained_tail.len() + 1);
        new_messages.push(crate::session::compaction_summary_message(
            &summary_text,
            boundary,
        ));
        new_messages.extend_from_slice(&retained_tail);

        self.agent.clear_transcript_state();
        self.agent.replace_transcript(new_messages);
        // The summary is synthetic (no entry id). A hook-supplied tail carries
        // unknown entry ids (the hook's messages need not be the harness's
        // persisted entries); the harness-computed tail retains the ids it
        // walked from the session, padding with `None` where the transcript
        // has no persisted entries (e.g. rebuilt via `replace_transcript`).
        let tail_len = retained_tail.len();
        let mut new_ids: Vec<Option<String>> = Vec::with_capacity(tail_len + 1);
        new_ids.push(None);
        if from_hook {
            new_ids.extend((0..tail_len).map(|_| None));
        } else {
            let mut known = self
                .message_entry_ids
                .get(cut_point..)
                .map(|s| s.to_vec())
                .unwrap_or_default();
            known.resize(tail_len, None);
            new_ids.extend(known);
        }
        self.message_entry_ids = new_ids;

        self.last_compaction_at = Some(boundary);
        let tokens_after = self.estimate_current_tokens();

        let result = CompactionResult {
            summary: summary_text,
            first_kept_entry_id,
            tokens_before,
            tokens_after,
            usage,
            details,
            retained_tail,
        };

        // Run after-compact hooks.
        let _hook_ctx = self.run_hooks(
            HookPoint::SessionAfterCompact,
            HookContext::new(HookPoint::SessionAfterCompact).with_data(serde_json::json!({
                "tokens_before": tokens_before,
                "tokens_after": tokens_after,
                "cut_point": cut_point,
            })),
        );

        self.phase = AgentHarnessPhase::Idle;
        Ok(result)
    }

    /// Summarize the compacted prefix via the harness stream function.
    ///
    /// Consumes the [`CompactionPreparation`]: `previous_summary` is folded
    /// into the summarization prompt, and the computed file lists are appended
    /// to the summary text and returned as `details` — mirroring TS `compact`.
    /// A terminal `Error`/`Aborted` stop reason or an empty summary bails before
    /// anything is persisted so the transcript and session stay intact.
    async fn summarize_via_model(
        &mut self,
        preparation: &CompactionPreparation,
        custom_instructions: Option<&str>,
    ) -> Result<(String, Option<Usage>, Option<JsonValue>), anyhow::Error> {
        let prompt = compaction::build_compaction_prompt(
            &preparation.messages_to_summarize,
            preparation.previous_summary.as_deref(),
            custom_instructions,
        );
        let summary_context = AgentContext {
            system_prompt: compaction::SUMMARIZATION_SYSTEM_PROMPT.into(),
            messages: vec![AgentMessage::user(prompt)],
            tools: Arc::from(Vec::new()),
            model: self.model.clone(),
            thinking_level: None,
            cache_retention: CacheRetention::None,
            session_id: None,
            metadata: Default::default(),
        };
        let signal = CancellationToken::new();
        let (event_tx, mut event_rx) = mpsc::channel::<crate::types::AgentEvent>(64);
        // Run the summarization stream concurrently with draining its events:
        // the producer would block on the 64-cap channel once it fills, so the
        // receiver must drain while it runs, not after.
        let stream_fn = Arc::clone(&self.stream_fn);
        let stream_handle =
            tokio::spawn(async move { stream_fn.stream(&summary_context, signal, event_tx).await });
        // The harness does not surface summarization events; just keep the
        // channel empty so the producer never blocks.
        while event_rx.recv().await.is_some() {}
        let summary_response = match stream_handle.await {
            Ok(Ok(m)) => m,
            Ok(Err(e)) => {
                self.phase = AgentHarnessPhase::Idle;
                return Err(e);
            }
            Err(join_err) => {
                self.phase = AgentHarnessPhase::Idle;
                return Err(anyhow::Error::new(join_err));
            }
        };

        let (summary_text, usage) = extract_summary(&summary_response);
        let failed = match &summary_response {
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Error | StopReason::Aborted),
                ..
            } => true,
            _ => summary_text.trim().is_empty(),
        };
        if failed {
            self.phase = AgentHarnessPhase::Idle;
            let label = match &summary_response {
                AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                } => "error",
                AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Aborted),
                    ..
                } => "aborted",
                _ => "no summary",
            };
            let detail = match &summary_response {
                AgentMessage::Assistant {
                    error_message: Some(msg),
                    ..
                } => format!(": {msg}"),
                _ => String::new(),
            };
            return Err(anyhow::anyhow!("summarization failed ({label}){detail}"));
        }
        let (read_files, modified_files) = compaction::compute_file_lists(&preparation.file_ops);
        let block = compaction::format_file_operations(&read_files, &modified_files);
        let summary_text = format!("{summary_text}{block}");
        let details = serde_json::json!({
            "readFiles": read_files,
            "modifiedFiles": modified_files,
        });
        Ok((summary_text, usage, Some(details)))
    }

    /// Build a compaction prompt for the current conversation.
    ///
    /// Returns the prompt that should be sent to the LLM to generate a
    /// summary, along with the cut point index. `None` when the summarizable
    /// range is empty — everything fits in the keep-recent window, or only
    /// the leading summary carrier would be cut.
    pub fn build_compaction_prompt(&self) -> Option<(String, usize)> {
        let messages = self.agent.state().messages.clone();
        if messages.is_empty() {
            return None;
        }

        let cut_point =
            compaction::find_cut_point(&messages, self.compaction_settings.keep_recent_tokens);

        // A leading compaction-summary carrier is folded into the
        // summarization as `previous_summary`, never re-summarized — the same
        // exclusion `build_preparation` derives from the session branch.
        let start = usize::from(messages.first().is_some_and(|m| {
            matches!(m, AgentMessage::User { content, .. }
                if matches!(content.first(), Some(ContentBlock::Text { text, .. }) if text.starts_with(crate::session::COMPACTION_SUMMARY_PREFIX)))
        }));
        if cut_point <= start {
            return None; // Nothing to compact.
        }

        let compacted = &messages[start..cut_point];
        let prompt = compaction::build_compaction_prompt(compacted, None, None);
        Some((prompt, cut_point))
    }
}

/// Build the loop-config observation closures that route the harness's
/// registered hooks into the agent loop. Each closure clones the shared hook
/// list so it sees handlers registered via `on()` after the harness is built.
fn build_loop_hooks(hooks: Arc<Mutex<Vec<(HookPoint, HookHandler)>>>) -> LoopHooks {
    let provider = Arc::clone(&hooks);
    let before_provider_request: BeforeProviderRequestHook = Arc::new(move |ctx: &AgentContext| {
        let mut hc = HookContext::new(HookPoint::BeforeProviderRequest).with_context(ctx.clone());
        let list = provider.lock().unwrap();
        for (point, handler) in list.iter() {
            if *point == HookPoint::BeforeProviderRequest {
                hc = handler(hc);
            }
        }
        drop(list);
        // A handler that replaced the context wins; otherwise the original
        // context flows through unchanged.
        hc.agent_context.unwrap_or_else(|| ctx.clone())
    });

    let tool = Arc::clone(&hooks);
    let before_tool_call: BeforeToolCallHook =
        Arc::new(move |id: &str, name: &str, args: &JsonValue| {
            let mut hc = HookContext::new(HookPoint::ToolCall).with_data(serde_json::json!({
                "tool_call_id": id,
                "tool_name": name,
                "args": args.clone(),
            }));
            let list = tool.lock().unwrap();
            for (point, handler) in list.iter() {
                if *point == HookPoint::ToolCall {
                    hc = handler(hc);
                }
            }
            drop(list);
            hc.block_reason
        });

    let result = Arc::clone(&hooks);
    let after_tool_call: AfterToolCallHook = Arc::new(move |r: &AgentToolResult| {
        let mut hc = HookContext::new(HookPoint::ToolResult)
            .with_data(serde_json::json!({
                "is_error": r.is_error,
            }))
            .with_tool_result(r.clone());
        let list = result.lock().unwrap();
        for (point, handler) in list.iter() {
            if *point == HookPoint::ToolResult {
                hc = handler(hc);
            }
        }
        drop(list);
        hc.tool_result.unwrap_or_else(|| r.clone())
    });

    LoopHooks {
        before_provider_request: Some(before_provider_request),
        before_tool_call: Some(before_tool_call),
        after_tool_call: Some(after_tool_call),
    }
}

/// Pull the summary text and token usage out of the summarization response.
///
/// Only a completed assistant turn carries trustworthy usage; an unfinished
/// or non-assistant response contributes no usage anchor.
fn extract_summary(message: &AgentMessage) -> (String, Option<Usage>) {
    match message {
        AgentMessage::Assistant {
            content,
            usage,
            stop_reason: Some(_),
            ..
        } => {
            let text = content
                .iter()
                .find_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            (text, Some((**usage).clone()))
        }
        _ => (String::new(), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionStorage;
    use crate::session::SessionTreeEntry;
    use crate::types::{AgentEvent, ContentBlock, StopReason, ThinkingKind, Usage};
    use tokio_util::sync::CancellationToken;

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

    /// A stream whose summarization response is an `Aborted` terminal — the
    /// shape a cancelled or failed compaction call produces.
    struct AbortedSummaryStreamFn;

    #[async_trait::async_trait]
    impl StreamFn for AbortedSummaryStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            Ok(AgentMessage::Assistant {
                content: vec![],
                model: "test".into(),
                provider: "test".into(),
                api: "test".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(StopReason::Aborted),
                usage: Box::new(Usage::default()),
                error_message: Some("summarization was cancelled".into()),
                timestamp: chrono::Utc::now(),
            })
        }
    }

    /// A stream that emits more events than the channel capacity before
    /// returning, exposing a deadlock if the harness drains only after the
    /// producer returns.
    struct ChattyStreamFn;

    #[async_trait::async_trait]
    impl StreamFn for ChattyStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            for i in 0..200u32 {
                let _ = event_tx
                    .send(AgentEvent::MessageUpdate {
                        message: Box::new(AgentMessage::user(format!("chunk {i}"))),
                        assistant_message_event: crate::types::AssistantMessageEvent::TextDelta {
                            content_index: 0,
                            delta: format!("chunk {i}"),
                        },
                    })
                    .await;
            }
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "summary".into(),
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
            context_window: 100_000,
            max_tokens: 8_192,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        }
    }

    /// The model a session `model_change` resolves to in tests, distinct
    /// from [`test_model`] so a restore's model application is observable.
    fn resolved_model() -> Model {
        Model {
            provider: "anthropic".into(),
            id: "claude-opus".into(),
            context_window: 200_000,
            max_tokens: 16_384,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        }
    }

    /// A resolver for the `anthropic/claude-opus` reference the test
    /// sessions carry.
    fn test_model_resolver()
    -> impl Fn(&crate::session::SessionModelRef) -> Option<Model> + Send + Sync + 'static {
        |mref: &crate::session::SessionModelRef| {
            (mref.provider == "anthropic" && mref.model_id == "claude-opus").then(resolved_model)
        }
    }

    /// A transcript `find_cut_point` splits mid-way under
    /// [`compact_test_settings`]: four messages of ~100 estimated tokens
    /// each, so the 150-token keep-recent budget retains only the trailing
    /// assistant and leaves a three-message prefix to summarize. The
    /// assistants' 90_000-token usage anchors `needs_compaction` against the
    /// 100_000-token test model.
    fn compactable_transcript() -> Vec<AgentMessage> {
        let long = "x".repeat(400);
        let assistant = |text: String| AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text,
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
            usage: Box::new(Usage {
                total_tokens: 90_000,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        vec![
            AgentMessage::user(long.clone()),
            assistant(long.clone()),
            AgentMessage::user(long.clone()),
            assistant(long),
        ]
    }

    /// The keep-recent budget that cuts [`compactable_transcript`] after the
    /// second user message.
    fn compact_test_settings() -> CompactionSettings {
        CompactionSettings {
            keep_recent_tokens: 150,
            ..Default::default()
        }
    }

    // In-memory session storage for testing.
    struct MemStorage {
        entries: std::sync::Mutex<Vec<SessionTreeEntry>>,
        leaf_id: std::sync::Mutex<Option<String>>,
        /// Number of `append_entry` calls so far.
        append_calls: std::sync::Mutex<u64>,
        /// Call number at which `append_entry` fails; `u64::MAX` means never.
        fail_at_call: std::sync::Mutex<u64>,
    }

    impl MemStorage {
        fn new() -> Self {
            MemStorage {
                entries: std::sync::Mutex::new(Vec::new()),
                leaf_id: std::sync::Mutex::new(None),
                append_calls: std::sync::Mutex::new(0),
                fail_at_call: std::sync::Mutex::new(u64::MAX),
            }
        }
    }

    #[async_trait::async_trait]
    impl SessionStorage for MemStorage {
        async fn create_entry_id(&self) -> Result<String, anyhow::Error> {
            Ok(uuid::Uuid::new_v4().to_string())
        }
        async fn append_entry(&self, entry: &SessionTreeEntry) -> Result<(), anyhow::Error> {
            let mut calls = self.append_calls.lock().unwrap();
            *calls += 1;
            if *calls == *self.fail_at_call.lock().unwrap() {
                anyhow::bail!("injected append failure");
            }
            drop(calls);
            // Advance the cursor as part of the append, mirroring the JSONL
            // backend's contract: a `get_leaf_id` right after reflects this
            // entry without a separate `set_leaf_id`.
            let cursor = entry.leaf_cursor_after();
            self.entries.lock().unwrap().push(entry.clone());
            *self.leaf_id.lock().unwrap() = cursor;
            Ok(())
        }
        async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, anyhow::Error> {
            Ok(self
                .entries
                .lock()
                .unwrap()
                .iter()
                .find(|e| e.id() == id)
                .cloned())
        }
        async fn get_leaf_id(&self) -> Result<Option<String>, anyhow::Error> {
            Ok(self.leaf_id.lock().unwrap().clone())
        }
        async fn set_leaf_id(&self, leaf_id: Option<&str>) -> Result<(), anyhow::Error> {
            *self.leaf_id.lock().unwrap() = leaf_id.map(|s| s.to_string());
            Ok(())
        }
        async fn get_entries(&self) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
            Ok(self.entries.lock().unwrap().clone())
        }
        async fn get_path(
            &self,
            leaf_id: Option<&str>,
        ) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
            // Mirror the JSONL backend: walk the full path to the root, with
            // the same loud errors for an unknown leaf or a broken chain.
            // Returning every entry regardless of the cursor would mask
            // path-relative logic (e.g. `previousSummary` extraction).
            let entries = self.entries.lock().unwrap();
            let target_id = match leaf_id {
                None => return Ok(Vec::new()),
                Some(id) if entries.iter().any(|e| e.id() == id) => id.to_string(),
                Some(id) => anyhow::bail!("entry {id} not found"),
            };
            let mut index: std::collections::HashMap<&str, &SessionTreeEntry> =
                entries.iter().map(|e| (e.id(), e)).collect();
            let mut path: Vec<&SessionTreeEntry> = Vec::new();
            let mut current_id: Option<&str> = Some(&target_id);
            while let Some(id) = current_id {
                let entry = match index.remove(id) {
                    Some(e) => e,
                    None => anyhow::bail!("entry {id} not found: session chain is broken"),
                };
                current_id = entry.parent_id();
                path.push(entry);
            }
            path.reverse();
            Ok(path.into_iter().cloned().collect())
        }
    }

    #[tokio::test]
    async fn test_harness_prompt() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);

        let result = harness.prompt("Hello").await;
        assert!(result.is_ok());

        let messages = result.unwrap();
        assert!(!messages.is_empty());
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    #[tokio::test]
    async fn test_harness_phase_guard() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        // Directly set phase to Turn to test the guard.
        harness.phase = AgentHarnessPhase::Turn;
        let result = harness.prompt("Hello").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_harness_abort() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        harness.abort();
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    #[tokio::test]
    async fn test_harness_needs_compaction() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        // With empty messages, should not need compaction.
        assert!(!harness.needs_compaction());
    }

    #[tokio::test]
    async fn test_compact_drops_stale_usage_anchor() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());
        assert!(harness.needs_compaction());

        // After compaction the retained usage measured a different prefix, so
        // the post-compaction count is the plain character heuristic and the
        // harness does not immediately ask for another compaction.
        let result = harness.compact(None).await.unwrap();
        assert!(
            result.tokens_after < 1_000,
            "tokens_after={}",
            result.tokens_after
        );
        assert!(!harness.needs_compaction());
    }

    /// Compaction appends exactly one entry — the `compaction` itself — and
    /// the cursor advances to it via that single append. No companion `leaf`
    /// entry is written, so a failed `append_entry` cannot leave a compacted
    /// transcript with no boundary.
    #[tokio::test]
    async fn test_compact_appends_single_entry_and_advances_cursor() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());
        assert!(harness.needs_compaction());

        let before = *harness.session().storage().append_calls.lock().unwrap();
        harness.compact(None).await.unwrap();
        let after = *harness.session().storage().append_calls.lock().unwrap();

        // One append_entry for the compaction; no spurious leaf write.
        assert_eq!(after - before, 1);

        let entries = harness.session().storage().get_entries().await.unwrap();
        let compaction_id = match entries.last() {
            Some(SessionTreeEntry::Compaction { id, .. }) => id.clone(),
            other => panic!("expected a compaction entry, got {:?}", other),
        };
        // The cursor is the compaction's own id, not a leaf's target.
        let leaf = harness
            .session()
            .storage()
            .get_leaf_id()
            .await
            .unwrap()
            .expect("cursor must advance to the compaction");
        assert_eq!(leaf, compaction_id);
    }

    /// Compaction swaps the transcript but never the queues: steering and
    /// follow-up messages pending at compaction time stay deliverable, where
    /// a full reset would silently drop them. TS pairs this with one
    /// continuation after auto-compaction so the surviving queue drains.
    #[tokio::test]
    async fn test_compact_preserves_queued_messages() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());

        harness.agent_mut().steer(AgentMessage::user("steered in"));
        harness
            .agent_mut()
            .follow_up(AgentMessage::user("followed up"));
        assert!(harness.agent().has_queued_messages());

        harness.compact(None).await.unwrap();
        assert!(
            harness.agent().has_queued_messages(),
            "compaction must not drop queued user input"
        );

        // Both queues drain into the next run, steering before follow-up.
        harness.continue_().await.unwrap();
        assert!(!harness.agent().has_queued_messages());
        let user_texts: Vec<&str> = harness
            .agent()
            .state()
            .messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::User { content, .. } => match &content[0] {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let steered = user_texts
            .iter()
            .position(|t| *t == "steered in")
            .expect("steering message delivered");
        let followed = user_texts
            .iter()
            .position(|t| *t == "followed up")
            .expect("follow-up message delivered");
        assert!(steered < followed);
    }

    /// A transcript that fits inside the keep-recent window has nothing to
    /// summarize: compact refuses with [`compaction::NothingToCompact`]
    /// before the phase changes, any hook fires, or the model is called, and
    /// nothing persists. Mirrors TS `prepareCompaction` returning `undefined`.
    #[tokio::test]
    async fn test_compact_refuses_when_nothing_to_summarize() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingStreamFn {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl StreamFn for CountingStreamFn {
            async fn stream(
                &self,
                _context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "MUST NOT BE CALLED".into(),
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

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(CountingStreamFn {
                calls: Arc::clone(&calls),
            }),
        );
        // Two short messages fit the default keep-recent window whole.
        harness.prompt("hello").await.unwrap();
        // The prompt turn consumed one stream call; only compaction's own
        // summarization would add another.
        calls.store(0, Ordering::SeqCst);

        let hook_calls = Arc::new(AtomicUsize::new(0));
        harness.on(HookPoint::SessionBeforeCompact, {
            let hook_calls = Arc::clone(&hook_calls);
            Arc::new(move |ctx: HookContext| {
                hook_calls.fetch_add(1, Ordering::SeqCst);
                ctx
            })
        });

        let err = harness.compact(None).await.unwrap_err();
        assert!(
            err.downcast_ref::<compaction::NothingToCompact>().is_some(),
            "typed refusal, got: {err:#}"
        );
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
        assert_eq!(hook_calls.load(Ordering::SeqCst), 0, "no hook fired");
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no model call");
        assert!(
            harness
                .session()
                .storage()
                .get_entries()
                .await
                .unwrap()
                .iter()
                .all(|e| !matches!(e, SessionTreeEntry::Compaction { .. })),
            "no compaction entry persisted"
        );
        // The transcript is untouched.
        assert_eq!(harness.agent().state().messages.len(), 2);
    }

    /// After a compaction, a transcript of summary carrier + retained tail
    /// that still fits the keep-recent window is refused too: the carrier is
    /// folded in as `previous_summary`, never re-summarized, so the
    /// summarizable range is empty.
    #[tokio::test]
    async fn test_compact_refuses_when_only_summary_carrier_would_be_cut() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());
        harness.compact(None).await.unwrap();

        // The post-compaction transcript (summary + one retained assistant)
        // fits the window: a second compact finds nothing new to summarize.
        let err = harness.compact(None).await.unwrap_err();
        assert!(
            err.downcast_ref::<compaction::NothingToCompact>().is_some(),
            "typed refusal, got: {err:#}"
        );
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
        assert_eq!(harness.agent().state().messages.len(), 2);
    }

    /// A stream fn playing a scripted sequence of conversation outcomes.
    /// The summarization call — recognized by its system prompt — bypasses
    /// the script and always succeeds with a fixed summary.
    struct ScriptedStreamFn {
        script: std::sync::Mutex<std::collections::VecDeque<ScriptedTurn>>,
        summaries: Arc<std::sync::atomic::AtomicUsize>,
        fail_summaries: bool,
        /// Runs inside the summarization call, e.g. to queue a follow-up
        /// while compaction is in flight.
        on_summary: Option<Arc<dyn Fn() + Send + Sync>>,
    }

    enum ScriptedTurn {
        /// A completed assistant reply whose text is `0`-repeated `x` bytes.
        Answer(usize),
        /// A completed one-token reply reporting `total_tokens` of context
        /// usage, anchoring the token estimate above the threshold.
        AnswerWithUsage { total_tokens: u64 },
        /// A provider failure classified as context overflow.
        OverflowError,
        /// A transient provider failure the agent-level auto-retry restarts.
        RetryableError,
        /// A completed reply whose reported input exceeded the window — the
        /// silent overflow that compacts without retry.
        SilentOverflow,
        /// An overflow terminal stamped with a different model's identity.
        ForeignModelOverflow,
    }

    impl ScriptedStreamFn {
        fn new(script: Vec<ScriptedTurn>) -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
            let summaries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                ScriptedStreamFn {
                    script: std::sync::Mutex::new(script.into()),
                    summaries: Arc::clone(&summaries),
                    fail_summaries: false,
                    on_summary: None,
                },
                summaries,
            )
        }

        fn failing_summaries(mut self) -> Self {
            self.fail_summaries = true;
            self
        }

        fn on_summary(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
            self.on_summary = Some(Arc::new(f));
            self
        }
    }

    fn scripted_assistant(text: String, provider: &str, model: &str) -> AgentMessage {
        AgentMessage::Assistant {
            content: vec![ContentBlock::Text {
                text,
                signature: None,
            }],
            model: model.into(),
            provider: provider.into(),
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
    impl StreamFn for ScriptedStreamFn {
        async fn stream(
            &self,
            context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            if context.system_prompt == compaction::SUMMARIZATION_SYSTEM_PROMPT {
                self.summaries
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if let Some(on_summary) = &self.on_summary {
                    on_summary();
                }
                if self.fail_summaries {
                    return Err(anyhow::anyhow!("summarization boom"));
                }
                return Ok(scripted_assistant("summary".into(), "test", "test"));
            }
            match self.script.lock().unwrap().pop_front() {
                Some(ScriptedTurn::Answer(len)) => {
                    Ok(scripted_assistant("x".repeat(len), "test", "test"))
                }
                Some(ScriptedTurn::AnswerWithUsage { total_tokens }) => {
                    let mut message = scripted_assistant("x".into(), "test", "test");
                    if let AgentMessage::Assistant { usage, .. } = &mut message {
                        usage.total_tokens = total_tokens;
                    }
                    Ok(message)
                }
                Some(ScriptedTurn::OverflowError) => Err(anyhow::anyhow!(
                    "http 400: prompt is too long: 213462 tokens > 200000 maximum"
                )),
                Some(ScriptedTurn::RetryableError) => {
                    Err(anyhow::anyhow!("http 529: overloaded, please retry later"))
                }
                Some(ScriptedTurn::SilentOverflow) => {
                    let mut message = scripted_assistant("x".into(), "test", "test");
                    if let AgentMessage::Assistant { usage, .. } = &mut message {
                        usage.input_tokens = 150_000;
                    }
                    Ok(message)
                }
                Some(ScriptedTurn::ForeignModelOverflow) => Ok(AgentMessage::Assistant {
                    content: Vec::new(),
                    model: "other".into(),
                    provider: "other".into(),
                    api: "test".into(),
                    response_model: None,
                    response_id: None,
                    diagnostics: None,
                    raw_stop_reason: None,
                    stop_reason: Some(StopReason::Error),
                    usage: Box::new(Usage::default()),
                    error_message: Some(
                        "prompt is too long: 213462 tokens > 200000 maximum".into(),
                    ),
                    timestamp: chrono::Utc::now(),
                }),
                None => panic!("script exhausted"),
            }
        }
    }

    async fn compaction_entries(harness: &AgentHarness<MemStorage>) -> Vec<SessionTreeEntry> {
        harness
            .session()
            .storage()
            .get_entries()
            .await
            .expect("entries")
            .into_iter()
            .filter(|e| matches!(e, SessionTreeEntry::Compaction { .. }))
            .collect()
    }

    /// The core closed loop: a turn ending in a context overflow drops its
    /// terminal message from the transcript, compacts, and retries once —
    /// the session keeps the failed turn for history.
    #[tokio::test]
    async fn test_overflow_compacts_and_retries() {
        let (stream_fn, summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::OverflowError,
            ScriptedTurn::Answer(16),
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        let messages = harness.prompt("second").await.unwrap();
        // The failed turn's output plus the retry's reply are returned
        // together: user, overflow terminal, recovered assistant.
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert!(matches!(
            &messages[1],
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Error),
                ..
            }
        ));
        assert!(matches!(
            &messages[2],
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Stop),
                ..
            }
        ));

        // The transcript holds no error: summary carrier, retained prompt,
        // recovered reply.
        let transcript = &harness.agent().state().messages;
        assert_eq!(transcript.len(), 3, "{transcript:?}");
        assert!(
            transcript.iter().all(|m| !matches!(
                m,
                AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                }
            )),
            "the failed terminal left the transcript: {transcript:?}"
        );

        // The session keeps the failed turn and exactly one boundary.
        let entries = harness.session().storage().get_entries().await.unwrap();
        assert!(
            entries.iter().any(|e| matches!(
                e,
                SessionTreeEntry::Message {
                    message: AgentMessage::Assistant {
                        stop_reason: Some(StopReason::Error),
                        ..
                    },
                    ..
                }
            )),
            "the overflow error stays persisted: {entries:?}"
        );
        assert_eq!(compaction_entries(&harness).await.len(), 1);
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// The recovery budget is one-shot per error episode: when the retry
    /// overflows again, the second error surfaces instead of another
    /// compact-and-retry.
    #[tokio::test]
    async fn test_overflow_recovery_is_one_shot() {
        let (stream_fn, summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::OverflowError,
            ScriptedTurn::OverflowError,
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        let messages = harness.prompt("second").await.unwrap();
        // Both overflow terminals are returned; no third conversation call
        // happened (the script held exactly two overflow entries).
        assert!(
            matches!(
                messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                })
            ),
            "{messages:?}"
        );
        assert_eq!(compaction_entries(&harness).await.len(), 1);
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            matches!(
                harness.agent().state().messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                })
            ),
            "the second overflow error stands"
        );
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    /// A successful retry rearms the recovery budget: a later overflow gets
    /// its own compact-and-retry.
    #[tokio::test]
    async fn test_overflow_recovery_rearms_after_success() {
        let (stream_fn, summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::OverflowError,
            ScriptedTurn::Answer(2048),
            ScriptedTurn::OverflowError,
            ScriptedTurn::Answer(16),
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        harness.prompt("second").await.unwrap();
        let messages = harness.prompt("third").await.unwrap();
        assert!(
            matches!(
                messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Stop),
                    ..
                })
            ),
            "the second episode recovered too: {messages:?}"
        );
        assert_eq!(compaction_entries(&harness).await.len(), 2);
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(
            harness.agent().state().messages.iter().all(|m| !matches!(
                m,
                AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                }
            )),
            "no error survives in the transcript"
        );
    }

    /// An overflow with nothing summarizable cannot shrink the context: the
    /// failed terminal leaves the transcript, no boundary is persisted, and
    /// no retry runs.
    #[tokio::test]
    async fn test_overflow_without_summarizable_range_surfaces_error() {
        let (stream_fn, summaries) =
            ScriptedStreamFn::new(vec![ScriptedTurn::Answer(8), ScriptedTurn::OverflowError]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        let messages = harness.prompt("second").await.unwrap();
        assert!(
            matches!(
                messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                })
            ),
            "{messages:?}"
        );
        assert!(compaction_entries(&harness).await.is_empty());
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 0);
        // The failed terminal left the transcript (the session keeps it);
        // the transcript ends on the second prompt.
        let transcript = &harness.agent().state().messages;
        assert!(matches!(transcript.last(), Some(AgentMessage::User { .. })));
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    /// An overflow terminal attributed to a different model is not this
    /// model's error to recover: no compaction, no retry.
    #[tokio::test]
    async fn test_overflow_from_other_model_is_not_recovered() {
        let (stream_fn, summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::ForeignModelOverflow,
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });
        let messages = harness.prompt("second").await.unwrap();
        assert!(
            matches!(
                messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                })
            ),
            "{messages:?}"
        );
        assert!(compaction_entries(&harness).await.is_empty());
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 0);
        // The foreign error message stays in the transcript.
        assert!(
            matches!(
                harness.agent().state().messages.last(),
                Some(AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                })
            ),
            "the untouched error stands"
        );
    }

    /// A settled turn whose reported usage crosses the threshold is
    /// compacted for the next turn — without a retry: the returned messages
    /// are the turn's own, and the transcript is rebuilt behind one
    /// compaction boundary.
    #[tokio::test]
    async fn test_threshold_compaction_fires_after_settled_turn() {
        let (stream_fn, summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::AnswerWithUsage {
                total_tokens: 90_000,
            },
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 0);
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        let messages = harness.prompt("second").await.unwrap();
        // No retry: exactly the turn's own user message and reply.
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(matches!(
            messages.last(),
            Some(AgentMessage::Assistant {
                stop_reason: Some(StopReason::Stop),
                ..
            })
        ));
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(compaction_entries(&harness).await.len(), 1);
        // The transcript was rebuilt: summary carrier first, the 90k-usage
        // reply retained in the tail.
        let transcript = &harness.agent().state().messages;
        assert!(transcript.len() < 4, "{transcript:?}");
        assert!(matches!(
            transcript.last(),
            Some(AgentMessage::Assistant { .. })
        ));
    }

    /// A failed maintenance compaction leaves the settled turn's result
    /// intact: prompt resolves with the turn's messages, nothing is
    /// persisted, and the transcript keeps the full conversation.
    #[tokio::test]
    async fn test_threshold_compaction_failure_keeps_turn_result() {
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::AnswerWithUsage {
                total_tokens: 90_000,
            },
        ]);
        let stream_fn = stream_fn.failing_summaries();
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        let messages = harness.prompt("second").await.unwrap();
        assert_eq!(messages.len(), 2, "{messages:?}");
        assert!(compaction_entries(&harness).await.is_empty());
        assert_eq!(harness.agent().state().messages.len(), 4);
    }

    /// A follow-up queued while threshold compaction runs is delivered by
    /// one continuation immediately after — compaction never waits for the
    /// next explicit prompt to drain the queue.
    #[tokio::test]
    async fn test_threshold_compaction_delivers_queued_follow_up() {
        let handle_slot: Arc<std::sync::Mutex<Option<crate::agent::RunHandle>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot_in_summary = Arc::clone(&handle_slot);
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::AnswerWithUsage {
                total_tokens: 90_000,
            },
            ScriptedTurn::Answer(8),
        ]);
        let stream_fn = stream_fn.on_summary(move || {
            if let Some(handle) = slot_in_summary.lock().unwrap().as_ref() {
                handle.follow_up(AgentMessage::user("while compacting"));
            }
        });
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        *handle_slot.lock().unwrap() = Some(harness.agent().run_handle());

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        let messages = harness.prompt("second").await.unwrap();
        // Turn output plus the drain continuation's follow-up and reply.
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert!(
            messages.iter().any(|m| matches!(
                m,
                AgentMessage::User { content, .. }
                    if matches!(&content[0], ContentBlock::Text { text, .. } if text == "while compacting")
            )),
            "the queued follow-up was delivered: {messages:?}"
        );
        assert!(!harness.agent().has_queued_messages());
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    /// A follow-up queued by an `agent_end` listener is delivered by one
    /// continuation even when nothing needs compaction — the queued check is
    /// the loop's own exit condition, not a side effect of the threshold
    /// branch.
    #[tokio::test]
    async fn test_agent_end_listener_follow_up_is_delivered_without_compaction() {
        let handle = std::sync::Arc::new(std::sync::Mutex::new(None::<crate::agent::RunHandle>));
        let handle_in_listener = std::sync::Arc::clone(&handle);
        let (stream_fn, _summaries) =
            ScriptedStreamFn::new(vec![ScriptedTurn::Answer(2048), ScriptedTurn::Answer(8)]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        let agent_handle = harness.agent().run_handle();
        *handle.lock().unwrap() = Some(agent_handle.clone());
        // Queue exactly once: the listener fires on every run's AgentEnd,
        // including the drain continuation's, and an unconditional queue
        // would keep the settle loop alive forever.
        let queued = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let queued_in_listener = std::sync::Arc::clone(&queued);
        let _sub = harness.agent().subscribe(Arc::new(move |event, _token| {
            let handle = std::sync::Arc::clone(&handle_in_listener);
            let queued = std::sync::Arc::clone(&queued_in_listener);
            Box::pin(async move {
                if matches!(event, AgentEvent::AgentEnd { .. })
                    && !queued.swap(true, std::sync::atomic::Ordering::SeqCst)
                    && let Some(handle) = handle.lock().unwrap().as_ref()
                {
                    handle.follow_up(AgentMessage::user("from agent_end"));
                }
            })
        }));

        let messages = harness.prompt("first").await.unwrap();
        // Turn output plus the drain continuation's follow-up and reply.
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert!(
            messages.iter().any(|m| matches!(
                m,
                AgentMessage::User { content, .. }
                    if matches!(&content[0], ContentBlock::Text { text, .. } if text == "from agent_end")
            )),
            "the agent_end follow-up was delivered: {messages:?}"
        );
        assert!(!harness.agent().has_queued_messages());
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);
    }

    /// A follow-up queued while the overflow compact-no-retry path compacts
    /// is delivered by one continuation after — the same delivery the
    /// threshold branch gets, reached through the overflow path this time.
    #[tokio::test]
    async fn test_overflow_compact_no_retry_delivers_queued_follow_up() {
        let handle_slot: Arc<std::sync::Mutex<Option<crate::agent::RunHandle>>> =
            Arc::new(std::sync::Mutex::new(None));
        let slot_in_summary = Arc::clone(&handle_slot);
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::SilentOverflow,
            ScriptedTurn::Answer(8),
        ]);
        let stream_fn = stream_fn.on_summary(move || {
            if let Some(handle) = slot_in_summary.lock().unwrap().as_ref() {
                handle.follow_up(AgentMessage::user("while compacting"));
            }
        });
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        *handle_slot.lock().unwrap() = Some(harness.agent().run_handle());

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });

        let messages = harness.prompt("second").await.unwrap();
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert!(
            messages.iter().any(|m| matches!(
                m,
                AgentMessage::User { content, .. }
                    if matches!(&content[0], ContentBlock::Text { text, .. } if text == "while compacting")
            )),
            "the queued follow-up was delivered: {messages:?}"
        );
        assert!(!harness.agent().has_queued_messages());
    }

    /// A transient provider failure is retried after the backoff and the
    /// recovered reply is returned with the failed turn — the error message
    /// leaves the transcript for the retry while the session keeps it.
    #[tokio::test]
    async fn test_retryable_error_is_retried_after_backoff() {
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::RetryableError,
            ScriptedTurn::Answer(16),
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        harness.set_retry_settings(RetrySettings {
            base_delay_ms: 2,
            ..Default::default()
        });

        harness.prompt("first").await.unwrap();
        let messages = harness.prompt("second").await.unwrap();
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert!(matches!(
            &messages[1],
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Error),
                error_message: Some(err),
                ..
            } if err.contains("overloaded")
        ));
        assert!(matches!(
            &messages[2],
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Stop),
                ..
            }
        ));
        // The transcript ends on the recovered reply with the failed turn
        // removed (the session keeps it).
        let transcript = &harness.agent().state().messages;
        assert_eq!(transcript.len(), 4, "{transcript:?}");
        assert!(
            !transcript.iter().any(|m| matches!(
                m,
                AgentMessage::Assistant {
                    stop_reason: Some(StopReason::Error),
                    ..
                }
            )),
            "the failed turn left the transcript: {transcript:?}"
        );
        assert!(matches!(
            transcript.last(),
            Some(AgentMessage::Assistant {
                stop_reason: Some(StopReason::Stop),
                ..
            })
        ));
    }

    /// The retry budget is per error episode: a failure that stays retryable
    /// exhausts `maxRetries` attempts, and the terminal error then settles
    /// without another retry.
    #[tokio::test]
    async fn test_retry_budget_exhaustion_keeps_terminal_error() {
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::RetryableError,
            ScriptedTurn::RetryableError,
            ScriptedTurn::RetryableError,
            ScriptedTurn::RetryableError,
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        harness.set_retry_settings(RetrySettings {
            base_delay_ms: 1,
            max_retries: 3,
            ..Default::default()
        });

        harness.prompt("first").await.unwrap();
        let messages = harness.prompt("second").await.unwrap();
        // The original failure plus three retried failures; the fourth
        // retryable failure is terminal.
        assert_eq!(messages.len(), 5, "{messages:?}");
        assert!(messages[1..].iter().all(|m| matches!(
            m,
            AgentMessage::Assistant {
                stop_reason: Some(StopReason::Error),
                ..
            }
        )));
        assert_eq!(harness.retry_attempt(), 0);
    }

    /// The retry lifecycle emits `auto_retry_start` then `auto_retry_end` on
    /// success, mirroring the TS session events.
    #[tokio::test]
    async fn test_auto_retry_emits_lifecycle_events() {
        let (stream_fn, _summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::RetryableError,
            ScriptedTurn::Answer(16),
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        harness.set_retry_settings(RetrySettings {
            base_delay_ms: 1,
            ..Default::default()
        });
        let events: Arc<std::sync::Mutex<Vec<RetryEvent>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = std::sync::Arc::clone(&events);
        harness.on_auto_retry(move |event| capture.lock().unwrap().push(event));

        harness.prompt("first").await.unwrap();
        harness.prompt("second").await.unwrap();
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(
            &events[0],
            RetryEvent::Start {
                attempt: 1,
                max_attempts: 3,
                error_message,
                ..
            } if error_message.contains("overloaded")
        ));
        assert!(matches!(
            &events[1],
            RetryEvent::End {
                success: true,
                attempt: 1,
                final_error: None,
            }
        ));
    }

    /// Context overflow never enters the agent-level auto-retry — it goes to
    /// compaction, and no retry lifecycle events fire.
    #[tokio::test]
    async fn test_overflow_error_is_not_auto_retried() {
        let (stream_fn, summaries) = ScriptedStreamFn::new(vec![
            ScriptedTurn::Answer(2048),
            ScriptedTurn::OverflowError,
            ScriptedTurn::Answer(16),
        ]);
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(stream_fn),
        );
        harness.set_retry_settings(RetrySettings {
            base_delay_ms: 1,
            ..Default::default()
        });
        let retries = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = std::sync::Arc::clone(&retries);
        harness.on_auto_retry(move |_| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });

        harness.prompt("first").await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 10,
            ..Default::default()
        });
        let messages = harness.prompt("second").await.unwrap();
        // Overflow compact-and-retry, not agent-level retry.
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert_eq!(summaries.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(retries.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(harness.retry_attempt(), 0);
    }

    /// A failed summarization (Error/Aborted terminal, or an empty summary)
    /// must not persist a compaction entry nor rewrite the transcript — the
    /// compacted prefix would otherwise be replaced with nothing and lost.
    #[tokio::test]
    async fn test_compact_rejects_failed_summary_without_persisting() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(AbortedSummaryStreamFn),
        );

        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());
        assert!(harness.needs_compaction());

        let entries_before = harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap()
            .len();
        let transcript_before = harness.agent_mut().state().messages.len();

        let err = harness.compact(None).await.unwrap_err();
        assert!(
            err.to_string().contains("summarization failed (aborted)"),
            "{}",
            err
        );

        // No compaction entry was persisted.
        let entries_after = harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap()
            .len();
        assert_eq!(entries_after, entries_before);

        // The transcript is untouched: the compacted prefix survives.
        assert_eq!(
            harness.agent_mut().state().messages.len(),
            transcript_before
        );
    }

    /// A `SessionBeforeCompact` hook can cancel the run: nothing is persisted
    /// and the transcript is left intact. Mirrors the TS `cancel` branch.
    #[tokio::test]
    async fn test_before_compact_hook_can_cancel() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());
        let entries_before = harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap()
            .len();

        harness.on(
            HookPoint::SessionBeforeCompact,
            Arc::new(|ctx: HookContext| ctx.with_cancel_compaction()),
        );

        let err = harness.compact(None).await.unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");

        // No compaction entry was persisted.
        assert_eq!(
            harness
                .session()
                .storage()
                .get_entries()
                .await
                .unwrap()
                .len(),
            entries_before
        );
        // The transcript is untouched.
        assert_eq!(harness.agent_mut().state().messages.len(), 4);
    }

    /// A `SessionBeforeCompact` hook can supply the summary directly: the
    /// summarization model is never called, and the persisted entry carries
    /// `fromHook` and the hook's `details`. Mirrors the TS `compaction` branch.
    #[tokio::test]
    async fn test_before_compact_hook_can_supply_summary() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingStreamFn {
            calls: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl StreamFn for CountingStreamFn {
            async fn stream(
                &self,
                _context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "MUST NOT BE USED".into(),
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

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(CountingStreamFn {
                calls: Arc::clone(&calls),
            }),
        );
        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());

        harness.on(
            HookPoint::SessionBeforeCompact,
            Arc::new(|ctx: HookContext| {
                ctx.with_compact_override(BeforeCompactOverride {
                    summary: "hook-authored summary".into(),
                    tokens_before: 90_000,
                    first_kept_entry_id: None,
                    retained_tail: vec![],
                    details: Some(serde_json::json!({"files": ["a.rs", "b.rs"]})),
                    usage: Some(Usage {
                        total_tokens: 7,
                        ..Default::default()
                    }),
                })
            }),
        );

        let result = harness.compact(None).await.expect("hook-supplied compact");
        assert_eq!(result.summary, "hook-authored summary");
        // The summarization model was never called.
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // The persisted entry carries fromHook and the hook's details.
        let compaction = harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap()
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Compaction {
                    summary,
                    from_hook,
                    details,
                    usage,
                    tokens_before,
                    ..
                } => Some((
                    summary.clone(),
                    *from_hook,
                    details.clone(),
                    usage.clone(),
                    *tokens_before,
                )),
                _ => None,
            })
            .expect("compaction persisted");
        assert_eq!(compaction.0, "hook-authored summary");
        assert_eq!(compaction.1, Some(true));
        assert_eq!(
            compaction.2,
            Some(serde_json::json!({"files": ["a.rs", "b.rs"]}))
        );
        assert_eq!(compaction.3.map(|u| u.total_tokens), Some(7));
        // tokens_before is persisted verbatim from the hook, not recomputed.
        assert_eq!(compaction.4, 90_000);

        // The returned result mirrors the hook's authorship, so callers need
        // not re-read storage to recover it.
        assert_eq!(result.summary, "hook-authored summary");
        assert_eq!(result.usage.map(|u| u.total_tokens), Some(7));
        assert_eq!(
            result.details,
            Some(serde_json::json!({"files": ["a.rs", "b.rs"]}))
        );
        assert!(result.retained_tail.is_empty());
    }

    /// The before-compact hook receives the TS-shaped `preparation` and the
    /// session `branchEntries`, so it can decide on the specific content being
    /// compacted rather than blind. Mirrors the TS `SessionBeforeCompactEvent`.
    #[tokio::test]
    async fn test_before_compact_hook_receives_preparation_and_branch_entries() {
        use std::sync::Mutex;

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        // Build a real session (two long prompts → two user/assistant pairs
        // persisted as Message entries) so `branchEntries` is non-empty. The
        // keep-recent budget sits between one and two turns, so the cut lands
        // mid-transcript: a non-empty prefix is summarized and a non-empty tail
        // with a real first-kept entry id is retained.
        let long = "x".repeat(2048);
        harness.prompt(&long).await.unwrap();
        harness.prompt(&long).await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 600,
            ..Default::default()
        });

        let captured = Arc::new(Mutex::new(serde_json::Value::Null));
        harness.on(HookPoint::SessionBeforeCompact, {
            let captured = Arc::clone(&captured);
            Arc::new(move |ctx: HookContext| {
                *captured.lock().unwrap() = ctx.data.clone();
                ctx
            })
        });

        harness.compact(None).await.expect("compact");

        let data = captured.lock().unwrap().clone();
        assert_eq!(
            data.get("type").and_then(|v| v.as_str()),
            Some("session_before_compact"),
            "the typed event carries the TS discriminator: {data}"
        );
        // `customInstructions` is omitted (None) when compact() takes none —
        // the field is part of the contract but absent on the wire, matching
        // TS optionality.
        assert!(data.get("customInstructions").is_none());

        let preparation = data
            .get("preparation")
            .expect("hook data carries preparation");
        // TS `CompactionPreparation` field names and shapes.
        assert!(
            preparation
                .get("firstKeptEntryId")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "firstKeptEntryId is the real id of the first retained entry: {preparation}"
        );
        assert!(
            preparation
                .get("tokensBefore")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                > 0,
            "preparation reports the pre-compaction token count: {preparation}"
        );
        assert!(
            !preparation
                .get("isSplitTurn")
                .and_then(|v| v.as_bool())
                .unwrap_or(true),
            "the Rust port never splits a turn"
        );
        let messages_to_summarize = preparation
            .get("messagesToSummarize")
            .and_then(|v| v.as_array())
            .expect("preparation carries messagesToSummarize");
        assert!(
            !messages_to_summarize.is_empty(),
            "messagesToSummarize holds the compacted prefix"
        );
        let retained_tail = preparation
            .get("retainedTail")
            .and_then(|v| v.as_array())
            .expect("preparation carries retainedTail");
        assert!(
            !retained_tail.is_empty(),
            "retainedTail holds the kept suffix"
        );
        // The Rust port has no split-turn prefix.
        assert!(
            preparation
                .get("turnPrefixMessages")
                .and_then(|v| v.as_array())
                .map(|a| a.is_empty())
                .unwrap_or(false),
            "turnPrefixMessages is always empty"
        );
        // No prior compaction on this branch.
        assert!(
            preparation.get("previousSummary").is_none(),
            "previousSummary is absent when the branch never compacted"
        );
        assert!(
            preparation.get("fileOps").is_some(),
            "fileOps is always present"
        );
        assert!(
            preparation.get("settings").is_some(),
            "settings is always present"
        );

        let branch_entries = data
            .get("branchEntries")
            .and_then(|v| v.as_array())
            .expect("hook data carries branchEntries");
        assert!(
            !branch_entries.is_empty(),
            "branchEntries holds the session entries of the branch"
        );
    }

    /// `compact(Some(_))` folds custom instructions into the summarization
    /// prompt on the default (no-override) path — they must not be silently
    /// dropped merely because no hook supplied a summary.
    #[tokio::test]
    async fn test_compact_custom_instructions_reach_summarization_prompt() {
        use std::sync::Mutex;

        struct CaptureStreamFn {
            seen: Arc<Mutex<String>>,
        }

        #[async_trait::async_trait]
        impl StreamFn for CaptureStreamFn {
            async fn stream(
                &self,
                context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                if let Some(AgentMessage::User { content, .. }) = context.messages.first() {
                    for b in content {
                        if let ContentBlock::Text { text, .. } = b {
                            *self.seen.lock().unwrap() = text.clone();
                        }
                    }
                }
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "summary".into(),
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

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let seen = Arc::new(Mutex::new(String::new()));
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(CaptureStreamFn {
                seen: Arc::clone(&seen),
            }),
        );
        // A transcript large enough to compact a non-empty prefix past the
        // keep-recent budget, so the summarization model is actually invoked.
        let long = "x".repeat(2048);
        harness.prompt(&long).await.unwrap();
        harness.prompt(&long).await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 600,
            ..Default::default()
        });

        harness
            .compact(Some("emphasize the auth module"))
            .await
            .expect("compact");

        let prompt = seen.lock().unwrap().clone();
        assert!(
            prompt.contains("Additional focus: emphasize the auth module"),
            "custom instructions reach the summarization prompt, not dropped: {prompt}"
        );
    }

    /// The default (no-override) summarization path consumes the
    /// [`CompactionPreparation`]: file operations extracted from the compacted
    /// prefix are appended to the summary text and returned as `details`
    /// (`readFiles`/`modifiedFiles`), mirroring TS `compact`. A read call on a
    /// file that was also edited/written is classified as modified, not read.
    #[tokio::test]
    async fn test_compact_model_path_folds_file_operations() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        let tool_use = |id: &str, name: &str, path: &str| ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({"path": path}),
            thought_signature: None,
        };
        // user1 alone exceeds the keep-recent budget, so the cut lands after
        // the assistant tool-use turn — putting the tool calls in the prefix
        // that gets summarized (and thus in `fileOps`).
        let long = "x".repeat(4096);
        let assistant = AgentMessage::Assistant {
            content: vec![
                tool_use("1", "read", "a.rs"),
                tool_use("2", "read", "c.rs"),
                tool_use("3", "edit", "a.rs"),
                tool_use("4", "write", "b.rs"),
            ],
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
        };
        harness.agent_mut().replace_transcript(vec![
            AgentMessage::user(&long),
            assistant,
            AgentMessage::user(&long),
            AgentMessage::user("recent tail"),
        ]);
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 600,
            ..Default::default()
        });

        let result = harness.compact(None).await.expect("compact");
        // `a.rs` is read and edited → modified wins; `c.rs` is read-only.
        assert!(
            result.summary.contains("<read-files>\nc.rs\n</read-files>"),
            "read-only files are appended to the summary: {summary}",
            summary = result.summary
        );
        assert!(
            result
                .summary
                .contains("<modified-files>\na.rs\nb.rs\n</modified-files>"),
            "edited ∪ written files are appended to the summary: {summary}",
            summary = result.summary
        );
        assert_eq!(
            result.details,
            Some(serde_json::json!({
                "readFiles": ["c.rs"],
                "modifiedFiles": ["a.rs", "b.rs"],
            })),
            "details carries the computed file lists"
        );
    }

    /// A repeated compaction folds the prior summary in once (as
    /// `previousSummary`) and excludes the synthetic `summary_message` from
    /// `messagesToSummarize` — mirroring TS, whose `messagesToSummarize` starts
    /// at the boundary's first kept entry, not the compaction entry. The prior
    /// summary must not appear twice in the summarization prompt.
    #[tokio::test]
    async fn test_repeated_compaction_does_not_duplicate_prior_summary() {
        use std::sync::Mutex;

        struct CaptureStreamFn {
            seen: Arc<Mutex<String>>,
        }

        #[async_trait::async_trait]
        impl StreamFn for CaptureStreamFn {
            async fn stream(
                &self,
                context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                if let Some(AgentMessage::User { content, .. }) = context.messages.first() {
                    for b in content {
                        if let ContentBlock::Text { text, .. } = b {
                            *self.seen.lock().unwrap() = text.clone();
                        }
                    }
                }
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "prior session covered the API".into(),
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

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let seen = Arc::new(Mutex::new(String::new()));
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(CaptureStreamFn {
                seen: Arc::clone(&seen),
            }),
        );
        let long = "x".repeat(4096);
        harness.prompt(&long).await.unwrap();
        harness.prompt(&long).await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 600,
            ..Default::default()
        });

        // First compaction establishes a prior summary on the branch.
        harness.compact(None).await.expect("first compact");
        // Grow the transcript so a second compaction has a non-empty prefix.
        harness.prompt(&long).await.unwrap();
        harness.prompt(&long).await.unwrap();
        harness.compact(None).await.expect("second compact");

        let prompt = seen.lock().unwrap().clone();
        // The prior summary is folded in once as <previous-summary> context.
        assert!(
            prompt
                .contains("<previous-summary>\nprior session covered the API\n</previous-summary>"),
            "previousSummary is folded into the prompt: {prompt}"
        );
        // The synthetic summary carrier is excluded from messagesToSummarize —
        // its wrapper must not leak into the prompt as a transcript line.
        assert!(
            !prompt.contains(
                "The conversation history before this point was compacted into the following summary:"
            ),
            "the prior summary message is not double-counted: {prompt}"
        );
    }

    /// A hook override with an empty summary must not persist a compaction:
    /// the compacted prefix would be replaced with nothing and lost. Same
    /// invariant the model path enforces for empty model summaries.
    #[tokio::test]
    async fn test_before_compact_hook_empty_summary_rejected() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());
        let entries_before = harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap()
            .len();

        harness.on(
            HookPoint::SessionBeforeCompact,
            Arc::new(|ctx: HookContext| {
                ctx.with_compact_override(BeforeCompactOverride {
                    summary: "   \n\t".into(),
                    tokens_before: 0,
                    first_kept_entry_id: None,
                    retained_tail: vec![],
                    details: None,
                    usage: None,
                })
            }),
        );

        let err = harness.compact(None).await.unwrap_err();
        assert!(err.to_string().contains("empty summary"), "{err}");

        // No compaction entry was persisted and the transcript is intact.
        assert_eq!(
            harness
                .session()
                .storage()
                .get_entries()
                .await
                .unwrap()
                .len(),
            entries_before
        );
        assert_eq!(harness.agent_mut().state().messages.len(), 4);
    }

    /// A hook that overrides only the summary keeps the harness-computed tail
    /// by passing `preparation.retained_tail` through verbatim. The required
    /// `retained_tail` field makes erasure an explicit `vec![]`, not an
    /// accidental omission — so a summary-only override still retains context.
    #[tokio::test]
    async fn test_before_compact_hook_passes_through_retained_tail() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        let long = "x".repeat(2048);
        harness.prompt(&long).await.unwrap();
        harness.prompt(&long).await.unwrap();
        harness.set_compaction_settings(CompactionSettings {
            keep_recent_tokens: 600,
            ..Default::default()
        });

        harness.on(
            HookPoint::SessionBeforeCompact,
            Arc::new(|ctx: HookContext| {
                // Pass the harness-computed tail through; supply a fresh summary
                // and the preparation's token count so the override is complete.
                let preparation = ctx
                    .data
                    .get("preparation")
                    .unwrap_or(&serde_json::Value::Null);
                let retained_tail: Vec<AgentMessage> = preparation
                    .get("retainedTail")
                    .cloned()
                    .and_then(|v| serde_json::from_value(v).ok())
                    .unwrap_or_default();
                let tokens_before = preparation
                    .get("tokensBefore")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                ctx.with_compact_override(BeforeCompactOverride {
                    summary: "hook summary that keeps the tail".into(),
                    tokens_before,
                    first_kept_entry_id: None,
                    retained_tail,
                    usage: None,
                    details: None,
                })
            }),
        );

        let result = harness.compact(None).await.expect("compact");
        // The hook's fresh summary replaces the prefix; the passed-through tail
        // survives, so the rebuilt transcript is summary + retained messages.
        assert!(
            !result.retained_tail.is_empty(),
            "tail was retained, not erased"
        );
        assert_eq!(
            harness.agent().state().messages.len(),
            1 + result.retained_tail.len(),
            "transcript = summary + retained tail"
        );
    }

    #[tokio::test]
    async fn test_compact_drains_more_events_than_channel_capacity() {
        // The summarization channel caps at 64 events; ChattyStreamFn emits
        // 200. A harness that drains only after the producer returns would
        // deadlock here (the test would hang and time out).
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ChattyStreamFn),
        );
        harness
            .agent_mut()
            .replace_transcript(compactable_transcript());
        harness.set_compaction_settings(compact_test_settings());

        let result = harness
            .compact(None)
            .await
            .expect("compact must not deadlock");
        assert_eq!(result.summary, "summary");
    }

    #[tokio::test]
    async fn test_compaction_boundary_survives_session_restore() {
        use crate::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};

        let dir = tempfile::tempdir().unwrap();
        let meta = || JsonlSessionMetadata {
            id: "s".into(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        };

        let stale_assistant = AgentMessage::Assistant {
            content: vec![],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage {
                total_tokens: 90_000,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };

        // Compact in a first harness over an on-disk session.
        {
            let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
                .await
                .unwrap();
            let session = Session::new(storage);
            let mut harness = AgentHarness::new(
                session,
                "You are a test assistant.",
                test_model(),
                Arc::new(TestStreamFn),
            );
            harness
                .agent_mut()
                .replace_transcript(compactable_transcript());
            harness.set_compaction_settings(compact_test_settings());
            let result = harness.compact(None).await.unwrap();
            assert_eq!(result.tokens_before, 90_000);
        }

        // Reopen the session from disk: the compaction entry survived; the
        // kept segment behind it is reconstructed by walking the tree from
        // its first-kept entry id, not read off the boundary.
        let storage = JsonlSessionStorage::open(&dir.path().join("session.jsonl"))
            .await
            .unwrap();
        let entries = storage.get_entries().await.unwrap();
        let boundary = entries.iter().find_map(|e| match e {
            SessionTreeEntry::Compaction {
                summary,
                tokens_before,
                ..
            } => Some((summary.clone(), *tokens_before)),
            _ => None,
        });
        assert_eq!(boundary, Some(("Test response".to_string(), 90_000)));

        // A fresh harness over the restored session recovers the boundary, so
        // a transcript whose usage predates the compaction cannot anchor.
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness.recover_boundary().await.unwrap();
        harness
            .agent_mut()
            .replace_transcript(vec![AgentMessage::user("q"), stale_assistant.clone()]);
        assert!(!harness.needs_compaction());

        // Without recovery the stale usage anchors and the threshold trips.
        let storage = JsonlSessionStorage::open(&dir.path().join("session.jsonl"))
            .await
            .unwrap();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness
            .agent_mut()
            .replace_transcript(vec![AgentMessage::user("q"), stale_assistant]);
        assert!(harness.needs_compaction());
    }

    #[tokio::test]
    async fn test_restore_rebuilds_transcript_from_session() {
        use crate::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};

        let dir = tempfile::tempdir().unwrap();
        let meta = || JsonlSessionMetadata {
            id: "s".into(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        };

        // Run a turn, compact, run another turn — all over an on-disk
        // session. The first prompt is long and the keep-recent budget
        // narrow, so the cut retains only the first turn's assistant reply:
        // the post-compaction transcript is summary + assistant.
        let expected;
        {
            let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
                .await
                .unwrap();
            let session = Session::new(storage);
            let mut harness = AgentHarness::new(
                session,
                "You are a test assistant.",
                test_model(),
                Arc::new(TestStreamFn),
            );
            harness.set_compaction_settings(CompactionSettings {
                keep_recent_tokens: 100,
                ..Default::default()
            });
            harness.prompt(&"f".repeat(2048)).await.unwrap();
            harness.compact(None).await.unwrap();
            harness.prompt("second").await.unwrap();
            expected = serde_json::to_value(&harness.agent().state().messages).unwrap();
        }

        // A fresh harness restores the full transcript: summary, retained
        // tail, and the post-compaction messages.
        let storage = JsonlSessionStorage::open(&dir.path().join("session.jsonl"))
            .await
            .unwrap();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness.restore().await.unwrap();

        fn text_of(m: &AgentMessage) -> &str {
            match m {
                AgentMessage::User { content, .. } | AgentMessage::Assistant { content, .. } => {
                    match &content[0] {
                        ContentBlock::Text { text, .. } => text.as_str(),
                        _ => "",
                    }
                }
                _ => "",
            }
        }
        let messages = &harness.agent().state().messages;
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert_eq!(
            text_of(&messages[0]),
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\nTest response\n</summary>"
        );
        assert!(matches!(&messages[1], AgentMessage::Assistant { .. }));
        assert_eq!(text_of(&messages[2]), "second");
        assert!(matches!(&messages[3], AgentMessage::Assistant { .. }));

        // The restored transcript equals the post-compaction one exactly,
        // summary timestamp included.
        let restored = serde_json::to_value(messages).unwrap();
        assert_eq!(restored, expected);

        // The estimation boundary came along: needs_compaction works without
        // a separate recover_boundary() call.
        assert!(!harness.needs_compaction());
    }

    /// A hook-supplied retained tail persists with the boundary: the restored
    /// transcript replays it even though the hook's messages were never
    /// session entries, so a first-kept tree walk could never find them.
    #[tokio::test]
    async fn test_hook_supplied_retained_tail_survives_restore() {
        use crate::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};

        let dir = tempfile::tempdir().unwrap();
        let meta = || JsonlSessionMetadata {
            id: "s".into(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        };

        let expected;
        {
            let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
                .await
                .unwrap();
            let session = Session::new(storage);
            let mut harness = AgentHarness::new(
                session,
                "You are a test assistant.",
                test_model(),
                Arc::new(TestStreamFn),
            );
            harness.on(
                HookPoint::SessionBeforeCompact,
                Arc::new(|ctx: HookContext| {
                    ctx.with_compact_override(BeforeCompactOverride {
                        summary: "hook summary".into(),
                        tokens_before: 90_000,
                        first_kept_entry_id: None,
                        retained_tail: vec![
                            AgentMessage::user("hook-kept question"),
                            AgentMessage::user("hook-kept answer"),
                        ],
                        details: None,
                        usage: None,
                    })
                }),
            );
            harness
                .agent_mut()
                .replace_transcript(compactable_transcript());
            harness.set_compaction_settings(compact_test_settings());
            harness.compact(None).await.unwrap();
            expected = serde_json::to_value(&harness.agent().state().messages).unwrap();
        }

        let storage = JsonlSessionStorage::open(&dir.path().join("session.jsonl"))
            .await
            .unwrap();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness.restore().await.unwrap();

        let messages = &harness.agent().state().messages;
        // Summary carrier + both hook-authored tail messages.
        assert_eq!(messages.len(), 3, "{messages:?}");
        // The restored transcript equals the post-compaction one exactly.
        let restored = serde_json::to_value(messages).unwrap();
        assert_eq!(restored, expected);
    }

    /// Restore projects every message-producing entry variant — messages,
    /// custom messages, branch summaries — and replays the run configuration
    /// the path carries (reasoning tier, model, active tools) without
    /// appending new entries. Display/state entries stay out of the
    /// transcript.
    #[tokio::test]
    async fn test_restore_projects_all_entry_variants_and_settings() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let storage = session.storage();

        storage
            .append_entry(&SessionTreeEntry::ThinkingLevelChange {
                id: "t1".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                thinking_level: "high".into(),
            })
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::ModelChange {
                id: "mc".into(),
                parent_id: Some("t1".into()),
                timestamp: chrono::Utc::now(),
                provider: "anthropic".into(),
                model_id: "claude-opus".into(),
            })
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::Message {
                id: "m1".into(),
                parent_id: Some("mc".into()),
                timestamp: chrono::Utc::now(),
                message: AgentMessage::user("hello"),
            })
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::CustomMessage {
                id: "cm1".into(),
                parent_id: Some("m1".into()),
                timestamp: chrono::Utc::now(),
                custom_type: "notice".into(),
                content: vec![ContentBlock::Text {
                    text: "heads up".into(),
                    signature: None,
                }],
                details: None,
                display: true,
            })
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::BranchSummary {
                id: "bs1".into(),
                parent_id: Some("cm1".into()),
                timestamp: chrono::Utc::now(),
                from_id: "m1".into(),
                summary: "explored a side branch".into(),
                details: None,
                usage: None,
                from_hook: None,
            })
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::Message {
                id: "m2".into(),
                parent_id: Some("bs1".into()),
                timestamp: chrono::Utc::now(),
                message: AgentMessage::user("after"),
            })
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::ActiveToolsChange {
                id: "at1".into(),
                parent_id: Some("m2".into()),
                timestamp: chrono::Utc::now(),
                active_tool_names: vec!["echo".into()],
            })
            .await
            .unwrap();

        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        )
        .with_tools(two_tools())
        .with_model_resolver(test_model_resolver());
        harness.restore().await.unwrap();

        let messages = &harness.agent().state().messages;
        assert_eq!(messages.len(), 4, "{messages:?}");
        assert!(matches!(&messages[0], AgentMessage::User { .. }));
        match &messages[1] {
            AgentMessage::Custom {
                custom_type,
                display,
                ..
            } => {
                assert_eq!(custom_type, "notice");
                assert!(display);
            }
            other => panic!("expected Custom, got {other:?}"),
        }
        match &messages[2] {
            AgentMessage::User { content, .. } => match &content[0] {
                ContentBlock::Text { text, .. } => assert_eq!(
                    text,
                    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\nexplored a side branch</summary>"
                ),
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected User, got {other:?}"),
        }
        assert!(matches!(&messages[3], AgentMessage::User { .. }));

        // The reasoning tier on the path reaches the agent, the resolver
        // swaps in the session's model, and the active-tool selection narrows
        // the mounted set — restore replays the run configuration without
        // appending anything.
        assert_eq!(
            harness.agent().state().thinking_level.as_deref(),
            Some("high")
        );
        assert_eq!(harness.model().id, "claude-opus");
        assert_eq!(harness.agent().state().model.id, "claude-opus");
        assert_eq!(mounted_names(&harness), ["echo"]);
        assert_eq!(harness.active_tool_names(), Some(&["echo".to_string()][..]));
        let entry_count = harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap()
            .len();
        harness.restore().await.unwrap();
        assert_eq!(
            harness
                .session()
                .storage()
                .get_entries()
                .await
                .unwrap()
                .len(),
            entry_count
        );
    }

    /// Without a resolver the session's model reference is unresolvable, so
    /// restore keeps the construction-time model.
    #[tokio::test]
    async fn test_restore_without_resolver_keeps_construction_model() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        session
            .storage()
            .append_entry(&SessionTreeEntry::ModelChange {
                id: "mc".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                provider: "anthropic".into(),
                model_id: "claude-opus".into(),
            })
            .await
            .unwrap();

        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
        harness.restore().await.unwrap();
        assert_eq!(harness.model().id, "test");
        assert_eq!(harness.agent().state().model.id, "test");
    }

    /// `set_model` persists a `model_change` entry, and a restore replays it
    /// through the resolver even after the in-memory model was scrambled.
    #[tokio::test]
    async fn test_set_model_persists_and_replays() {
        let session = Session::new(MemStorage::new());
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        )
        .with_model_resolver(test_model_resolver());

        harness.set_model(resolved_model()).await.unwrap();
        assert_eq!(harness.model().id, "claude-opus");
        assert_eq!(harness.agent().state().model.id, "claude-opus");

        let entries = harness.session().storage().get_entries().await.unwrap();
        match entries.last() {
            Some(SessionTreeEntry::ModelChange {
                provider, model_id, ..
            }) => {
                assert_eq!(provider, "anthropic");
                assert_eq!(model_id, "claude-opus");
            }
            other => panic!("expected a trailing ModelChange, got {other:?}"),
        }

        // Scramble the in-memory model; restore replays the persisted choice.
        harness.agent_mut().set_model(test_model());
        harness.restore().await.unwrap();
        assert_eq!(harness.model().id, "claude-opus");
        assert_eq!(harness.agent().state().model.id, "claude-opus");
    }

    /// `set_active_tools` rejects names outside the mounted set, filters the
    /// agent's tools, persists an `active_tools_change` entry, and a restore
    /// replays the selection after the in-memory set was scrambled.
    #[tokio::test]
    async fn test_set_active_tools_validates_filters_and_replays() {
        let session = Session::new(MemStorage::new());
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        )
        .with_tools(two_tools());
        assert_eq!(mounted_names(&harness), ["echo", "other"]);

        // An unknown name is refused and persists nothing.
        let before = harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap()
            .len();
        let err = harness
            .set_active_tools(vec!["bogus".to_string()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bogus"), "{err:?}");
        assert_eq!(
            harness
                .session()
                .storage()
                .get_entries()
                .await
                .unwrap()
                .len(),
            before
        );
        assert_eq!(mounted_names(&harness), ["echo", "other"]);

        harness
            .set_active_tools(vec!["echo".to_string()])
            .await
            .unwrap();
        assert_eq!(mounted_names(&harness), ["echo"]);
        let entries = harness.session().storage().get_entries().await.unwrap();
        match entries.last() {
            Some(SessionTreeEntry::ActiveToolsChange {
                active_tool_names, ..
            }) => assert_eq!(active_tool_names, &["echo".to_string()]),
            other => panic!("expected a trailing ActiveToolsChange, got {other:?}"),
        }

        // Scramble the in-memory tool list; restore replays the selection.
        harness.agent_mut().set_tools(two_tools());
        harness.restore().await.unwrap();
        assert_eq!(mounted_names(&harness), ["echo"]);
    }

    #[tokio::test]
    async fn test_persist_failure_on_first_message_reverts_to_empty() {
        let storage = MemStorage::new();
        *storage.fail_at_call.lock().unwrap() = 1;
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        let err = harness.prompt("hi").await.unwrap_err();
        assert!(
            err.to_string().contains("failed to persist messages"),
            "{err:#}"
        );
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);

        // Nothing persisted, so the reverted transcript is empty too —
        // identical to what a fresh harness would restore from this session.
        assert!(harness.agent().state().messages.is_empty());
        assert!(
            harness
                .session()
                .build_context_entries()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn test_persist_failure_mid_turn_keeps_only_the_persisted_prefix() {
        let storage = MemStorage::new();
        *storage.fail_at_call.lock().unwrap() = 2;
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        // The user message persists; the assistant reply does not.
        let err = harness.prompt("hi").await.unwrap_err();
        assert!(
            err.to_string().contains("failed to persist messages"),
            "{err:#}"
        );
        assert_eq!(harness.phase(), AgentHarnessPhase::Idle);

        // Both views hold exactly the persisted prefix: the pending user
        // message, and nothing else.
        let messages = &harness.agent().state().messages;
        assert_eq!(messages.len(), 1, "{messages:?}");
        assert!(matches!(&messages[0], AgentMessage::User { .. }));
        assert_eq!(
            harness
                .session()
                .build_context_entries()
                .await
                .unwrap()
                .len(),
            1
        );

        // With the failure spent, continuing answers the pending user
        // message — the conversation continues coherently, not forked.
        let produced = harness.continue_().await.unwrap();
        assert!(
            produced
                .iter()
                .any(|m| matches!(m, AgentMessage::Assistant { .. }))
        );
        assert_eq!(harness.agent().state().messages.len(), 2);
        assert_eq!(
            harness
                .session()
                .build_context_entries()
                .await
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn test_harness_hooks() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        use std::sync::atomic::{AtomicBool, Ordering};
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = Arc::clone(&called);

        harness.on(
            HookPoint::BeforeAgentStart,
            Arc::new(move |ctx| {
                called_clone.store(true, Ordering::SeqCst);
                ctx
            }),
        );

        let _ = harness.prompt("Hello").await;
        assert!(called.load(Ordering::SeqCst));
    }

    // A tool that echoes its `message` arg, used to exercise the tool hook
    // path end-to-end through the harness's real ToolContext.
    struct EchoTool;

    #[async_trait::async_trait]
    impl crate::tool::AgentTool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "Echoes the input"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"message": {"type": "string"}}})
        }
        async fn execute(
            &self,
            _id: &str,
            params: serde_json::Value,
            _signal: CancellationToken,
            _ctx: &dyn crate::tool::ToolContext,
        ) -> Result<crate::tool::AgentToolResult, crate::tool::ToolError> {
            let msg = params["message"].as_str().unwrap_or("no message");
            Ok(crate::tool::AgentToolResult::text(msg))
        }
    }

    // A tool whose only distinguishing feature is its name, for exercising
    // the active-tool filter.
    struct NamedTool(&'static str);

    #[async_trait::async_trait]
    impl crate::tool::AgentTool for NamedTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "named"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(
            &self,
            _id: &str,
            _params: serde_json::Value,
            _signal: CancellationToken,
            _ctx: &dyn crate::tool::ToolContext,
        ) -> Result<crate::tool::AgentToolResult, crate::tool::ToolError> {
            Ok(crate::tool::AgentToolResult::text("ok"))
        }
    }

    /// The two-tool mounted set used by active-tool tests: `echo` plus a
    /// second name to filter away.
    fn two_tools() -> Arc<[Arc<dyn crate::tool::AgentTool>]> {
        Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>,
            Arc::new(NamedTool("other")),
        ])
    }

    fn mounted_names(harness: &AgentHarness<MemStorage>) -> Vec<String> {
        harness
            .agent()
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect()
    }

    // A stream fn that issues one tool call then stops, so a single prompt
    // drives a full tool-execution round through the harness.
    struct ToolUseStreamFn {
        call: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl StreamFn for ToolUseStreamFn {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            let n = self.call.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                Ok(AgentMessage::Assistant {
                    content: vec![crate::types::ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "echo".into(),
                        input: serde_json::json!({"message": "hi"}),
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
                })
            } else {
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "done".into(),
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
    }

    #[tokio::test]
    async fn before_provider_request_hook_fires_on_prompt() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        use std::sync::atomic::{AtomicUsize, Ordering};
        let count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&count);
        harness.on(
            HookPoint::BeforeProviderRequest,
            Arc::new(move |ctx| {
                count_clone.fetch_add(1, Ordering::SeqCst);
                assert!(ctx.agent_context.is_some());
                ctx
            }),
        );

        let _ = harness.prompt("Hello").await.unwrap();
        assert!(
            count.load(Ordering::SeqCst) >= 1,
            "BeforeProviderRequest must fire when the provider is called"
        );
    }

    #[tokio::test]
    async fn tool_call_and_tool_result_hooks_fire_during_execution() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        use std::sync::atomic::{AtomicUsize, Ordering};
        let tool_call = Arc::new(AtomicUsize::new(0));
        let tool_result = Arc::new(AtomicUsize::new(0));
        let tc = Arc::clone(&tool_call);
        let tr = Arc::clone(&tool_result);
        harness.on(
            HookPoint::ToolCall,
            Arc::new(move |ctx| {
                if ctx.data.get("tool_name").and_then(|v| v.as_str()) == Some("echo") {
                    tc.fetch_add(1, Ordering::SeqCst);
                }
                ctx
            }),
        );
        harness.on(
            HookPoint::ToolResult,
            Arc::new(move |ctx| {
                tr.fetch_add(1, Ordering::SeqCst);
                let _ = ctx;
                ctx
            }),
        );

        let _ = harness.prompt("run the echo tool").await.unwrap();
        assert_eq!(
            tool_call.load(Ordering::SeqCst),
            1,
            "ToolCall hook must fire once for the echo call"
        );
        assert_eq!(
            tool_result.load(Ordering::SeqCst),
            1,
            "ToolResult hook must fire once for the echo result"
        );
    }

    #[tokio::test]
    async fn before_provider_request_mutation_reaches_the_provider() {
        // A stream fn that records the context it actually received.
        let received: Arc<std::sync::Mutex<Vec<AgentMessage>>> = Arc::default();
        let captured = Arc::clone(&received);
        struct CaptureStreamFn {
            captured: Arc<std::sync::Mutex<Vec<AgentMessage>>>,
        }
        #[async_trait::async_trait]
        impl StreamFn for CaptureStreamFn {
            async fn stream(
                &self,
                context: &AgentContext,
                _signal: CancellationToken,
                _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
            ) -> Result<AgentMessage, anyhow::Error> {
                *self.captured.lock().unwrap() = context.messages.clone();
                Ok(AgentMessage::Assistant {
                    content: vec![ContentBlock::Text {
                        text: "ok".into(),
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

        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(CaptureStreamFn { captured }),
        );

        // The hook appends a sentinel user message to the context the
        // provider is about to see.
        harness.on(
            HookPoint::BeforeProviderRequest,
            Arc::new(|mut ctx: HookContext| {
                let mut ac = ctx.agent_context.take().expect("context present");
                ac.messages.push(AgentMessage::user("SENTINEL_FROM_HOOK"));
                ctx.agent_context = Some(ac);
                ctx
            }),
        );

        let _ = harness.prompt("hello").await.unwrap();
        let received = received.lock().unwrap();
        assert!(
            received
                .iter()
                .any(|m| matches!(m, AgentMessage::User { content, .. } if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "SENTINEL_FROM_HOOK")))),
            "a mutated context from BeforeProviderRequest must reach the provider"
        );
    }

    #[tokio::test]
    async fn tool_call_hook_can_block_execution() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        // Block every tool call with a reason.
        harness.on(
            HookPoint::ToolCall,
            Arc::new(|ctx: HookContext| ctx.with_block_reason("denied by test hook")),
        );

        let messages = harness.prompt("run the echo tool").await.unwrap();
        // The blocked call surfaces as an error tool result carrying the reason.
        let blocked = messages.iter().any(|m| matches!(m, AgentMessage::ToolResult { is_error, content, .. } if *is_error && content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text.contains("denied by test hook")))));
        assert!(
            blocked,
            "a ToolCall hook returning a block reason must abort the call"
        );
    }

    #[tokio::test]
    async fn tool_result_hook_can_patch_the_result() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(ToolUseStreamFn {
                call: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }),
        )
        .with_tools(Arc::from(vec![
            Arc::new(EchoTool) as Arc<dyn crate::tool::AgentTool>
        ]));

        // Replace whatever the tool produced with a fixed patched payload.
        harness.on(
            HookPoint::ToolResult,
            Arc::new(|ctx: HookContext| {
                ctx.with_tool_result(crate::tool::AgentToolResult::text("PATCHED_BY_HOOK"))
            }),
        );

        let messages = harness.prompt("run the echo tool").await.unwrap();
        let patched = messages.iter().any(|m| matches!(m, AgentMessage::ToolResult { content, .. } if content.iter().any(|b| matches!(b, ContentBlock::Text { text, .. } if text == "PATCHED_BY_HOOK"))));
        assert!(
            patched,
            "a ToolResult hook returning a replacement must patch the result"
        );
    }

    /// A stream that records the system prompt of every context it sees, so
    /// tests can tell which prompt each provider call carried.
    struct SystemPromptSpy {
        seen: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl StreamFn for SystemPromptSpy {
        async fn stream(
            &self,
            context: &AgentContext,
            _signal: CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            self.seen
                .lock()
                .unwrap()
                .push(context.system_prompt.clone());
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "spy response".into(),
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

    #[tokio::test]
    async fn before_agent_start_messages_join_the_prompt_batch() {
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );

        harness.on(
            HookPoint::BeforeAgentStart,
            Arc::new(|ctx: HookContext| {
                ctx.with_inject_messages(vec![AgentMessage::user("INJECTED_BY_HOOK")])
            }),
        );

        let messages = harness.prompt("Hello").await.unwrap();
        let positions: Vec<&str> = messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::User { content, .. } => content.iter().find_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(
            positions,
            ["Hello", "INJECTED_BY_HOOK"],
            "hook messages are appended after the user message, before the response"
        );

        // Injected messages persist like any prompt message.
        let entries = harness.session().storage().get_entries().await.unwrap();
        let persisted: Vec<&str> = entries
            .iter()
            .filter_map(|e| match e {
                SessionTreeEntry::Message {
                    message: AgentMessage::User { content, .. },
                    ..
                } => content.iter().find_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(persisted, ["Hello", "INJECTED_BY_HOOK"]);
    }

    #[tokio::test]
    async fn before_agent_start_system_prompt_reaches_the_first_context_only() {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "base prompt",
            test_model(),
            Arc::new(SystemPromptSpy {
                seen: Arc::clone(&seen),
            }),
        );

        harness.on(
            HookPoint::BeforeAgentStart,
            Arc::new(|ctx: HookContext| ctx.with_system_prompt("override prompt")),
        );

        harness.prompt("first").await.unwrap();
        harness.prompt("second").await.unwrap();

        // The hook fires on every prompt, so both runs see the override in
        // their initial context...
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            ["override prompt", "override prompt"]
        );
        // ...but the agent state is untouched between runs: an override never
        // becomes the configured prompt.
        assert_eq!(harness.agent().state().system_prompt, "base prompt");
    }

    #[tokio::test]
    async fn before_agent_start_without_override_keeps_the_original_prompt() {
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let storage = MemStorage::new();
        let session = Session::new(storage);
        let mut harness = AgentHarness::new(
            session,
            "base prompt",
            test_model(),
            Arc::new(SystemPromptSpy {
                seen: Arc::clone(&seen),
            }),
        );

        // A handler that observes the event but returns no result fields.
        harness.on(
            HookPoint::BeforeAgentStart,
            Arc::new(|ctx: HookContext| ctx),
        );

        let messages = harness.prompt("Hello").await.unwrap();
        assert_eq!(messages.len(), 2, "user message plus the response");
        assert_eq!(seen.lock().unwrap().as_slice(), ["base prompt"]);
    }
}
