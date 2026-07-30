// AgentHarness — orchestration layer.
//
// Wraps the agent loop with session persistence, hooks, compaction
// integration, and phase management. This is the primary public API
// for consumers of the harness.

use std::sync::Arc;

use crate::agent::Agent;
use crate::agent_loop::StreamFn;
use crate::compaction::{self, CompactionSettings, CompactionResult};
use crate::session::{Session, SessionStorage};
use crate::types::{AgentMessage, AgentContext, Model};

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
#[derive(Debug, Clone)]
pub struct HookContext {
    /// The hook point being triggered.
    pub hook: HookPoint,
    /// The current agent context (messages, model, etc.).
    pub agent_context: Option<AgentContext>,
    /// Arbitrary data attached to the hook event.
    pub data: serde_json::Value,
}

impl HookContext {
    pub fn new(hook: HookPoint) -> Self {
        HookContext {
            hook,
            agent_context: None,
            data: serde_json::Value::Null,
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
    hooks: Vec<(HookPoint, HookHandler)>,
}

impl<S: SessionStorage> AgentHarness<S> {
    /// Create a new harness with the given session, model, and stream function.
    pub fn new(
        session: Session<S>,
        system_prompt: impl Into<String>,
        model: Model,
        stream_fn: Arc<dyn StreamFn>,
    ) -> Self {
        let agent = Agent::new(system_prompt, model.clone(), stream_fn);
        AgentHarness {
            agent,
            session,
            model,
            phase: AgentHarnessPhase::Idle,
            compaction_settings: CompactionSettings::default(),
            last_compaction_at: None,
            hooks: Vec::new(),
        }
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
        self.hooks.push((hook, handler));
    }

    /// Run all registered hooks for a given point.
    fn run_hooks(&self, hook: HookPoint, mut ctx: HookContext) -> HookContext {
        for (point, handler) in &self.hooks {
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
    pub async fn prompt(
        &mut self,
        text: &str,
    ) -> Result<Vec<AgentMessage>, anyhow::Error> {
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!(
                "Cannot prompt while harness is in {:?} phase",
                self.phase
            );
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
                // Persist messages to session.
                for msg in &messages {
                    self.session.append_message(msg.clone()).await?;
                }

                self.phase = AgentHarnessPhase::Idle;

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
            anyhow::bail!(
                "Cannot continue while harness is in {:?} phase",
                self.phase
            );
        }

        self.phase = AgentHarnessPhase::Turn;
        let result = self.agent.continue_().await;

        match result {
            Ok(messages) => {
                for msg in &messages {
                    self.session.append_message(msg.clone()).await?;
                }
                self.phase = AgentHarnessPhase::Idle;
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
        for entry in &entries {
            match entry {
                crate::session::SessionTreeEntry::Compaction {
                    summary,
                    retained_tail,
                    timestamp,
                    ..
                } => {
                    messages.push(summary_message(summary, *timestamp));
                    messages.extend(retained_tail.iter().cloned());
                }
                crate::session::SessionTreeEntry::Message { message, .. } => {
                    messages.push(message.clone());
                }
                _ => {}
            }
        }
        self.agent.reset();
        self.agent.replace_transcript(messages);
        self.recover_boundary().await?;
        Ok(())
    }

    /// Check whether compaction is needed based on current context size.
    pub fn needs_compaction(&self) -> bool {        let tokens = self.estimate_current_tokens();
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
    /// This finds the cut point, builds a compaction prompt, and returns
    /// the compaction result. The caller is responsible for calling the
    /// LLM with the compaction prompt and providing the summary.
    pub async fn compact(
        &mut self,
        summary: &str,
    ) -> Result<CompactionResult, anyhow::Error> {
        if self.phase != AgentHarnessPhase::Idle {
            anyhow::bail!(
                "Cannot compact while harness is in {:?} phase",
                self.phase
            );
        }

        self.phase = AgentHarnessPhase::Compaction;

        // Run before-compact hooks.
        let _hook_ctx = self.run_hooks(
            HookPoint::SessionBeforeCompact,
            HookContext::new(HookPoint::SessionBeforeCompact),
        );

        let messages = self.agent.state().messages.clone();
        let tokens_before = compaction::estimate_context_tokens(&messages).tokens;

        let cut_point = compaction::find_cut_point(
            &messages,
            self.compaction_settings.keep_recent_tokens,
        );

        let _compacted = &messages[..cut_point];
        let kept = &messages[cut_point..];

        // Build a new transcript: summary as system context + kept messages.
        let mut new_messages = Vec::new();
        new_messages.push(summary_message(summary, chrono::Utc::now()));
        new_messages.extend_from_slice(kept);

        // Replace the agent's transcript. The boundary is stamped before the
        // post-compaction count so the stale retained usage cannot anchor it.
        self.agent.reset();
        self.agent.replace_transcript(new_messages);
        self.last_compaction_at = Some(chrono::Utc::now());
        let tokens_after = self.estimate_current_tokens();

        // Persist the boundary so a rebuilt session stops at this point,
        // with the retained tail embedded for the rebuild.
        if let Err(e) = self
            .session
            .append_compaction(summary, None, tokens_before, kept.to_vec())
            .await
        {
            self.phase = AgentHarnessPhase::Idle;
            return Err(e);
        }

        let result = CompactionResult {
            summary: summary.to_string(),
            first_kept_entry_id: kept.first().map(|_| "compacted".to_string()),
            tokens_before,
            tokens_after,
        };

        // Run after-compact hooks.
        let _hook_ctx = self.run_hooks(
            HookPoint::SessionAfterCompact,
            HookContext::new(HookPoint::SessionAfterCompact)
                .with_data(serde_json::json!({
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

        let cut_point = compaction::find_cut_point(
            &messages,
            self.compaction_settings.keep_recent_tokens,
        );

        if cut_point == 0 {
            return None; // Nothing to compact.
        }

        let compacted = &messages[..cut_point];
        let prompt = compaction::build_compaction_prompt(compacted, None);
        Some((prompt, cut_point))
    }
}

/// The in-transcript carrier for a compaction summary: a tagged user
/// message. Kept symmetric between compaction and restore so the summary
/// reads identically whether it was just written or rebuilt from storage.
fn summary_message(
    summary: &str,
    timestamp: chrono::DateTime<chrono::Utc>,
) -> AgentMessage {
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
            max_tokens: 8_192,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        }
    }

    // In-memory session storage for testing.
    struct MemStorage {
        entries: std::sync::Mutex<Vec<SessionTreeEntry>>,
        leaf_id: std::sync::Mutex<Option<String>>,
    }

    impl MemStorage {
        fn new() -> Self {
            MemStorage {
                entries: std::sync::Mutex::new(Vec::new()),
                leaf_id: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl SessionStorage for MemStorage {
        async fn create_entry_id(&self) -> Result<String, anyhow::Error> {
            Ok(uuid::Uuid::new_v4().to_string())
        }
        async fn append_entry(&self, entry: &SessionTreeEntry) -> Result<(), anyhow::Error> {
            self.entries.lock().unwrap().push(entry.clone());
            Ok(())
        }
        async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, anyhow::Error> {
            Ok(self.entries.lock().unwrap().iter().find(|e| e.id() == id).cloned())
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
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage {
                total_tokens: 90_000,
                ..Default::default()
            },
            timestamp: chrono::Utc::now(),
        };
        harness
            .agent_mut()
            .replace_transcript(vec![AgentMessage::user("q"), assistant]);
        assert!(harness.needs_compaction());

        // After compaction the retained usage measured a different prefix, so
        // the post-compaction count is the plain character heuristic and the
        // harness does not immediately ask for another compaction.
        let result = harness.compact("summary").await.unwrap();
        assert!(
            result.tokens_after < 1_000,
            "tokens_after={}",
            result.tokens_after
        );
        assert!(!harness.needs_compaction());
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
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage {
                total_tokens: 90_000,
                ..Default::default()
            },
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
            let result = harness.compact("summary").await.unwrap();
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
        assert_eq!(boundary, Some(("summary".to_string(), 90_000, 2)));

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
            harness.compact("summary").await.unwrap();
            harness.prompt("second").await.unwrap();
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
            "<conversation_history_summary>\nsummary\n</conversation_history_summary>"
        );
        assert_eq!(text_of(&messages[1]), "first");
        assert!(matches!(&messages[2], AgentMessage::Assistant { .. }));
        assert_eq!(text_of(&messages[3]), "second");
        assert!(matches!(&messages[4], AgentMessage::Assistant { .. }));

        // The estimation boundary came along: needs_compaction works without
        // a separate recover_boundary() call.
        assert!(!harness.needs_compaction());
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
}