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
use crate::compaction::{self, CompactionResult, CompactionSettings};
use crate::env::{ExecutionEnv, TokioExecutionEnv};
use crate::session::{Session, SessionStorage};
use crate::tool::{AgentToolResult, LocalToolContext, ToolState};
use crate::types::{
    AgentContext, AgentMessage, CacheRetention, ContentBlock, Model, StopReason, Usage,
};
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

/// Context passed to hook handlers.
///
/// Handlers return a (possibly mutated) copy; the harness threads selected
/// fields back into the loop. `agent_context` feeds the provider request,
/// `block_reason` gates a tool call, and `tool_result` patches a tool result.
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
}

impl HookContext {
    pub fn new(hook: HookPoint) -> Self {
        HookContext {
            hook,
            agent_context: None,
            data: serde_json::Value::Null,
            block_reason: None,
            tool_result: None,
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

        // Run before-agent-start hooks.
        let _hook_ctx = self.run_hooks(
            HookPoint::BeforeAgentStart,
            HookContext::new(HookPoint::BeforeAgentStart)
                .with_data(serde_json::json!({"text": text})),
        );

        let result = self.agent.prompt(text).await;

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
    /// The context walk stops at the latest compaction boundary; that entry
    /// contributes the summary message and its retained tail, and later
    /// entries contribute their messages verbatim. The compaction boundary
    /// used by token estimation is recovered alongside.
    pub async fn restore(&mut self) -> Result<(), anyhow::Error> {
        let entries = self.session.build_context().await?;
        let mut messages = Vec::new();
        let mut entry_ids: Vec<Option<String>> = Vec::new();
        for entry in &entries {
            match entry {
                crate::session::SessionTreeEntry::Compaction {
                    summary,
                    retained_tail,
                    timestamp,
                    ..
                } => {
                    // The summary is a synthetic carrier; the retained tail is
                    // folded into the entry, so neither has a standalone id.
                    entry_ids.push(None);
                    for _ in retained_tail {
                        entry_ids.push(None);
                    }
                    messages.push(summary_message(summary, *timestamp));
                    messages.extend(retained_tail.iter().cloned());
                }
                crate::session::SessionTreeEntry::Message { id, message, .. } => {
                    entry_ids.push(Some(id.clone()));
                    messages.push(message.clone());
                }
                _ => {}
            }
        }
        self.agent.reset();
        self.agent.replace_transcript(messages);
        self.message_entry_ids = entry_ids;
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
    /// the summarization usage, and the retained tail. The agent transcript is
    /// rewritten to the summary message plus the kept tail.
    pub async fn compact(&mut self) -> Result<CompactionResult, anyhow::Error> {
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!("Cannot compact while harness is in {:?} phase", self.phase);
        }
        if self.agent.state().messages.is_empty() {
            anyhow::bail!("Cannot compact an empty transcript");
        }

        self.phase = AgentHarnessPhase::Compaction;

        // Run before-compact hooks.
        let _hook_ctx = self.run_hooks(
            HookPoint::SessionBeforeCompact,
            HookContext::new(HookPoint::SessionBeforeCompact),
        );

        let messages = self.agent.state().messages.clone();
        let tokens_before = compaction::estimate_context_tokens(&messages).tokens;
        let cut_point =
            compaction::find_cut_point(&messages, self.compaction_settings.keep_recent_tokens);
        let compacted = &messages[..cut_point];
        let kept = &messages[cut_point..];

        let prompt = compaction::build_compaction_prompt(compacted, None);
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
        // A failed summarization must not persist an empty compaction: that
        // would replace the compacted prefix with nothing and lose history.
        // An Error/Aborted terminal or an empty summary is a failure; bail
        // before touching the session or transcript so both stay intact.
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
        let first_kept_entry_id = self.message_entry_ids.get(cut_point).cloned().flatten();

        // Persist the boundary first — the session is the durable record and
        // a failure here leaves the agent transcript untouched.
        let boundary = match self
            .session
            .append_compaction(
                &summary_text,
                first_kept_entry_id.clone(),
                tokens_before,
                usage.clone(),
                kept.to_vec(),
            )
            .await
        {
            Ok((_id, timestamp)) => timestamp,
            Err(e) => {
                self.phase = AgentHarnessPhase::Idle;
                return Err(e);
            }
        };

        // Rebuild the transcript: summary as context + kept messages. The
        // summary message carries the boundary instant, so a transcript
        // rebuilt from storage equals this one exactly.
        let mut new_messages = Vec::with_capacity(kept.len() + 1);
        new_messages.push(summary_message(&summary_text, boundary));
        new_messages.extend_from_slice(kept);

        self.agent.reset();
        self.agent.replace_transcript(new_messages);
        // The summary is synthetic; kept messages retain their entry ids.
        let mut new_ids: Vec<Option<String>> = Vec::with_capacity(kept.len() + 1);
        new_ids.push(None);
        new_ids.extend(self.message_entry_ids[cut_point..].iter().cloned());
        self.message_entry_ids = new_ids;

        self.last_compaction_at = Some(boundary);
        let tokens_after = self.estimate_current_tokens();

        let result = CompactionResult {
            summary: summary_text,
            first_kept_entry_id,
            tokens_before,
            tokens_after,
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
        let prompt = compaction::build_compaction_prompt(compacted, None);
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

/// The in-transcript carrier for a compaction summary: a tagged user
/// message. Kept symmetric between compaction and restore so the summary
/// reads identically whether it was just written or rebuilt from storage.
fn summary_message(summary: &str, timestamp: chrono::DateTime<chrono::Utc>) -> AgentMessage {
    AgentMessage::User {
        content: vec![crate::types::ContentBlock::Text {
            text: format!(
                "<conversation_history_summary>\n{summary}\n</conversation_history_summary>"
            ),
            signature: None,
        }],
        timestamp,
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
            self.entries.lock().unwrap().push(entry.clone());
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
        async fn get_path_to_root_or_compaction(
            &self,
            _leaf_id: Option<&str>,
        ) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
            Ok(self.entries.lock().unwrap().clone())
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
        let result = harness.compact().await.unwrap();
        assert!(
            result.tokens_after < 1_000,
            "tokens_after={}",
            result.tokens_after
        );
        assert!(!harness.needs_compaction());
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

        let err = harness.compact().await.unwrap_err();
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

        let result = harness.compact().await.expect("compact must not deadlock");
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
            let storage = JsonlSessionStorage::open(dir.path(), meta()).await.unwrap();
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
            let result = harness.compact().await.unwrap();
            assert_eq!(result.tokens_before, 90_000);
        }

        // Reopen the session from disk: the compaction entry survived, with
        // the retained tail embedded for a future context rebuild.
        let storage = JsonlSessionStorage::open(dir.path(), meta()).await.unwrap();
        let entries = storage.get_entries().await.unwrap();
        let boundary = entries.iter().find_map(|e| match e {
            SessionTreeEntry::Compaction {
                summary,
                tokens_before,
                retained_tail,
                ..
            } => Some((summary.clone(), *tokens_before, retained_tail.len())),
            _ => None,
        });
        assert_eq!(boundary, Some(("Test response".to_string(), 90_000, 2)));

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
        let storage = JsonlSessionStorage::open(dir.path(), meta()).await.unwrap();
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
        };

        // Run a turn, compact, run another turn — all over an on-disk session.
        let expected;
        {
            let storage = JsonlSessionStorage::open(dir.path(), meta()).await.unwrap();
            let session = Session::new(storage);
            let mut harness = AgentHarness::new(
                session,
                "You are a test assistant.",
                test_model(),
                Arc::new(TestStreamFn),
            );
            harness.prompt("first").await.unwrap();
            harness.compact().await.unwrap();
            harness.prompt("second").await.unwrap();
            expected = serde_json::to_value(&harness.agent().state().messages).unwrap();
        }

        // A fresh harness restores the full transcript: summary, retained
        // tail, and the post-compaction messages.
        let storage = JsonlSessionStorage::open(dir.path(), meta()).await.unwrap();
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
            "<conversation_history_summary>\nTest response\n</conversation_history_summary>"
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
        assert!(harness.session().build_context().await.unwrap().is_empty());
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
        assert_eq!(harness.session().build_context().await.unwrap().len(), 1);

        // With the failure spent, continuing answers the pending user
        // message — the conversation continues coherently, not forked.
        let produced = harness.continue_().await.unwrap();
        assert!(
            produced
                .iter()
                .any(|m| matches!(m, AgentMessage::Assistant { .. }))
        );
        assert_eq!(harness.agent().state().messages.len(), 2);
        assert_eq!(harness.session().build_context().await.unwrap().len(), 2);
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
}
