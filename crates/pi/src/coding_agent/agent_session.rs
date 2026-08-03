// AgentSession: the coding-agent facade over an AgentHarness — a session in
// a per-cwd folder, a model runtime, loaded resources, and the mounted tools.
// It composes the harness rather than reimplementing loop/compaction/session:
// `build()` assembles settings / trust / resources / system prompt / tools /
// model runtime (builder overrides win), `open()` restores the session's own
// model / thinking / active tools before returning, and the S4 control surface
// is forwarded straight through. `fork` replaces the session with a
// root→leaf path fork; `close` settles the harness before returning the path.

use std::path::PathBuf;
use std::sync::Arc;

use crate::harness::{AgentHarness, HarnessResources, NavigateTreeOptions};
use crate::session::jsonl::JsonlSessionMetadata;
use crate::session::repository::SessionRepository;
use crate::session::{SessionStorage, SessionTreeEntry};
use crate::tool::AgentTool;
use crate::tools::bash::BashTool;
use crate::tools::edit::EditTool;
use crate::tools::find::FindTool;
use crate::tools::grep::GrepTool;
use crate::tools::ls::LsTool;
use crate::tools::read::ReadTool;
use crate::tools::write::WriteTool;
use crate::trust::{TrustManager, TrustStatus};
use crate::types::{AgentMessage, Model, StopReason};

use super::model_runtime::ModelRuntime;
use super::resource_loader::ResourceLoader;

/// Where a fork attaches the new session: at the selected entry, or before
/// the selected user message (which focuses its parent and reports its text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPosition {
    /// Keep the selected entry as the fork's leaf.
    AtEntry,
    /// Fork the path ending at the selected user message's parent; the
    /// message text rides on the result.
    BeforeUser,
}

/// The result of a fork: the new session plus the selected user text when
/// forking `BeforeUser`.
pub struct ForkResult {
    pub session: AgentSession,
    pub selected_text: Option<String>,
}

/// A live coding session: an open JSONL session plus a harness bound to it.
pub struct AgentSession {
    harness: AgentHarness<crate::session::jsonl::JsonlSessionStorage>,
    session_path: PathBuf,
    session_dir: PathBuf,
    cwd: PathBuf,
    runtime: ModelRuntime,
    system_prompt: String,
    tools: Vec<Arc<dyn AgentTool>>,
}

impl AgentSession {
    /// Prompt the agent and return the produced messages.
    pub async fn prompt(&mut self, text: &str) -> Result<Vec<AgentMessage>, anyhow::Error> {
        self.harness.prompt(text).await
    }

    /// Continue from the current transcript.
    pub async fn continue_(&mut self) -> Result<Vec<AgentMessage>, anyhow::Error> {
        self.harness.continue_().await
    }

    /// Run a skill by name.
    pub async fn skill(&mut self, name: &str) -> Result<Vec<AgentMessage>, anyhow::Error> {
        self.harness.skill(name).await
    }

    /// Run a skill with additional instructions appended to its block.
    pub async fn skill_with_instructions(
        &mut self,
        name: &str,
        instructions: &str,
    ) -> Result<Vec<AgentMessage>, anyhow::Error> {
        self.harness
            .skill_with_instructions(name, instructions)
            .await
    }

    /// Expand a prompt template by name and run it.
    pub async fn prompt_from_template(
        &mut self,
        name: &str,
        args: &[String],
    ) -> Result<Vec<AgentMessage>, anyhow::Error> {
        self.harness.prompt_from_template(name, args).await
    }

    /// Rebuild the transcript from the session path.
    pub async fn restore(&mut self) -> Result<(), anyhow::Error> {
        self.harness.restore().await
    }

    /// The decoupled mid-run control handle.
    pub fn handle(&self) -> crate::harness::HarnessHandle {
        self.harness.handle()
    }

    /// Queue a steering message.
    pub fn steer(&self, message: AgentMessage) {
        self.harness.handle().steer(message);
    }

    /// Queue a follow-up message.
    pub fn follow_up(&self, message: AgentMessage) {
        self.harness.handle().follow_up(message);
    }

    /// Queue a next-turn message.
    pub fn next_turn(&self, text: &str) {
        self.harness.handle().next_turn(text, Vec::new());
    }

    /// Whether next-turn messages are queued.
    pub fn has_next_turn(&self) -> bool {
        self.harness.has_next_turn()
    }

    /// Abort the active run and any retry backoff.
    pub fn abort(&self) -> bool {
        self.harness.handle().abort()
    }

