// AgentSession: the coding-agent facade over an AgentHarness — a session in
// a per-cwd folder, a model runtime, loaded resources, and the mounted tools.
// It composes the harness rather than reimplementing loop/compaction/session.

use std::path::PathBuf;
use std::sync::Arc;

use crate::harness::{AgentHarness, HarnessResources};
use crate::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
use crate::session::repository::SessionRepository;
use crate::tool::AgentTool;
use crate::tools::bash::BashTool;
use crate::tools::edit::EditTool;
use crate::tools::find::FindTool;
use crate::tools::grep::GrepTool;
use crate::tools::ls::LsTool;
use crate::tools::read::ReadTool;
use crate::tools::write::WriteTool;
use crate::types::{AgentMessage, Model, StopReason};

use super::model_runtime::ModelRuntime;

/// A live coding session: an open JSONL session plus a harness bound to it.
pub struct AgentSession {
    harness: AgentHarness<JsonlSessionStorage>,
    session_path: PathBuf,
}

impl AgentSession {
    /// Prompt the agent and return the produced messages.
    pub async fn prompt(&mut self, text: &str) -> Result<Vec<AgentMessage>, anyhow::Error> {
        self.harness.prompt(text).await
    }

    /// Rebuild the transcript from the session path.
    pub async fn restore(&mut self) -> Result<(), anyhow::Error> {
        self.harness.restore().await
    }

    /// Configure the compaction policy (keep-recent budget, reserve).
    pub fn set_compaction_settings(&mut self, settings: crate::compaction::CompactionSettings) {
        self.harness.set_compaction_settings(settings);
    }

    /// Compact the conversation.
    pub async fn compact(&mut self) -> Result<(), anyhow::Error> {
        self.harness.compact(None).await?;
        Ok(())
    }

    /// Switch the model (persisted; the resolver serves the new api).
    pub async fn set_model(&mut self, model: Model) -> Result<(), anyhow::Error> {
        self.harness.set_model(model).await
    }

    /// Move the session cursor to an earlier entry, optionally summarizing
    /// the abandoned branch (TS `navigateTree`).
    pub async fn navigate(
        &mut self,
        target_id: &str,
        options: crate::harness::NavigateTreeOptions,
    ) -> Result<crate::harness::NavigateTreeResult, anyhow::Error> {
        self.harness
            .navigate_tree_with_options(target_id, options)
            .await
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

    /// Close the session. The JSONL file remains openable and resumable.
    pub fn close(self) -> PathBuf {
        self.session_path
    }
}

/// Builds an [`AgentSession`]: cwd, session folder, model runtime, resources,
/// and the tool allow list.
#[derive(Default)]
pub struct AgentSessionBuilder {
    cwd: Option<PathBuf>,
    session_dir: Option<PathBuf>,
    model_runtime: Option<ModelRuntime>,
    resources: Option<HarnessResources>,
    tools: Vec<Arc<dyn AgentTool>>,
    model: Option<Model>,
    system_prompt: Option<String>,
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

    /// Mount an allow list of tools; defaults to the seven built-ins.
    pub fn with_tools(mut self, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        self.tools = tools;
        self
    }

    /// Open (or create) the session and assemble the harness.
    pub async fn build(self) -> Result<AgentSession, anyhow::Error> {
        self.assemble(true).await
    }

    /// Open an existing session file and assemble the harness over it.
    pub async fn open(self, path: std::path::PathBuf) -> Result<AgentSession, anyhow::Error> {
        self.assemble_with_path(path).await
    }

    async fn assemble(self, create: bool) -> Result<AgentSession, anyhow::Error> {
        let cwd = self
            .cwd
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let session_dir = self.session_dir.unwrap_or_else(|| cwd.join(".pi-sessions"));
        tokio::fs::create_dir_all(&session_dir).await?;
        let repo = SessionRepository::new(session_dir);

        let model = self.model.unwrap_or_else(|| Model {
            provider: "anthropic".into(),
            api: "anthropic".into(),
            id: std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into()),
            context_window: 200_000,
            max_tokens: 8_192,
            thinking: crate::types::ThinkingKind::None,
            metadata: Default::default(),
        });

        let session = if create {
            repo.create(JsonlSessionMetadata {
                id: uuid::Uuid::new_v4().to_string(),
                cwd: cwd.to_string_lossy().into_owned(),
                created_at: chrono::Utc::now(),
                parent_session_path: None,
                metadata: None,
            })
            .await?
        } else {
            unreachable!("assemble_with_path handles open")
        };
        let session_path = session.storage().path().to_path_buf();

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

        let runtime = self.model_runtime.unwrap_or_else(ModelRuntime::from_env);
        let stream_fn = runtime.resolver()(&model)?;
        let mut harness = AgentHarness::new(
            session,
            self.system_prompt
                .unwrap_or_else(|| "You are a helpful coding agent.".into()),
            model,
            stream_fn,
        )
        .with_stream_resolver(runtime.resolver())
        .with_tools(Arc::from(tools));
        if let Some(resources) = self.resources {
            harness = harness.with_resources(resources);
        }

        Ok(AgentSession {
            harness,
            session_path,
        })
    }

    async fn assemble_with_path(
        self,
        path: std::path::PathBuf,
    ) -> Result<AgentSession, anyhow::Error> {
        let repo = SessionRepository::new(
            path.parent()
                .map(std::path::Path::to_path_buf)
                .unwrap_or_default(),
        );
        let session = repo.open(&path).await?;
        let session_path = session.storage().path().to_path_buf();

        let model = self.model.unwrap_or_else(|| Model {
            provider: "anthropic".into(),
            api: "anthropic".into(),
            id: std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into()),
            context_window: 200_000,
            max_tokens: 8_192,
            thinking: crate::types::ThinkingKind::None,
            metadata: Default::default(),
        });

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

        let runtime = self.model_runtime.unwrap_or_else(ModelRuntime::from_env);
        let stream_fn = runtime.resolver()(&model)?;
        let mut harness = AgentHarness::new(
            session,
            self.system_prompt
                .unwrap_or_else(|| "You are a helpful coding agent.".into()),
            model,
            stream_fn,
        )
        .with_stream_resolver(runtime.resolver())
        .with_tools(Arc::from(tools));
        if let Some(resources) = self.resources {
            harness = harness.with_resources(resources);
        }

        Ok(AgentSession {
            harness,
            session_path,
        })
    }
}
