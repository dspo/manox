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
        }
    }

    /// Mount tools on the underlying agent.
    pub fn with_tools(mut self, tools: Arc<[Box<dyn crate::tool::AgentTool>]>) -> Self {
        self.agent.set_tools(tools);
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
    pub async fn prompt(&mut self, text: &str) -> Result<Vec<AgentMessage>, anyhow::Error> {
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!("Cannot prompt while harness is in {:?} phase", self.phase);
        }

        self.phase = AgentHarnessPhase::Turn;

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
                // Persist messages to session, tracking the entry id of each
                // so `compact()` can record the real first-kept entry.
                let mut persist_result = Ok(());
                for msg in &messages {
                    match self.session.append_message(msg.clone()).await {
                        Ok(id) => self.message_entry_ids.push(Some(id)),
                        Err(e) => {
                            persist_result = Err(e);
                            break;
                        }
                    }
                }

                self.phase = AgentHarnessPhase::Idle;
                if let Err(e) = persist_result {
                    return Err(self.revert_transcript_after_persist_failure(e).await);
                }

                // Check if compaction is needed.
                let context_tokens = self.estimate_current_tokens();
                if compaction::should_compact(
                    context_tokens,
                    self.model.context_window as u64,
                    &self.compaction_settings,
                ) {
                    // Compaction is needed — caller should invoke compact().
                    // We don't auto-compact to avoid surprise latency.
                }

                Ok(messages)
            }
            Err(e) => {
                self.phase = AgentHarnessPhase::Idle;
                Err(e)
            }
        }
    }

    /// Continue from the current transcript.
    pub async fn continue_(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!("Cannot continue while harness is in {:?} phase", self.phase);
        }

        self.phase = AgentHarnessPhase::Turn;
        let result = self.agent.continue_().await;

        match result {
            Ok(messages) => {
                let mut persist_result = Ok(());
                for msg in &messages {
                    match self.session.append_message(msg.clone()).await {
                        Ok(id) => self.message_entry_ids.push(Some(id)),
                        Err(e) => {
                            persist_result = Err(e);
                            break;
                        }
                    }
                }
                self.phase = AgentHarnessPhase::Idle;
                if let Err(e) = persist_result {
                    return Err(self.revert_transcript_after_persist_failure(e).await);
                }
                Ok(messages)
            }
            Err(e) => {
                self.phase = AgentHarnessPhase::Idle;
                Err(e)
            }
        }
    }

    /// Abort the current agent run.
    pub fn abort(&mut self) {
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
    /// itself. The reasoning tier the path carries is applied to the agent;
    /// the model the path carries is reported by the session context but not
    /// applied — resolving it needs the provider registry, which lives at the
    /// facade layer. The compaction boundary used by token estimation is
    /// recovered alongside.
    pub async fn restore(&mut self) -> Result<(), anyhow::Error> {
        let context = self.session.build_session_context().await?;
        self.agent.reset();
        self.agent.replace_transcript(context.messages);
        self.agent.set_thinking_level(context.thinking_level);
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

        self.phase = AgentHarnessPhase::Compaction;

        // The session branch the harness is compacting — the same entries TS
        // exposes as `branchEntries` on the `session_before_compact` event:
        // the full path to the root, across compaction boundaries.
        let branch_entries = match self.session.get_branch().await {
            Ok(entries) => entries,
            Err(e) => {
                self.phase = AgentHarnessPhase::Idle;
                return Err(e);
            }
        };

        let messages = self.agent.state().messages.clone();
        let tokens_before = compaction::estimate_context_tokens(&messages).tokens;
        let cut_point =
            compaction::find_cut_point(&messages, self.compaction_settings.keep_recent_tokens);
        let kept = &messages[cut_point..];
        let first_kept_entry_id = self.message_entry_ids.get(cut_point).cloned().flatten();

        // The hook fires after the cut analysis — mirroring TS, which prepares
        // the compaction then emits the event with `preparation` +
        // `branchEntries` — so the handler decides on the specific content.
        // The typed event carries the full TS `CompactionPreparation`
        // (split-turn is always false here) plus the session branch and custom
        // instructions, rather than a trimmed ad-hoc payload.
        let preparation = compaction::build_preparation(
            &branch_entries,
            &messages,
            cut_point,
            first_kept_entry_id.clone(),
            tokens_before,
            &self.compaction_settings,
        );
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
        // a failure here leaves the agent transcript untouched.
        let boundary = match self
            .session
            .append_compaction(
                &summary_text,
                first_kept_entry_id.clone(),
                tokens_before,
                usage.clone(),
                authorship,
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

        self.agent.reset();
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
            system_prompt:
                "You compress a coding agent's conversation history into a concise summary.".into(),
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
    /// summary, along with the cut point index.
    pub fn build_compaction_prompt(&self) -> Option<(String, usize)> {
        let messages = self.agent.state().messages.clone();
        if messages.is_empty() {
            return None;
        }

        let cut_point =
            compaction::find_cut_point(&messages, self.compaction_settings.keep_recent_tokens);

        if cut_point == 0 {
            return None; // Nothing to compact.
        }

        let compacted = &messages[..cut_point];
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
            // the same unknown-leaf fallback. Returning every entry
            // regardless of the cursor would mask path-relative logic (e.g.
            // `previousSummary` extraction).
            let entries = self.entries.lock().unwrap();
            let target_id = match leaf_id {
                None => return Ok(Vec::new()),
                Some(id) if entries.iter().any(|e| e.id() == id) => id.to_string(),
                Some(_) => match entries.last() {
                    Some(e) => e.id().to_string(),
                    None => return Ok(Vec::new()),
                },
            };
            let mut index: std::collections::HashMap<&str, &SessionTreeEntry> =
                entries.iter().map(|e| (e.id(), e)).collect();
            let mut path: Vec<&SessionTreeEntry> = Vec::new();
            let mut current_id: Option<&str> = Some(&target_id);
            while let Some(id) = current_id {
                let entry = match index.remove(id) {
                    Some(e) => e,
                    None => break,
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

        // An assistant whose usage exceeds the threshold anchors the estimate.
        let assistant = AgentMessage::Assistant {
            content: vec![],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage {
                total_tokens: 90_000,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        harness
            .agent_mut()
            .replace_transcript(vec![AgentMessage::user("q"), assistant]);
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

        let assistant = AgentMessage::Assistant {
            content: vec![],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage {
                total_tokens: 90_000,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        harness
            .agent_mut()
            .replace_transcript(vec![AgentMessage::user("q"), assistant]);
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

        let assistant = AgentMessage::Assistant {
            content: vec![],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage {
                total_tokens: 90_000,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        harness
            .agent_mut()
            .replace_transcript(vec![AgentMessage::user("q"), assistant]);
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
        let assistant = AgentMessage::Assistant {
            content: vec![],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage {
                total_tokens: 90_000,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        harness
            .agent_mut()
            .replace_transcript(vec![AgentMessage::user("q"), assistant]);
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
        assert_eq!(harness.agent_mut().state().messages.len(), 2);
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
        let assistant = AgentMessage::Assistant {
            content: vec![],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage {
                total_tokens: 90_000,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        harness
            .agent_mut()
            .replace_transcript(vec![AgentMessage::user("q"), assistant]);

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
        // The prior summary is folded in once as <previous-summary>-style context.
        assert!(
            prompt.contains("Here is a summary of the earlier conversation"),
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
        let assistant = AgentMessage::Assistant {
            content: vec![],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage {
                total_tokens: 90_000,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        harness
            .agent_mut()
            .replace_transcript(vec![AgentMessage::user("q"), assistant]);
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
        assert_eq!(harness.agent_mut().state().messages.len(), 2);
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
        let assistant = AgentMessage::Assistant {
            content: vec![],
            model: "test".into(),
            provider: "test".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(StopReason::Stop),
            usage: Box::new(Usage {
                total_tokens: 90_000,
                ..Default::default()
            }),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        harness
            .agent_mut()
            .replace_transcript(vec![AgentMessage::user("q"), assistant]);

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
                .replace_transcript(vec![AgentMessage::user("q"), stale_assistant.clone()]);
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

        // Run a turn, compact, run another turn — all over an on-disk session.
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
            harness.prompt("first").await.unwrap();
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
        assert_eq!(messages.len(), 5, "{messages:?}");
        assert_eq!(
            text_of(&messages[0]),
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\nTest response\n</summary>"
        );
        assert_eq!(text_of(&messages[1]), "first");
        assert!(matches!(&messages[2], AgentMessage::Assistant { .. }));
        assert_eq!(text_of(&messages[3]), "second");
        assert!(matches!(&messages[4], AgentMessage::Assistant { .. }));

        // The restored transcript equals the post-compaction one exactly,
        // summary timestamp included.
        let restored = serde_json::to_value(messages).unwrap();
        assert_eq!(restored, expected);

        // The estimation boundary came along: needs_compaction works without
        // a separate recover_boundary() call.
        assert!(!harness.needs_compaction());
    }

    /// Restore projects every message-producing entry variant — messages,
    /// custom messages, branch summaries — and applies the reasoning tier the
    /// path carries. Display/state entries stay out of the transcript.
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

        let mut harness = AgentHarness::new(
            session,
            "You are a test assistant.",
            test_model(),
            Arc::new(TestStreamFn),
        );
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

        // The reasoning tier on the path reaches the agent; the model stays
        // untouched (resolving it is the facade layer's job).
        assert_eq!(
            harness.agent().state().thinking_level.as_deref(),
            Some("high")
        );
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
            Box::new(EchoTool) as Box<dyn crate::tool::AgentTool>
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
            Box::new(EchoTool) as Box<dyn crate::tool::AgentTool>
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
            Box::new(EchoTool) as Box<dyn crate::tool::AgentTool>
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