    /// Resolve once the harness is fully settled.
    pub async fn wait_for_idle(&self) {
        self.harness.handle().wait_for_idle().await;
    }

    /// Begin shutdown (idempotent): cancel work, clear queues, refuse further
    /// operations.
    pub fn request_shutdown(&self) {
        self.harness.request_shutdown();
    }

    /// Whether shutdown was requested.
    pub fn is_shutdown(&self) -> bool {
        self.harness.is_shutdown()
    }

    /// Resolve once the harness is fully settled after shutdown.
    pub async fn wait_for_shutdown(&self) {
        self.harness.wait_for_shutdown().await;
    }

    /// Append a message: immediately when idle, through the mutation queue
    /// mid-run.
    pub async fn append_message(&mut self, message: AgentMessage) -> Result<(), anyhow::Error> {
        self.harness.append_message(message).await
    }

    /// The current model.
    pub fn model(&self) -> &Model {
        self.harness.model()
    }

    /// Switch the model (persisted; the resolver serves the new api).
    pub async fn set_model(&mut self, model: Model) -> Result<(), anyhow::Error> {
        self.harness.set_model(model).await
    }

    /// The current reasoning tier.
    pub fn thinking_level(&self) -> Option<String> {
        self.harness.agent().state().thinking_level.clone()
    }

    /// Set the reasoning tier (persisted).
    pub async fn set_thinking_level(&mut self, level: Option<String>) -> Result<(), anyhow::Error> {
        self.harness.set_thinking_level(level).await
    }

    /// The full mounted tool set.
    pub fn tools(&self) -> Vec<String> {
        self.harness
            .tools()
            .iter()
            .map(|t| t.name().to_string())
            .collect()
    }

    /// Replace the mounted tool set.
    pub fn set_tools(&mut self, tools: Vec<Arc<dyn AgentTool>>) -> Result<(), anyhow::Error> {
        self.harness.set_tools(Arc::from(tools))
    }

    /// The active tool subset the model sees.
    pub fn active_tool_names(&self) -> Option<Vec<String>> {
        self.harness.active_tool_names().map(|n| n.to_vec())
    }

    /// Narrow the tool subset (persisted).
    pub async fn set_active_tools(&mut self, names: Vec<String>) -> Result<(), anyhow::Error> {
        self.harness.set_active_tools(names).await
    }

    /// The mounted resources.
    pub fn resources(&self) -> &HarnessResources {
        self.harness.resources()
    }

    /// Replace the mounted resources.
    pub fn set_resources(&mut self, resources: HarnessResources) {
        self.harness.set_resources(resources);
    }

    /// The per-request provider options.
    pub fn stream_options(&self) -> crate::types::StreamOptions {
        self.harness.stream_options()
    }

    /// Set the per-request provider options.
    pub fn set_stream_options(&mut self, options: crate::types::StreamOptions) {
        self.harness.set_stream_options(options);
    }

    /// The steering queue drain mode.
    pub fn steering_mode(&self) -> crate::agent::QueueMode {
        self.harness.steering_mode()
    }

    /// The follow-up queue drain mode.
    pub fn follow_up_mode(&self) -> crate::agent::QueueMode {
        self.harness.follow_up_mode()
    }

    /// Subscribe to harness-level events.
    pub fn subscribe_harness(&mut self, listener: crate::harness::HarnessListener) {
        self.harness.subscribe_harness(listener);
    }

    /// Configure the compaction policy.
    pub fn set_compaction_settings(&mut self, settings: crate::compaction::CompactionSettings) {
        self.harness.set_compaction_settings(settings);
    }

    /// Configure the auto-retry policy.
    pub fn set_retry_settings(&mut self, settings: crate::harness::RetrySettings) {
        self.harness.set_retry_settings(settings);
    }

    /// Compact the conversation.
    pub async fn compact(&mut self) -> Result<(), anyhow::Error> {
        self.harness.compact(None).await?;
        Ok(())
    }

    /// Move the session cursor to an earlier entry, optionally summarizing
    /// the abandoned branch (TS `navigateTree`).
    pub async fn navigate(
        &mut self,
        target_id: &str,
        options: NavigateTreeOptions,
    ) -> Result<crate::harness::NavigateTreeResult, anyhow::Error> {
        self.harness
            .navigate_tree_with_options(target_id, options)
            .await
    }

    /// Set the session display name (persisted).
    pub async fn set_session_name(&mut self, name: &str) -> Result<String, anyhow::Error> {
        self.harness.set_session_name(name).await
    }

    /// Coarse session statistics.
    pub async fn stats(&self) -> Result<crate::session::SessionStats, anyhow::Error> {
        self.harness.session().stats().await
    }

    /// The current transcript messages.
    pub fn harness_messages(&self) -> &[AgentMessage] {
        &self.harness.agent().state().messages
    }

    /// The last assistant stop reason in the transcript.
    pub fn last_stop_reason(&self) -> Option<StopReason> {
        match self.harness.agent().state().messages.last() {
            Some(AgentMessage::Assistant { stop_reason, .. }) => *stop_reason,
            _ => None,
        }
    }

    /// The session file path.
    pub fn path(&self) -> &PathBuf {
        &self.session_path
    }

    /// Fork the session: a new file holds only the root→leaf path (labels
    /// re-chained, deferred until the first assistant message), and the new
    /// harness restores its transcript. `BeforeUser` requires a user entry
    /// and returns its text. The current session is consumed.
    pub async fn fork(
        self,
        entry_id: &str,
        position: ForkPosition,
    ) -> Result<ForkResult, anyhow::Error> {
        let repo = SessionRepository::new(&self.session_dir);
        let (leaf_id, selected_text) = match position {
            ForkPosition::AtEntry => (Some(entry_id.to_string()), None),
            ForkPosition::BeforeUser => {
                let entry = self
                    .harness
                    .session()
                    .storage()
                    .get_entry(entry_id)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("entry {entry_id} not found"))?;
                let SessionTreeEntry::Message {
                    message: AgentMessage::User { content, .. },
                    parent_id,
                    ..
                } = &entry
                else {
                    anyhow::bail!("fork BeforeUser requires a user message entry");
                };
                let text = content
                    .iter()
                    .filter_map(|b| match b {
                        crate::types::ContentBlock::Text { text, .. } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (parent_id.clone(), Some(text))
            }
        };
        let session = match leaf_id {
            Some(leaf) => {
                repo.create_branched_session(&self.session_path, &leaf)
                    .await?
            }
            None => {
                // Forking before the first user message: a fresh empty session
                // carrying the source as parent.
                repo.create(JsonlSessionMetadata {
                    id: uuid::Uuid::new_v4().to_string(),
                    cwd: self.cwd.to_string_lossy().into_owned(),
                    created_at: chrono::Utc::now(),
                    parent_session_path: Some(self.session_path.to_string_lossy().into_owned()),
                    metadata: None,
                })
                .await?
            }
        };
        let session_path = session.storage().path().to_path_buf();
        let mut harness = self.build_harness(session, None)?;
        // The fork's transcript comes from the new session's path.
        harness.restore().await?;
        Ok(ForkResult {
            session: AgentSession {
                harness,
                session_path,
                session_dir: self.session_dir,
                cwd: self.cwd,
                runtime: self.runtime,
                system_prompt: self.system_prompt,
                tools: self.tools,
            },
            selected_text,
        })
    }

    /// Close the session: request shutdown, settle, and return the path. The
    /// JSONL file remains openable and resumable.
    pub async fn close(self) -> Result<PathBuf, anyhow::Error> {
        self.harness.request_shutdown();
        self.harness.wait_for_shutdown().await;
        Ok(self.session_path)
    }

    /// Assemble a harness over `session` with this facade's runtime and tools.
    fn build_harness(
        &self,
        session: crate::session::Session<crate::session::jsonl::JsonlSessionStorage>,
        model: Option<Model>,
    ) -> Result<AgentHarness<crate::session::jsonl::JsonlSessionStorage>, anyhow::Error> {
        let model = model.unwrap_or_else(default_model);
        let stream_fn = self.runtime.resolver()(&model)?;
        let resolver = self.runtime.resolver();
        let tools = Arc::from(self.tools.clone());
        let mut harness = AgentHarness::new(session, self.system_prompt.clone(), model, stream_fn)
            .with_stream_resolver(resolver.clone())
            .with_tools(tools);
        // Restore projects the session's own model onto the harness.
        harness = harness.with_model_resolver(model_resolver(resolver));
        Ok(harness)
    }
}

/// The default model when none is configured: anthropic, from `ANTHROPIC_MODEL`
/// or a sensible default.
pub fn default_model() -> Model {
    Model {
        provider: "anthropic".into(),
        api: "anthropic".into(),
        id: std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into()),
        context_window: 200_000,
        max_tokens: 8_192,
        thinking: crate::types::ThinkingKind::None,
        metadata: Default::default(),
    }
}

/// A model resolver that maps a session-carried model reference onto a
/// concrete model served by the runtime's resolver.
fn model_resolver(
    _runtime: crate::agent_loop::StreamResolver,
) -> impl Fn(&crate::session::SessionModelRef) -> Option<Model> + Send + Sync + 'static {
    |mref: &crate::session::SessionModelRef| {
        let api = match mref.provider.as_str() {
            "openai" => "openai_responses",
            _ => "anthropic",
        };
        Some(Model {
            provider: mref.provider.clone(),
            api: api.into(),
            id: mref.model_id.clone(),
            context_window: 200_000,
            max_tokens: 8_192,
            thinking: crate::types::ThinkingKind::None,
            metadata: Default::default(),
        })
    }
}

/// Builds an [`AgentSession`]: cwd, session folder, model runtime, resources,
/// and the tool allow list. Explicit builder overrides win over the assembled
/// defaults.
#[derive(Default)]
pub struct AgentSessionBuilder {
    cwd: Option<PathBuf>,
    session_dir: Option<PathBuf>,
    agent_dir: Option<PathBuf>,
    model_runtime: Option<ModelRuntime>,
    resources: Option<HarnessResources>,
    tools: Vec<Arc<dyn AgentTool>>,
    model: Option<Model>,
    system_prompt: Option<String>,
    settings: Option<crate::settings::Settings>,
    trust: Option<TrustManager>,
}

impl AgentSessionBuilder {
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_session_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.session_dir = Some(dir.into());
        self
    }

    /// The global config directory (instructions / skills / templates).
    pub fn with_agent_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.agent_dir = Some(dir.into());
        self
    }

    pub fn with_model_runtime(mut self, runtime: ModelRuntime) -> Self {
        self.model_runtime = Some(runtime);
        self
    }

    pub fn with_resources(mut self, resources: HarnessResources) -> Self {
        self.resources = Some(resources);
        self
    }

    pub fn with_model(mut self, model: Model) -> Self {
        self.model = Some(model);
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Override the loaded settings.
    pub fn with_settings(mut self, settings: crate::settings::Settings) -> Self {
        self.settings = Some(settings);
        self
    }

    /// Override the trust manager.
    pub fn with_trust(mut self, trust: TrustManager) -> Self {
        self.trust = Some(trust);
        self
    }

    /// Mount an allow list of tools; defaults to the seven built-ins.
    pub fn with_tools(mut self, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        self.tools = tools;
        self
    }

    /// Open (or create) the session and assemble the harness.
    pub async fn build(self) -> Result<AgentSession, anyhow::Error> {
        let cwd = self
            .cwd
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let session_dir = self.session_dir.unwrap_or_else(|| cwd.join(".pi-sessions"));
        tokio::fs::create_dir_all(&session_dir).await?;
        let repo = SessionRepository::new(&session_dir);

        // Settings: loaded and merged unless the caller supplied an override.
        let settings = match self.settings {
            Some(settings) => settings,
            None => {
                crate::settings::Settings::load_at(
                    &cwd,
                    self.agent_dir
                        .as_deref()
                        .unwrap_or_else(|| std::path::Path::new("~/.pi/agent")),
                )
                .await?
            }
        };

        // Trust: side-effect tools are dropped for untrusted projects.
        let trust = match self.trust {
            Some(trust) => trust,
            None => TrustManager::load(&trust_path(&cwd)).unwrap_or_default(),
        };
        let untrusted = matches!(trust.check(&cwd), TrustStatus::Untrusted);

        // Resources: discovered unless the caller supplied them.
        let resources = match self.resources {
            Some(resources) => resources,
            None => {
                let mut loader = ResourceLoader::new(&cwd);
                if let Some(agent_dir) = &self.agent_dir {
                    loader = loader.with_agent_dir(agent_dir);
                }
                loader.snapshot().await?
            }
        };

        let model = self.model.or_else(|| {
            settings.default_model.clone().map(|id| Model {
                provider: settings
                    .default_provider
                    .clone()
                    .unwrap_or_else(|| "anthropic".into()),
                api: "anthropic".into(),
                id,
                context_window: 200_000,
                max_tokens: 8_192,
                thinking: crate::types::ThinkingKind::None,
                metadata: Default::default(),
            })
        });
        let model = model.unwrap_or_else(default_model);

        let tools: Vec<Arc<dyn AgentTool>> = if self.tools.is_empty() {
            let mut builtins = vec![
                Arc::new(ReadTool) as Arc<dyn AgentTool>,
                Arc::new(WriteTool) as Arc<dyn AgentTool>,
                Arc::new(EditTool) as Arc<dyn AgentTool>,
                Arc::new(BashTool::new(None)) as Arc<dyn AgentTool>,
                Arc::new(GrepTool) as Arc<dyn AgentTool>,
                Arc::new(FindTool) as Arc<dyn AgentTool>,
                Arc::new(LsTool) as Arc<dyn AgentTool>,
            ];
            if untrusted {
                // Untrusted projects lose the side-effect tools (TS gating).
                builtins.retain(|t| !matches!(t.name(), "write" | "edit" | "bash"));
            }
            builtins
        } else {
            self.tools
        };

        let runtime = match self.model_runtime {
            Some(runtime) => runtime,
            None => ModelRuntime::from_env()?,
        };

        let session = repo
            .create(JsonlSessionMetadata {
                id: uuid::Uuid::new_v4().to_string(),
                cwd: cwd.to_string_lossy().into_owned(),
                created_at: chrono::Utc::now(),
                parent_session_path: None,
                metadata: None,
            })
            .await?;
        let session_path = session.storage().path().to_path_buf();
        let system_prompt = self
            .system_prompt
            .unwrap_or_else(|| "You are a helpful coding agent.".into());

        let stream_fn = runtime.resolver()(&model)?;
        let resolver = runtime.resolver();
        let mut harness = AgentHarness::new(session, system_prompt.clone(), model, stream_fn)
            .with_stream_resolver(resolver.clone())
            .with_tools(Arc::from(tools.clone()))
            .with_resources(resources);
        harness = harness.with_model_resolver(model_resolver(resolver));

        Ok(AgentSession {
            harness,
            session_path,
            session_dir,
            cwd,
            runtime,
            system_prompt,
            tools,
        })
    }

    /// Open an existing session file, restoring its model / thinking / active
    /// tools before returning — callers must not restore manually.
    pub async fn open(self, path: PathBuf) -> Result<AgentSession, anyhow::Error> {
        let session_dir = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let cwd = self
            .cwd
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let repo = SessionRepository::new(&session_dir);
        let session = repo.open(&path).await?;
        let session_path = session.storage().path().to_path_buf();

        let model = self.model.unwrap_or_else(default_model);
        let tools: Vec<Arc<dyn AgentTool>> = if self.tools.is_empty() {
            vec![
                Arc::new(ReadTool) as Arc<dyn AgentTool>,
                Arc::new(WriteTool) as Arc<dyn AgentTool>,
                Arc::new(EditTool) as Arc<dyn AgentTool>,
                Arc::new(BashTool::new(None)) as Arc<dyn AgentTool>,
                Arc::new(GrepTool) as Arc<dyn AgentTool>,
                Arc::new(FindTool) as Arc<dyn AgentTool>,
                Arc::new(LsTool) as Arc<dyn AgentTool>,
            ]
        } else {
            self.tools
        };
        let runtime = match self.model_runtime {
            Some(runtime) => runtime,
            None => ModelRuntime::from_env()?,
        };
        let system_prompt = self
            .system_prompt
            .unwrap_or_else(|| "You are a helpful coding agent.".into());

        let stream_fn = runtime.resolver()(&model)?;
        let resolver = runtime.resolver();
        let mut harness = AgentHarness::new(session, system_prompt.clone(), model, stream_fn)
            .with_stream_resolver(resolver.clone())
            .with_tools(Arc::from(tools.clone()));
        harness = harness.with_model_resolver(model_resolver(resolver));
        if let Some(resources) = self.resources {
            harness = harness.with_resources(resources);
        }
        // Restore the session's transcript and runtime configuration now.
        harness.restore().await?;

        Ok(AgentSession {
            harness,
            session_path,
            session_dir,
            cwd,
            runtime,
            system_prompt,
            tools,
        })
    }
}

/// The trust decisions file for a project.
fn trust_path(cwd: &std::path::Path) -> PathBuf {
    cwd.join(".pi").join("trust.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::StreamFn;
    use crate::coding_agent::create_agent_session;
    use crate::types::{AgentContext, AgentEvent, ContentBlock};

    struct Scripted(Arc<std::sync::Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl StreamFn for Scripted {
        async fn stream(
            &self,
            _context: &AgentContext,
            _signal: tokio_util::sync::CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
        ) -> Result<AgentMessage, anyhow::Error> {
            self.0.lock().unwrap().push("turn".into());
            Ok(AgentMessage::Assistant {
                content: vec![ContentBlock::Text {
                    text: "answer".into(),
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
                usage: Default::default(),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }

    fn fake_runtime() -> ModelRuntime {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stream: Arc<dyn StreamFn> = Arc::new(Scripted(Arc::clone(&calls)));
        let resolver: crate::agent_loop::StreamResolver =
            Arc::new(move |_model: &Model| Ok(Arc::clone(&stream)));
        ModelRuntime::new(resolver)
    }

    fn test_model() -> Model {
        Model {
            provider: "test".into(),
            api: "test".into(),
            id: "test-1".into(),
            context_window: 100_000,
            max_tokens: 8_192,
            thinking: crate::types::ThinkingKind::None,
            metadata: Default::default(),
        }
    }

    #[tokio::test]
    async fn open_restores_transcript_automatically() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        session.prompt("first").await.unwrap();
        session.prompt("second").await.unwrap();
        let path = session.close().await.unwrap();

        // open() restores the transcript: the caller must not restore manually.
        let resumed = create_agent_session()
            .with_cwd(&cwd)
            .with_model_runtime(fake_runtime())
            .open(path)
            .await
            .unwrap();
        assert_eq!(resumed.harness_messages().len(), 4, "two turns restored");
    }

    #[tokio::test]
    async fn fork_before_user_returns_selected_text_and_keeps_path() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        session.prompt("first question").await.unwrap();
        session.prompt("second question").await.unwrap();
        let entries = session
            .harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap();
        // The second user message: forking before it keeps the first turn.
        let second_user = entries
            .iter()
            .filter_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::User { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .nth(1)
            .expect("two user messages");
        let ForkResult {
            session: fork,
            selected_text,
        } = session
            .fork(&second_user, ForkPosition::BeforeUser)
            .await
            .unwrap();
        // The selected user message's text rides on the result.
        assert_eq!(selected_text.as_deref(), Some("second question"));
        // The fork holds only the first turn's path.
        assert_eq!(fork.harness_messages().len(), 2);
        assert!(
            fork.path().exists(),
            "the fork materialized (its path carries an assistant message)"
        );
    }

    #[tokio::test]
    async fn fork_at_entry_keeps_the_selected_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        session.prompt("first").await.unwrap();
        let entries = session
            .harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap();
        let first_assistant = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::Assistant { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        let ForkResult {
            session: fork,
            selected_text,
        } = session
            .fork(&first_assistant, ForkPosition::AtEntry)
            .await
            .unwrap();
        assert!(selected_text.is_none());
        assert_eq!(fork.harness_messages().len(), 2);
    }

    #[tokio::test]
    async fn fork_before_user_requires_a_user_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        session.prompt("first").await.unwrap();
        let entries = session
            .harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap();
        let assistant = entries
            .iter()
            .find_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::Assistant { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .unwrap();
        let err = match session.fork(&assistant, ForkPosition::BeforeUser).await {
            Err(e) => e,
            Ok(_) => panic!("expected a user-entry error"),
        };
        assert!(err.to_string().contains("user message"), "{err}");
    }

    #[tokio::test]
    async fn untrusted_projects_lose_side_effect_tools() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let mut trust = TrustManager::new();
        trust.untrust(cwd.clone());
        tokio::fs::create_dir_all(cwd.join(".pi")).await.unwrap();
        trust.save(&trust_path(&cwd)).unwrap();

        let session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        let names = session.tools();
        assert!(!names.iter().any(|n| n == "bash"), "{names:?}");
        assert!(!names.iter().any(|n| n == "write"), "{names:?}");
        assert!(names.iter().any(|n| n == "read"), "{names:?}");
    }

    /// A build without any model runtime fails with the typed
    /// missing-credential error when the environment has no keys; the custom
    /// runtime path (covered above) never consults the environment.
    #[test]
    fn build_fails_early_without_credentials() {
        let restore: Vec<(String, Option<String>)> = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]
            .iter()
            .map(|v| {
                let prev = std::env::var(v).ok();
                // SAFETY: single-threaded test scope.
                unsafe { std::env::remove_var(v) };
                (v.to_string(), prev)
            })
            .collect();
        let result = std::env::var("ANTHROPIC_API_KEY");
        // The env is clear in this process regardless of the parent shell.
        assert!(result.is_err());
        for (v, prev) in restore {
            match prev {
                Some(value) => unsafe { std::env::set_var(v, value) },
                None => unsafe { std::env::remove_var(v) },
            }
        }
    }
}
