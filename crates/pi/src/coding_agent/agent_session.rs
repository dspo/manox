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

use anyhow::Context;

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

use super::model_runtime::{ModelCatalog, ModelRuntime};
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

/// The runtime assembly state of a session, captured from the harness so a
/// fork rebuilds an identical environment: tool set + active selection,
/// resources, stream options, queue modes, and policies.
#[derive(Clone)]
struct AssemblyConfig {
    tools: Vec<Arc<dyn AgentTool>>,
    active_tool_names: Option<Vec<String>>,
    resources: HarnessResources,
    stream_options: crate::types::StreamOptions,
    steering_mode: crate::agent::QueueMode,
    follow_up_mode: crate::agent::QueueMode,
    compaction: crate::compaction::CompactionSettings,
    retry: crate::harness::RetrySettings,
}

impl AssemblyConfig {
    fn capture(harness: &AgentHarness<crate::session::jsonl::JsonlSessionStorage>) -> Self {
        AssemblyConfig {
            tools: harness.tools().to_vec(),
            active_tool_names: harness.active_tool_names().map(|n| n.to_vec()),
            resources: harness.resources().clone(),
            stream_options: harness.stream_options(),
            steering_mode: harness.steering_mode(),
            follow_up_mode: harness.follow_up_mode(),
            compaction: harness.compaction_settings().clone(),
            retry: *harness.retry_settings(),
        }
    }

    async fn apply(
        self,
        harness: &mut AgentHarness<crate::session::jsonl::JsonlSessionStorage>,
    ) -> Result<(), anyhow::Error> {
        harness.set_tools(Arc::from(self.tools))?;
        if let Some(names) = self.active_tool_names {
            harness.set_active_tools(names).await?;
        }
        harness.set_resources(self.resources);
        harness.set_stream_options(self.stream_options);
        harness.set_steering_mode(self.steering_mode);
        harness.set_follow_up_mode(self.follow_up_mode);
        harness.set_compaction_settings(self.compaction);
        harness.set_retry_settings(self.retry);
        Ok(())
    }
}

/// A live coding session: an open JSONL session plus a harness bound to it.
pub struct AgentSession {
    harness: AgentHarness<crate::session::jsonl::JsonlSessionStorage>,
    session_path: PathBuf,
    session_dir: PathBuf,
    cwd: PathBuf,
    runtime: ModelRuntime,
    system_prompt: String,
    /// The base prompt the system-prompt builder starts from; kept so a
    /// fork re-installs the builder rather than freezing a rendered prompt.
    base_prompt: String,
    tools: Vec<Arc<dyn AgentTool>>,
    /// The facade's pending next-turn messages (TS coding-agent
    /// `_pendingNextTurnMessages`): delivered AFTER the prompt's own user
    /// message as asides, distinct from the harness's queued-first queue.
    pending_next_turn: Arc<std::sync::Mutex<Vec<AgentMessage>>>,
    /// Set when a reopened session's model could not be resolved exactly and
    /// the initial model was used instead (TS `modelFallbackMessage`).
    model_fallback_notice: Option<String>,
    /// The settings default thinking level applied at assembly; used when a
    /// model switch moves from a non-thinking model to a thinking one (TS
    /// restores the settings default).
    settings_thinking_default: Option<String>,
    /// Whether the system prompt is a caller-provided custom prompt (TS
    /// `customPrompt` replacement semantics), preserved across forks.
    custom_prompt: bool,
    /// The agent config dir; `set_model`/`set_thinking_level` persist the
    /// default model / thinking level to its global settings file (TS
    /// `setDefaultModelAndProvider`/`setDefaultThinkingLevel`).
    agent_dir: PathBuf,
}

impl AgentSession {
    /// Prompt the agent and return the produced messages. Expands the TS
    /// default prompt forms: `/skill:name args` becomes the skill invocation
    /// block (with `args` appended), and `/name args` expands a prompt
    /// template by name with `substituteArgs`.
    pub async fn prompt(&mut self, text: &str) -> Result<Vec<AgentMessage>, anyhow::Error> {
        let expanded = self.expand_prompt(text);
        // The facade delivers its pending next-turn messages as asides AFTER
        // the prompt's own user message (user-first); the harness's own
        // queue keeps pi-agent-core queued-first semantics.
        let asides = std::mem::take(&mut *self.pending_next_turn.lock().unwrap());
        self.harness
            .prompt_input(crate::harness::PromptInput {
                text: expanded,
                images: Vec::new(),
                asides,
            })
            .await
    }

    /// The TS `_expandSkillCommand` + `expandPromptTemplate` expansion.
    fn expand_prompt(&self, text: &str) -> String {
        if let Some(rest) = text.strip_prefix("/skill:") {
            let (name, args) = match rest.find(' ') {
                Some(i) => (&rest[..i], rest[i + 1..].trim().to_string()),
                None => (rest, String::new()),
            };
            if let Some(skill) = self.resources().skills.iter().find(|s| s.name == name) {
                let block = crate::harness::format_skill_invocation(skill, None);
                return if args.is_empty() {
                    block
                } else {
                    format!("{block}\n\n{args}")
                };
            }
            return text.to_string(); // Unknown skill, pass through.
        }
        if let Some(rest) = text.strip_prefix('/') {
            let (name, args_string) = match rest.find(' ') {
                Some(i) => (&rest[..i], rest[i + 1..].to_string()),
                None => (rest, String::new()),
            };
            if let Some(template) = self
                .resources()
                .prompt_templates
                .iter()
                .find(|t| t.name == name)
            {
                let args = crate::harness::parse_command_args(&args_string);
                return crate::harness::substitute_args(&template.content, &args);
            }
        }
        text.to_string()
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

    /// Queue a next-turn message delivered after the next prompt's own
    /// message (TS coding-agent `nextTurn` asides).
    pub fn next_turn(&self, text: &str) {
        // Refuse after shutdown: the facade's queue contract matches the
        // harness's (no further operations once shut down).
        if self.is_shutdown() {
            return;
        }
        self.pending_next_turn
            .lock()
            .unwrap()
            .push(AgentMessage::user(text));
    }

    /// The model-fallback notice when a reopened session's model could not
    /// be restored exactly (TS `modelFallbackMessage`), if any.
    pub fn model_fallback_notice(&self) -> Option<&str> {
        self.model_fallback_notice.as_deref()
    }

    /// Whether next-turn messages are queued (facade pending or harness).
    pub fn has_next_turn(&self) -> bool {
        !self.pending_next_turn.lock().unwrap().is_empty() || self.harness.has_next_turn()
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
        self.pending_next_turn.lock().unwrap().clear();
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
    /// Re-clamps the thinking level against the new model's capability and
    /// persists the actual level (TS `setModel` → `_getThinkingLevelForModelSwitch`
    /// → `setThinkingLevel`): a switch from a non-thinking model restores the
    /// settings default (else `medium`), otherwise the user preference is kept
    /// and clamped.
    pub async fn set_model(&mut self, model: Model) -> Result<(), anyhow::Error> {
        // Compute the effective thinking level up front: TS
        // `_getThinkingLevelForModelSwitch` keeps the user preference when
        // the current model supports thinking, else restores the settings
        // default; `setThinkingLevel` clamps against the NEW model.
        let current_level = self.harness.agent().state().thinking_level.clone();
        let current_supports = self.harness.model().thinking != crate::types::ThinkingKind::None;
        let desired = if current_supports {
            Some(current_level.clone().unwrap_or_else(|| "off".into()))
        } else {
            self.settings_thinking_default
                .clone()
                .or_else(|| Some("medium".into()))
        };
        // The runtime tier normalizes `off` to `None` (agent semantics); the
        // persisted preference keeps the wire form (`Some("off")`) so an
        // explicit off survives a round trip through a non-thinking model.
        let effective = clamp_thinking(&model, &self.runtime.thinking_levels(&model), desired)
            .filter(|l| l != "off");
        let wire = effective.clone().unwrap_or_else(|| "off".into());

        // TS writes the declared default (provider/model) before the
        // switch: settings.json carries the user's intent for future
        // sessions, and the harness switch follows. A failed switch leaves
        // the active session on its prior model (the session JSONL is
        // authoritative on reopen) while the declared default persists —
        // matching TS `setModel`.
        let provider = model.provider.clone();
        let model_id = model.id.clone();
        self.patch_global_settings(|obj| {
            obj.insert("defaultProvider".into(), provider.into());
            obj.insert("defaultModel".into(), model_id.into());
        })
        .await?;

        self.harness.set_model(model.clone()).await?;
        // TS `setThinkingLevel`: persist only when the effective level
        // actually changes; the entry write is synchronous and throws on
        // failure — propagate it so the caller knows the state did not land.
        if effective != current_level {
            self.harness
                .session()
                .append_thinking_level_change(&wire)
                .await?;
            // TS updates the settings default only when the new model
            // supports thinking or the level is not `off`. The switch and
            // thinking entry have already landed; the default is a best-effort
            // preference for future sessions, so a write failure is logged
            // rather than failing an otherwise-successful switch.
            if model.thinking != crate::types::ThinkingKind::None || wire != "off" {
                self.settings_thinking_default = Some(wire.clone());
                let default = wire.clone();
                if let Err(e) = self
                    .patch_global_settings(|obj| {
                        obj.insert("defaultThinkingLevel".into(), default.into());
                    })
                    .await
                {
                    tracing::warn!("failed to persist default thinking level: {e:#}");
                }
            }
        }
        self.harness.set_initial_thinking_level(effective);
        Ok(())
    }

    /// The current reasoning tier.
    pub fn thinking_level(&self) -> Option<String> {
        self.harness.agent().state().thinking_level.clone()
    }

    /// Set the reasoning tier (persisted). `None` reads as `"off"` (the
    /// facade's contract, mirrored by the harness); the effective level is
    /// clamped against the current model and persisted only when it changes
    /// (TS `setThinkingLevel`).
    pub async fn set_thinking_level(&mut self, level: Option<String>) -> Result<(), anyhow::Error> {
        let desired = Some(level.unwrap_or_else(|| "off".into()));
        let model = self.harness.model().clone();
        let levels = self.runtime.thinking_levels(&model);
        let effective = clamp_thinking(&model, &levels, desired).filter(|l| l != "off");
        let wire = effective.clone().unwrap_or_else(|| "off".into());
        let previous = self.harness.agent().state().thinking_level.clone();
        if effective != previous {
            self.harness
                .session()
                .append_thinking_level_change(&wire)
                .await?;
            if model.thinking != crate::types::ThinkingKind::None || wire != "off" {
                self.settings_thinking_default = Some(wire.clone());
                let default = wire.clone();
                // Best-effort preference for future sessions; the entry has
                // already landed, so a write failure is logged, not fatal.
                if let Err(e) = self
                    .patch_global_settings(|obj| {
                        obj.insert("defaultThinkingLevel".into(), default.into());
                    })
                    .await
                {
                    tracing::warn!("failed to persist default thinking level: {e:#}");
                }
            }
        }
        self.harness.set_initial_thinking_level(effective);
        Ok(())
    }

    /// Patch the global settings file at the field level: the raw JSON is
    /// read, the closure mutates specific top-level keys, and the whole
    /// object is written back atomically (temp file + rename). Fields the
    /// typed `Settings` does not model (theme, packages, extensions,
    /// transport, images, enabledModels, future keys) survive untouched, as
    /// TS merges only the modified fields. manox ships single-process, so no
    /// cross-process file lock is taken; concurrent writes from other
    /// processes racing on the same agent dir are not guarded.
    async fn patch_global_settings(
        &self,
        patch: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
    ) -> Result<(), anyhow::Error> {
        let path = crate::settings::Settings::global_path(&self.agent_dir);
        let root = match tokio::fs::read_to_string(&path).await {
            // A missing file is a fresh start: patch runs against `{}`.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
            Err(e) => return Err(e.into()),
            Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
                .context("parse global settings.json")?,
        };
        let mut root = match root {
            serde_json::Value::Object(map) => map,
            // A non-object root is corrupt; do not silently rewrite it under
            // a synthetic key (that key would itself become an unmodeled
            // field polluting the file).
            other => anyhow::bail!("global settings.json root is not an object: {other}"),
        };
        patch(&mut root);
        let serialized = serde_json::to_string_pretty(&serde_json::Value::Object(root))?;
        // The agent dir may not yet exist for a first-time user; create it so
        // the temp file write does not fail on a missing parent.
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, serialized).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
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
        let config = AssemblyConfig::capture(&self.harness);
        let mut harness = self.build_harness(session, None, config).await?;
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
                base_prompt: self.base_prompt,
                tools: self.tools,
                pending_next_turn: Arc::new(std::sync::Mutex::new(Vec::new())),
                model_fallback_notice: None,
                settings_thinking_default: self.settings_thinking_default.clone(),
                custom_prompt: self.custom_prompt,
                agent_dir: self.agent_dir.clone(),
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
    async fn build_harness(
        &self,
        session: crate::session::Session<crate::session::jsonl::JsonlSessionStorage>,
        model: Option<Model>,
        config: AssemblyConfig,
    ) -> Result<AgentHarness<crate::session::jsonl::JsonlSessionStorage>, anyhow::Error> {
        let model = model.unwrap_or_else(default_model);
        let stream_fn = self.runtime.resolver()(&model)?;
        let resolver = self.runtime.resolver();
        let mut harness = AgentHarness::new(session, self.system_prompt.clone(), model, stream_fn)
            .with_stream_resolver(resolver.clone())
            .with_tool_cwd(self.cwd.clone());
        // Re-install the system-prompt builder (not just the rendered
        // prompt), so a fork keeps rebuilding on active-tool/resource
        // changes.
        let base_prompt = self.base_prompt.clone();
        let cwd = self.cwd.clone();
        let custom_for_builder = self.custom_prompt;
        harness.set_system_prompt_builder(
            move |active_tools: &[String], resources: &HarnessResources| {
                crate::system_prompt::build_harness_prompt(
                    &base_prompt,
                    &cwd,
                    active_tools,
                    &resources.context_files,
                    &resources.skills,
                    custom_for_builder,
                )
            },
        );
        // Restore projects the session's own model onto the harness.
        harness = harness.with_model_resolver(model_resolver(&self.runtime));
        let observer = harness.request_observer();
        harness.rebind_stream_resolver(self.runtime.resolver_with_observer(observer));
        // Re-apply the captured assembly state so a fork is identical to the
        // session it came from; propagate every error (a fork must never
        // silently come back half-assembled).
        config.apply(&mut harness).await?;
        Ok(harness)
    }
}

/// Resolve a settings path against the project cwd (TS `resolvePath`).
fn resolve_path(cwd: &std::path::Path, path: &str) -> PathBuf {
    let expanded = expand_tilde(path);
    if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    }
}

/// Expand a leading `~` to the home directory, the TS `expandTildePath`.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(path)
}

/// The default agent config directory: `~/.pi/agent` (TS `getAgentDir`).
fn default_agent_dir() -> PathBuf {
    std::env::var("PI_AGENT_DIR")
        .map(|v| expand_tilde(&v))
        .unwrap_or_else(|_| expand_tilde("~/.pi/agent"))
}

/// Clamp a desired thinking level against a model's capability (TS
/// `clampThinkingLevel`): a non-thinking model forces `off`; with no desire,
/// a thinking-capable model defaults to `medium`.
fn clamp_thinking(model: &Model, levels: &[String], desired: Option<String>) -> Option<String> {
    let levels: Vec<&str> = if model.thinking == crate::types::ThinkingKind::None {
        vec!["off"]
    } else {
        levels.iter().map(|l| l.as_str()).collect()
    };
    match desired {
        None => {
            if model.thinking == crate::types::ThinkingKind::None {
                None
            } else {
                Some("medium".to_string())
            }
        }
        Some(level) => {
            if levels.contains(&level.as_str()) {
                return Some(level);
            }
            let order = crate::coding_agent::model_runtime::THINKING_LEVELS;
            let Some(requested) = order.iter().position(|l| *l == level) else {
                return levels.first().map(|l| l.to_string());
            };
            for candidate in &order[requested..] {
                if levels.contains(candidate) {
                    return Some(candidate.to_string());
                }
            }
            for candidate in order[..requested].iter().rev() {
                if levels.contains(candidate) {
                    return Some(candidate.to_string());
                }
            }
            levels.first().map(|l| l.to_string())
        }
    }
}

/// The default model when none is configured: Sonnet 4.6 resolved through
/// the default catalog, so its thinking capability and parameters are exact
/// (`ANTHROPIC_MODEL` overrides the id; an unknown override falls back to
/// Sonnet).
pub fn default_model() -> Model {
    let id = std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into());
    // Resolve through the default catalog; an unknown override falls back to
    // Sonnet entirely (id and capability), never a hand-built guess.
    crate::coding_agent::model_runtime::DefaultModelCatalog
        .resolve("anthropic", &id)
        .unwrap_or_else(|| {
            crate::coding_agent::model_runtime::DefaultModelCatalog
                .resolve("anthropic", "claude-sonnet-4-6")
                .expect("sonnet is in the default catalog")
        })
}

/// A model resolver that restores a session-carried model reference through
/// the runtime's catalog. An unknown provider resolves to `None`, so the
/// harness keeps its construction-time model instead of guessing a protocol.
fn model_resolver(
    runtime: &ModelRuntime,
) -> impl Fn(&crate::session::SessionModelRef) -> Option<Model> + Send + Sync + 'static {
    let runtime = runtime.clone();
    move |mref: &crate::session::SessionModelRef| {
        runtime.resolve_model(&mref.provider, &mref.model_id)
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

    /// The shared build/open assembly: settings, trust, resources, system
    /// prompt, model, tools, and runtime are resolved identically for a new
    /// session and a reopened one, so a reopen does not drift. `restore`
    /// rebuilds the transcript and runtime configuration for open.
    async fn assemble_common(
        &self,
        session: crate::session::Session<crate::session::jsonl::JsonlSessionStorage>,
        cwd: PathBuf,
        session_dir: PathBuf,
        restore: bool,
    ) -> Result<AgentSession, anyhow::Error> {
        // Agent config dir: explicit override, `PI_AGENT_DIR`, or `~/.pi/agent`
        // (with `~` expanded).
        let agent_dir = self.agent_dir.clone().unwrap_or_else(default_agent_dir);

        // Trust first: the global `agentDir/trust.json`, nearest-ancestor
        // match. Undecided projects are treated as untrusted (no UI to ask).
        let trust = match &self.trust {
            Some(trust) => trust.clone(),
            None => TrustManager::load(&agent_dir.join("trust.json")).unwrap_or_default(),
        };
        let trusted = matches!(trust.check(&cwd), TrustStatus::Trusted);

        // Settings: loaded and merged unless the caller supplied an override.
        // An untrusted project's settings are treated as an empty config —
        // `.pi/settings.json` cannot steer shell prefix, model, retry, or
        // resource paths until the project is trusted (TS).
        let settings = match &self.settings {
            Some(settings) => settings.clone(),
            None => {
                let mut global = crate::settings::Settings::load_global_at(&agent_dir).await?;
                if trusted {
                    let project = crate::settings::Settings::load_project_at(&cwd).await?;
                    global.merge(&project);
                }
                global
            }
        };

        // Resources: discovered unless the caller supplied them. Trust gates
        // project-scoped resources; the user/agentDir ones always load.
        let resources = match &self.resources {
            Some(resources) => resources.clone(),
            None => {
                let extra_skills = settings
                    .skills
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| resolve_path(&cwd, &p))
                    .collect();
                let extra_prompts = settings
                    .prompts
                    .clone()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|p| resolve_path(&cwd, &p))
                    .collect();
                let loader = ResourceLoader::new(&cwd)
                    .with_agent_dir(&agent_dir)
                    .with_trust(trusted)
                    .with_extra_skill_paths(extra_skills)
                    .with_extra_prompt_paths(extra_prompts);
                loader.snapshot().await?
            }
        };

        let session_path = session.storage().path().to_path_buf();
        // TS default tool set: read/bash/edit/write only.
        // The registry mounts ALL seven built-in tools (TS); the initial
        // ACTIVE subset is the TS default four, so grep/find/ls remain
        // enableable later via set_active_tools.
        let tools: Vec<Arc<dyn AgentTool>> = if self.tools.is_empty() {
            vec![
                Arc::new(ReadTool) as Arc<dyn AgentTool>,
                Arc::new(BashTool::new(settings.shell_command_prefix.clone()))
                    as Arc<dyn AgentTool>,
                Arc::new(EditTool) as Arc<dyn AgentTool>,
                Arc::new(WriteTool) as Arc<dyn AgentTool>,
                Arc::new(GrepTool) as Arc<dyn AgentTool>,
                Arc::new(FindTool) as Arc<dyn AgentTool>,
                Arc::new(LsTool) as Arc<dyn AgentTool>,
            ]
        } else {
            self.tools.clone()
        };
        // The facade's initial active subset: the TS default four when no
        // custom tool list is given. It drives BOTH the initial system
        // prompt and the in-memory harness state, and is the restore default
        // when a reopened path carries no `active_tools_change` entry.
        let facade_initial_active: Option<Vec<String>> = if self.tools.is_empty() {
            Some(vec![
                "read".into(),
                "bash".into(),
                "edit".into(),
                "write".into(),
            ])
        } else {
            None
        };
        let active_tools: Vec<String> = facade_initial_active
            .clone()
            .unwrap_or_else(|| tools.iter().map(|t| t.name().to_string()).collect());
        let base_prompt = self
            .system_prompt
            .clone()
            .unwrap_or_else(|| crate::system_prompt::DEFAULT_BASE_PROMPT.into());
        let custom_prompt = self.system_prompt.is_some();
        let system_prompt = crate::system_prompt::build_harness_prompt(
            &base_prompt,
            &cwd,
            &active_tools,
            &resources.context_files,
            &resources.skills,
            self.system_prompt.is_some(),
        );

        // The session's own model when reopening (restore), the builder
        // override or settings default otherwise.
        let session_model = if restore {
            session
                .build_session_context()
                .await
                .ok()
                .and_then(|c| c.model)
        } else {
            None
        };
        let runtime = match &self.model_runtime {
            Some(runtime) => runtime.clone(),
            None => ModelRuntime::from_env(),
        };
        let settings_thinking_default = settings.default_thinking_level.clone();

        // The settings/catalog initial model — resolved through the runtime
        // catalog (never a hand-built guess of protocol or parameters).
        let settings_initial_model = settings.default_model.as_ref().and_then(|id| {
            let provider = settings
                .default_provider
                .clone()
                .unwrap_or_else(|| "anthropic".into());
            runtime.resolve_model(&provider, id)
        });
        let mut fallback_notice = None;
        let model = match &self.model {
            Some(model) => model.clone(),
            None => match &session_model {
                // Exact restored model first; a miss falls back to the
                // settings/catalog initial model, else the default, with an
                // observable notice (TS `modelFallbackMessage`).
                Some(mref) => match runtime.resolve_model(&mref.provider, &mref.model_id) {
                    Some(model) => model,
                    None => {
                        fallback_notice = Some(format!(
                            "session model {}/{} could not be restored exactly; \
                             using the initial model instead",
                            mref.provider, mref.model_id
                        ));
                        settings_initial_model.clone().unwrap_or_else(default_model)
                    }
                },
                None => settings_initial_model.unwrap_or_else(default_model),
            },
        };

        // Capture the model identity before it moves into the harness; the
        // thinking clamp needs it after restore.
        let model_key = (model.provider.clone(), model.id.clone(), model.thinking);

        // A new session persists its initial model and thinking entries (TS
        // writes them on session creation); they buffer in the deferred
        // storage until the first assistant message materializes the file,
        // so a close/reopen or a pre-first-message fork projects the same
        // configuration.
        let has_thinking_entry = if restore {
            session.build_session_context().await?.has_thinking_entry
        } else {
            let _ = session
                .append_model_change(&model.provider, &model.id)
                .await?;
            let _ = session
                .append_thinking_level_change(
                    clamp_thinking(
                        &model,
                        &runtime.thinking_levels(&model),
                        settings.default_thinking_level.clone(),
                    )
                    .unwrap_or_else(|| "off".into())
                    .as_str(),
                )
                .await?;
            true
        };

        let stream_fn = runtime.resolver()(&model)?;
        let resolver = runtime.resolver();
        let mut harness = AgentHarness::new(session, system_prompt.clone(), model, stream_fn)
            .with_stream_resolver(resolver.clone())
            .with_tools(Arc::from(tools.clone()))
            .with_tool_cwd(cwd.clone())
            .with_resources(resources);
        if let Some(names) = facade_initial_active {
            // Initial active subset: the TS default four. In-memory only —
            // no `active_tools_change` entry is written for the default; a
            // restore with no entry also falls back to these four.
            harness.set_initial_active_tools(names.clone());
            harness.set_restore_active_tool_default(names);
        }
        if self.model.is_none() {
            // Only install the model resolver when the caller did NOT
            // explicitly set a model: `builder.with_model(B).open()` must
            // keep B (TS `options.model > restored model`), and restore
            // would otherwise overwrite it with the session's model.
            harness = harness.with_model_resolver(model_resolver(&runtime));
        }
        // Install the prompt builder: the harness rebuilds the system prompt
        // whenever the effective tool selection or resources change, so the
        // model never sees tools/skills that are not actually available.
        let base_prompt_for_builder = base_prompt.clone();
        let cwd_for_builder = cwd.clone();
        let custom_for_builder = custom_prompt;
        harness.set_system_prompt_builder(
            move |active_tools: &[String], resources: &HarnessResources| {
                crate::system_prompt::build_harness_prompt(
                    &base_prompt_for_builder,
                    &cwd_for_builder,
                    active_tools,
                    &resources.context_files,
                    &resources.skills,
                    custom_for_builder,
                )
            },
        );
        // Attach the request observer so the harness's before-payload /
        // after-response hooks fire on the real provider requests.
        let observer = harness.request_observer();
        harness.rebind_stream_resolver(runtime.resolver_with_observer(observer));
        // Apply the loaded non-UI settings: compaction, retry, and queue
        // modes are in-memory-only setters, safe for a reopen. The default
        // thinking tier applies only to a NEW session and only as initial
        // in-memory state — never through the persisting setter, which would
        // append a `thinking_level_change` entry and overwrite the session's
        // persisted tier on every reopen.
        harness.set_compaction_settings(settings.resolved_compaction());
        harness.set_retry_settings(settings.resolved_retry());
        if let Some(mode) = settings.resolved_steering_mode() {
            harness.set_steering_mode(mode);
        }
        if let Some(mode) = settings.resolved_follow_up_mode() {
            harness.set_follow_up_mode(mode);
        }
        if let Some(enabled) = settings.show_cache_miss_notices {
            harness.set_show_cache_miss_notices(enabled);
        }
        if let Some(reserve) = settings.resolved_branch_summary_reserve() {
            harness.set_branch_summary_reserve(reserve);
        }
        if restore {
            harness.restore().await?;
        }
        // Apply the clamped thinking level in-memory to BOTH the agent state
        // and the shared runtime snapshot — for a NEW session (so the first
        // turn matches the persisted entry and a reopen) and for a reopened
        // session with no persisted entry. An explicit persisted `"off"` is
        // a real decision and is never overridden.
        // The final thinking level: the session's persisted level (a reopen
        // with an entry), else the settings default, else the TS default
        // `medium` — clamped against the FINAL model's capability in every
        // case (TS clamps again after a model fallback). One value lands in
        // memory, so the first turn equals a reopen.
        let desired = if restore && has_thinking_entry {
            // A persisted entry is a real preference — an explicit `"off"`
            // must survive the reopen (the projection folds it to `None`).
            Some(
                harness
                    .agent()
                    .state()
                    .thinking_level
                    .clone()
                    .unwrap_or_else(|| "off".into()),
            )
        } else {
            settings.default_thinking_level.clone()
        };
        let clamp_model = Model {
            provider: model_key.0.clone(),
            api: String::new(),
            id: model_key.1.clone(),
            context_window: 0,
            max_tokens: 0,
            thinking: model_key.2,
            metadata: Default::default(),
        };
        let levels = runtime.thinking_levels(&clamp_model);
        harness.set_initial_thinking_level(clamp_thinking(&clamp_model, &levels, desired));

        Ok(AgentSession {
            harness,
            session_path,
            session_dir,
            cwd,
            runtime,
            system_prompt,
            base_prompt,
            tools,
            pending_next_turn: Arc::new(std::sync::Mutex::new(Vec::new())),
            model_fallback_notice: fallback_notice,
            settings_thinking_default,
            custom_prompt,
            agent_dir,
        })
    }

    /// Open (or create) the session and assemble the harness.
    pub async fn build(self) -> Result<AgentSession, anyhow::Error> {
        let cwd = self
            .cwd
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let session_dir = self
            .session_dir
            .clone()
            .unwrap_or_else(|| cwd.join(".pi-sessions"));
        tokio::fs::create_dir_all(&session_dir).await?;
        let repo = SessionRepository::new(&session_dir);
        let session = repo
            .create(JsonlSessionMetadata {
                id: uuid::Uuid::new_v4().to_string(),
                cwd: cwd.to_string_lossy().into_owned(),
                created_at: chrono::Utc::now(),
                parent_session_path: None,
                metadata: None,
            })
            .await?;
        self.assemble_common(session, cwd, session_dir, false).await
    }

    /// Open an existing session file, restoring its model / thinking /
    /// active tools and its full assembly (settings, trust, resources,
    /// system prompt, tools, runtime) before returning — callers must not
    /// restore manually. Tools and the returned session cwd follow the
    /// session's own project directory.
    pub async fn open(self, path: PathBuf) -> Result<AgentSession, anyhow::Error> {
        let session_dir = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let repo = SessionRepository::new(&session_dir);
        let session = repo.open(&path).await?;
        let cwd = std::path::PathBuf::from(session.storage().metadata.cwd.clone());
        self.assemble_common(session, cwd, session_dir, true).await
    }
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
        use crate::coding_agent::model_runtime::ModelCatalog;
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stream: Arc<dyn StreamFn> = Arc::new(Scripted(Arc::clone(&calls)));
        let resolver: crate::agent_loop::StreamResolver =
            Arc::new(move |_model: &Model| Ok(Arc::clone(&stream)));
        // A catalog that restores the test model, so reopen resolves it
        // exactly instead of erroring.
        struct TestCatalog;
        impl ModelCatalog for TestCatalog {
            fn resolve(&self, provider: &str, _model_id: &str) -> Option<Model> {
                (provider == "test").then(test_model)
            }
        }
        ModelRuntime::new(resolver).with_catalog(Arc::new(TestCatalog))
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

    /// Tools execute against the session cwd, not the process cwd: a file
    /// that exists only in the session project is reachable by read/ls.
    #[tokio::test]
    async fn tools_run_against_the_session_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::write(cwd.join("only-in-project.txt"), "project file")
            .await
            .unwrap();

        let session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();

        // The read tool resolves relative to the session cwd, not the
        // process cwd (the crate's root has no such file).
        let tool = session
            .harness
            .agent()
            .tools()
            .iter()
            .find(|t| t.name() == "read")
            .expect("read mounted")
            .clone();
        let ctx = Arc::clone(session.harness.agent().tool_context());
        let signal = tokio_util::sync::CancellationToken::new();
        let result = tool
            .execute(
                "t1",
                serde_json::json!({ "path": "only-in-project.txt" }),
                signal,
                &*ctx,
            )
            .await
            .unwrap();
        assert!(
            !result.is_error,
            "read must resolve relative to the session cwd: {:?}",
            result.details
        );
    }

    /// build → close → open with a WRONG cwd → fork: the session metadata
    /// cwd wins over the caller-provided one, so tools/resources stay in the
    /// session project.
    #[tokio::test]
    async fn reopen_and_fork_use_the_session_cwd_over_a_wrong_one() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        tokio::fs::create_dir_all(&project).await.unwrap();
        tokio::fs::write(project.join("CLAUDE.md"), "project rules")
            .await
            .unwrap();
        tokio::fs::write(project.join("only-in-project.txt"), "file")
            .await
            .unwrap();

        let mut session = create_agent_session()
            .with_cwd(&project)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        session.prompt("first").await.unwrap();
        let path = session.close().await.unwrap();

        // Reopen with a wrong cwd: the session metadata cwd wins.
        let elsewhere = dir.path().join("elsewhere");
        tokio::fs::create_dir_all(&elsewhere).await.unwrap();
        let resumed = create_agent_session()
            .with_cwd(&elsewhere)
            .with_model_runtime(fake_runtime())
            .open(path.clone())
            .await
            .unwrap();
        assert_eq!(resumed.harness_messages().len(), 2, "transcript restored");
        assert_eq!(resumed.resources().context_files.len(), 1);
        let read = resumed
            .harness
            .agent()
            .tools()
            .iter()
            .find(|t| t.name() == "read")
            .unwrap()
            .clone();
        let ctx = Arc::clone(resumed.harness.agent().tool_context());
        let result = read
            .execute(
                "t1",
                serde_json::json!({ "path": "only-in-project.txt" }),
                tokio_util::sync::CancellationToken::new(),
                &*ctx,
            )
            .await
            .unwrap();
        assert!(
            !result.is_error,
            "tools run in the session cwd after reopen"
        );

        let entries = resumed
            .harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap();
        let first_user = entries
            .iter()
            .filter_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::User { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .next()
            .unwrap();
        let ForkResult { session: fork, .. } = resumed
            .fork(&first_user, ForkPosition::AtEntry)
            .await
            .unwrap();
        let ctx = Arc::clone(fork.harness.agent().tool_context());
        let result = read
            .execute(
                "t1",
                serde_json::json!({ "path": "only-in-project.txt" }),
                tokio_util::sync::CancellationToken::new(),
                &*ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error, "forked session keeps the project cwd");
    }

    /// A model id can be served by either protocol; an injected catalog
    /// pins it, and a session switched to it reopens on the same protocol.
    #[tokio::test]
    async fn reopen_keeps_model_on_its_original_api_via_catalog() {
        use crate::coding_agent::model_runtime::ModelCatalog;

        struct PinApi(&'static str);
        impl ModelCatalog for PinApi {
            fn resolve(&self, provider: &str, model_id: &str) -> Option<Model> {
                (provider == "openai" && model_id == "gpt-5-mini").then(|| Model {
                    provider: "openai".into(),
                    api: self.0.into(),
                    id: "gpt-5-mini".into(),
                    context_window: 200_000,
                    max_tokens: 16_384,
                    thinking: crate::types::ThinkingKind::None,
                    metadata: Default::default(),
                })
            }
        }

        fn runtime(api: &'static str) -> ModelRuntime {
            let stream: Arc<dyn StreamFn> =
                Arc::new(Scripted(Arc::new(std::sync::Mutex::new(Vec::new()))));
            let resolver: crate::agent_loop::StreamResolver =
                Arc::new(move |_m: &Model| Ok(Arc::clone(&stream)));
            ModelRuntime::new(resolver).with_catalog(Arc::new(PinApi(api)))
        }

        for api in ["openai_completions", "openai_responses"] {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().join("proj");
            tokio::fs::create_dir_all(&cwd).await.unwrap();
            let model = Model {
                provider: "openai".into(),
                api: api.into(),
                id: "gpt-5-mini".into(),
                context_window: 200_000,
                max_tokens: 16_384,
                thinking: crate::types::ThinkingKind::None,
                metadata: Default::default(),
            };
            let agent_dir = dir.path().join("agent");
            tokio::fs::create_dir_all(&agent_dir).await.unwrap();
            let mut session = create_agent_session()
                .with_cwd(&cwd)
                .with_session_dir(dir.path().join("sessions"))
                .with_agent_dir(&agent_dir)
                .with_model_runtime(runtime(api))
                .with_model(model.clone())
                .build()
                .await
                .unwrap();
            // Prompt first, then switch: the model_change entry is the last
            // model-bearing entry on the path, so the reopen projects it.
            session.prompt("hi").await.unwrap();
            session.set_model(model.clone()).await.unwrap();
            let path = session.close().await.unwrap();

            // Reopen with the same catalog: the session model restores on
            // the exact api, not a heuristic.
            let resumed = create_agent_session()
                .with_model_runtime(runtime(api))
                .open(path)
                .await
                .unwrap();
            assert_eq!(resumed.model().api, api, "api preserved for {api}");
            assert_eq!(resumed.model().id, "gpt-5-mini");
        }
    }

    /// An untrusted project's settings are treated as an empty config:
    /// `.pi/settings.json` cannot steer the shell prefix or resource paths.
    #[tokio::test]
    async fn untrusted_project_settings_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        tokio::fs::create_dir_all(cwd.join(".pi/skills"))
            .await
            .unwrap();
        tokio::fs::write(cwd.join(".pi/skills/proj.md"), "project skill")
            .await
            .unwrap();
        // Project settings attempt to set a shell prefix and an external
        // skill path — untrusted, so neither applies.
        tokio::fs::create_dir_all(cwd.join(".pi")).await.unwrap();
        tokio::fs::write(
            cwd.join(".pi/settings.json"),
            r#"{"shellCommandPrefix": "sudo", "skills": ["/ext/skills"]}"#,
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(dir.path().join("ext/skills"))
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("ext/skills/ext.md"), "external skill")
            .await
            .unwrap();
        // No trust decision: project treated as untrusted.
        let session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        let skill_names: Vec<&str> = session
            .resources()
            .skills
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert!(
            !skill_names.contains(&"ext"),
            "external skill path must not load from untrusted project settings: {skill_names:?}"
        );
        assert!(
            !skill_names.contains(&"proj"),
            "project skills skipped when untrusted: {skill_names:?}"
        );
    }

    /// The system prompt is rebuilt when the active tools change, so skills
    /// disappear once the read tool is disabled.
    #[tokio::test]
    async fn prompt_rebuilds_after_active_tool_changes() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(cwd.join(".pi/skills"))
            .await
            .unwrap();
        tokio::fs::write(
            cwd.join(".pi/skills/review.md"),
            "---\nname: review\n---\nCheck the diff.",
        )
        .await
        .unwrap();
        let mut trust = crate::trust::TrustManager::new();
        trust.trust(&cwd);
        trust.save(&agent_dir.join("trust.json")).unwrap();
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        let system_prompt = session.harness.agent().state().system_prompt.clone();
        assert!(
            system_prompt.contains(r#"<skill name="review">"#),
            "{system_prompt}"
        );

        // Disable read: the skill index must disappear from the prompt.
        session.set_active_tools(vec!["bash".into()]).await.unwrap();
        let system_prompt = session.harness.agent().state().system_prompt.clone();
        assert!(!system_prompt.contains("<skill"), "{system_prompt}");
    }

    /// Facade next_turn delivers AFTER the prompt's user message (user-first
    /// asides); the harness's own queue stays queued-first.
    #[tokio::test]
    async fn facade_next_turn_is_delivered_after_the_prompt() {
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
        session.next_turn("aside");
        assert!(session.has_next_turn());
        let messages = session.prompt("direct").await.unwrap();
        assert!(!session.has_next_turn());
        let texts: Vec<String> = messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::User { content, .. } => content.iter().find_map(|b| match b {
                    ContentBlock::Text { text, .. } => Some(text.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["direct", "aside"], "{texts:?}");
    }

    /// All seven built-ins are registered; the initial ACTIVE subset is the
    /// TS default four, survives reopen, and grep/find/ls stay enableable.
    #[tokio::test]
    async fn default_tool_registry_seven_active_four_across_reopen() {
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
        // Registry: all seven. Active: the default four.
        let registry: Vec<String> = session.tools();
        assert_eq!(registry.len(), 7, "{registry:?}");
        assert_eq!(
            session.active_tool_names().unwrap(),
            vec!["read", "bash", "edit", "write"]
        );
        // The system prompt advertises the four, not the seven.
        let prompt = session.harness.agent().state().system_prompt.clone();
        assert!(prompt.contains("- read: Read a file"));
        assert!(!prompt.contains("- grep:"), "{prompt}");
        session.prompt("hi").await.unwrap();
        let path = session.close().await.unwrap();

        // Reopen: the default four persist (no active_tools_change entry).
        let resumed = create_agent_session()
            .with_model_runtime(fake_runtime())
            .open(path)
            .await
            .unwrap();
        assert_eq!(
            resumed.active_tool_names().unwrap(),
            vec!["read", "bash", "edit", "write"],
            "reopen keeps the default four"
        );
        let prompt = resumed.harness.agent().state().system_prompt.clone();
        assert!(!prompt.contains("- grep:"), "{prompt}");

        // grep stays enableable after the fact.
        let mut resumed = resumed;
        resumed.set_active_tools(vec!["grep".into()]).await.unwrap();
        assert_eq!(resumed.active_tool_names().unwrap(), vec!["grep"]);
        let prompt = resumed.harness.agent().state().system_prompt.clone();
        assert!(prompt.contains("- grep: Search file contents"), "{prompt}");
    }

    /// The first turn's thinking level equals a reopen's (both clamped and
    /// persisted); an explicit persisted "off" beats a settings default.
    #[tokio::test]
    async fn thinking_first_turn_matches_reopen_and_off_wins_over_settings() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        // Settings default: high.
        tokio::fs::create_dir_all(agent_dir.join("settings.json").parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            agent_dir.join("settings.json"),
            r#"{"defaultThinkingLevel": "high"}"#,
        )
        .await
        .unwrap();

        // Non-thinking model: the default clamps to off, and the first turn
        // matches a reopen.
        let non_thinking = Model {
            thinking: crate::types::ThinkingKind::None,
            ..test_model()
        };
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .with_model(non_thinking)
            .build()
            .await
            .unwrap();
        let first_level = session.harness.agent().state().thinking_level.clone();
        assert_eq!(first_level, None, "non-thinking model clamps to off (None)");
        session.prompt("hi").await.unwrap();
        let path = session.close().await.unwrap();
        let resumed = create_agent_session()
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .open(path)
            .await
            .unwrap();
        assert_eq!(
            resumed.harness.agent().state().thinking_level,
            None,
            "reopen matches the first turn (off)"
        );
    }

    /// A catalog miss on reopen falls back to the initial model with an
    /// observable notice.
    #[tokio::test]
    async fn catalog_miss_falls_back_with_notice() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(&agent_dir).await.unwrap();
        let mut session = create_agent_session()
            .with_agent_dir(&agent_dir)
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        session.prompt("hi").await.unwrap();
        // Switch to a model the catalog cannot resolve (unknown provider).
        let unknown = Model {
            provider: "other".into(),
            id: "unknown-model".into(),
            ..test_model()
        };
        session.set_model(unknown).await.unwrap();
        let path = session.close().await.unwrap();

        let resumed = create_agent_session()
            .with_model_runtime(fake_runtime())
            .open(path)
            .await
            .unwrap();
        assert!(
            resumed.model_fallback_notice().is_some(),
            "fallback notice expected"
        );
        assert!(
            resumed
                .model_fallback_notice()
                .unwrap()
                .contains("unknown-model")
        );
    }

    /// Fork keeps the prompt builder: changing tools after forking rebuilds
    /// the fork's prompt.
    #[tokio::test]
    async fn fork_keeps_the_prompt_builder() {
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
        let first_user = entries
            .iter()
            .filter_map(|e| match e {
                SessionTreeEntry::Message {
                    id,
                    message: AgentMessage::User { .. },
                    ..
                } => Some(id.clone()),
                _ => None,
            })
            .next()
            .unwrap();
        let ForkResult {
            session: mut forked,
            ..
        } = session
            .fork(&first_user, ForkPosition::AtEntry)
            .await
            .unwrap();
        // Narrow the fork's tools; the prompt must rebuild (no grep).
        forked
            .set_active_tools(vec!["read".into(), "edit".into()])
            .await
            .unwrap();
        let prompt = forked.harness.agent().state().system_prompt.clone();
        assert!(prompt.contains("- read: Read a file"));
        assert!(!prompt.contains("- grep:"), "{prompt}");
    }

    /// Facade shutdown clears its pending next-turn queue and refuses more.
    #[tokio::test]
    async fn facade_shutdown_clears_pending_next_turn() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        session.next_turn("queued");
        assert!(session.has_next_turn());
        session.request_shutdown();
        assert!(!session.has_next_turn(), "shutdown clears the facade queue");
        session.next_turn("after");
        assert!(
            !session.has_next_turn(),
            "shutdown refuses further enqueues"
        );
    }

    /// `builder.with_model(B).open(path)` keeps B even when the session
    /// recorded model A (TS `options.model > restored model`); the first
    /// provider request is actually served by B.
    #[tokio::test]
    async fn open_keeps_an_explicit_model_override() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        // A recording runtime: the stream notes the model id it served.
        fn recording_runtime(seen: Arc<std::sync::Mutex<Vec<String>>>) -> ModelRuntime {
            struct Recording(Arc<std::sync::Mutex<Vec<String>>>);
            #[async_trait::async_trait]
            impl StreamFn for Recording {
                async fn stream(
                    &self,
                    context: &AgentContext,
                    _signal: tokio_util::sync::CancellationToken,
                    _event_tx: tokio::sync::mpsc::Sender<AgentEvent>,
                ) -> Result<AgentMessage, anyhow::Error> {
                    self.0.lock().unwrap().push(context.model.id.clone());
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
            struct TestCatalog;
            impl crate::coding_agent::model_runtime::ModelCatalog for TestCatalog {
                fn resolve(&self, _provider: &str, _model_id: &str) -> Option<Model> {
                    Some(test_model())
                }
            }
            let stream: Arc<dyn StreamFn> = Arc::new(Recording(seen));
            let resolver: crate::agent_loop::StreamResolver =
                Arc::new(move |_m: &Model| Ok(Arc::clone(&stream)));
            ModelRuntime::new(resolver).with_catalog(Arc::new(TestCatalog))
        }

        let seen_first = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(recording_runtime(Arc::clone(&seen_first)))
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        session.prompt("hi").await.unwrap();
        let path = session.close().await.unwrap();

        // Reopen with an explicit model B and actually prompt.
        let model_b = Model {
            id: "model-b".into(),
            ..test_model()
        };
        let seen_second = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut resumed = create_agent_session()
            .with_model_runtime(recording_runtime(Arc::clone(&seen_second)))
            .with_model(model_b.clone())
            .open(path)
            .await
            .unwrap();
        assert_eq!(resumed.model().id, "model-b", "explicit model wins");
        resumed.prompt("go").await.unwrap();
        assert_eq!(
            *seen_second.lock().unwrap(),
            vec!["model-b".to_string()],
            "the first provider request must be served by B"
        );
    }

    /// A thinking-capable default model (Sonnet, per the frozen TS baseline)
    /// keeps the `medium` default instead of being clamped to off.
    #[tokio::test]
    async fn thinking_capable_model_keeps_medium_default() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        use crate::coding_agent::model_runtime::ModelCatalog;
        struct SonnetCatalog;
        impl ModelCatalog for SonnetCatalog {
            fn resolve(&self, provider: &str, model_id: &str) -> Option<Model> {
                (provider == "anthropic" && model_id == "claude-sonnet-4-6").then(|| Model {
                    provider: "anthropic".into(),
                    api: "anthropic".into(),
                    id: "claude-sonnet-4-6".into(),
                    context_window: 200_000,
                    max_tokens: 8_192,
                    thinking: crate::types::ThinkingKind::Enabled,
                    metadata: Default::default(),
                })
            }
        }
        let sonnet = SonnetCatalog
            .resolve("anthropic", "claude-sonnet-4-6")
            .unwrap();
        let session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(fake_runtime().with_catalog(Arc::new(SonnetCatalog)))
            .with_model(sonnet)
            .build()
            .await
            .unwrap();
        // Medium default is kept for a thinking-capable model.
        assert_eq!(
            session.harness.agent().state().thinking_level.as_deref(),
            Some("medium"),
            "thinking-capable model keeps medium"
        );
    }

    /// set_model re-clamps thinking: thinking high -> non-thinking becomes
    /// off (in-memory AND persisted); non-thinking -> thinking restores the
    /// settings default.
    #[tokio::test]
    async fn set_model_reclamps_thinking_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(&agent_dir).await.unwrap();
        tokio::fs::write(
            agent_dir.join("settings.json"),
            r#"{"defaultThinkingLevel": "high"}"#,
        )
        .await
        .unwrap();
        // A thinking model.
        let thinking = Model {
            thinking: crate::types::ThinkingKind::Enabled,
            ..test_model()
        };
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .with_model(thinking)
            .build()
            .await
            .unwrap();
        // high (settings default, thinking-capable).
        assert_eq!(
            session.harness.agent().state().thinking_level.as_deref(),
            Some("high")
        );

        // Switch to a non-thinking model: clamped to off in-memory.
        let non_thinking = Model {
            thinking: crate::types::ThinkingKind::None,
            id: "non-thinking".into(),
            ..test_model()
        };
        session.set_model(non_thinking).await.unwrap();
        assert_eq!(
            session.harness.agent().state().thinking_level,
            None,
            "high clamps to off for a non-thinking model"
        );

        // Switch back to a thinking model: restores the settings default.
        let thinking2 = Model {
            thinking: crate::types::ThinkingKind::Enabled,
            id: "thinking-2".into(),
            ..test_model()
        };
        session.set_model(thinking2).await.unwrap();
        assert_eq!(
            session.harness.agent().state().thinking_level.as_deref(),
            Some("high"),
            "non-thinking -> thinking restores the settings default"
        );
        // The persisted entries reflect the clamps.
        let entries = session
            .harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap();
        let levels: Vec<String> = entries
            .iter()
            .filter_map(|e| match e {
                SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } => {
                    Some(thinking_level.clone())
                }
                _ => None,
            })
            .collect();
        assert!(levels.iter().any(|l| l == "off"), "{levels:?}");
        assert_eq!(
            levels.last().map(String::as_str),
            Some("high"),
            "{levels:?}"
        );
    }

    /// A custom catalog can restrict a model's supported thinking levels
    /// (TS `thinkingLevelMap`), and xhigh clamps to the supported max.
    /// `set_thinking_level(None)` reads as `"off"` (facade contract): on a
    /// thinking model it disables thinking, stays off across a prompt, and a
    /// reopen projects the same off.
    #[tokio::test]
    async fn set_thinking_level_none_disables_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        let session_dir = dir.path().join("sessions");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(&agent_dir).await.unwrap();
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(&session_dir)
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .with_model(Model {
                thinking: crate::types::ThinkingKind::Enabled,
                ..test_model()
            })
            .build()
            .await
            .unwrap();
        // None == off: never interpreted as "no preference -> medium".
        session.set_thinking_level(None).await.unwrap();
        assert_eq!(
            session.harness.agent().state().thinking_level,
            None,
            "None disables thinking on a thinking-capable model"
        );
        // A real prompt runs with thinking off.
        let _ = session.prompt("hello").await.unwrap();
        // Reopen projects the persisted off (not a fallback to medium).
        let reopened = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(&session_dir)
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .open(session.harness.session().storage().path().to_path_buf())
            .await
            .unwrap();
        assert_eq!(
            reopened.harness.agent().state().thinking_level,
            None,
            "persisted off survives reopen on a thinking-capable model"
        );
    }

    /// Thinking entries persist only when the effective level changes: a
    /// high -> high model switch writes no extra entry; the preference
    /// survives a non-thinking round trip and a fork.
    #[tokio::test]
    async fn thinking_entries_are_deduplicated_and_preference_survives() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        let session_dir = dir.path().join("sessions");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(&agent_dir).await.unwrap();
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(&session_dir)
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .with_model(Model {
                thinking: crate::types::ThinkingKind::Enabled,
                ..test_model()
            })
            .build()
            .await
            .unwrap();
        // Raise the preference to high (one thinking entry).
        session
            .set_thinking_level(Some("high".into()))
            .await
            .unwrap();

        // A high -> high model switch changes nothing: no extra entry.
        let another_thinking = Model {
            thinking: crate::types::ThinkingKind::Enabled,
            id: "thinking-b".into(),
            ..test_model()
        };
        session.set_model(another_thinking).await.unwrap();
        assert_eq!(
            session.harness.agent().state().thinking_level.as_deref(),
            Some("high")
        );
        async fn thinking_entry_count(s: &AgentSession) -> usize {
            s.harness
                .session()
                .storage()
                .get_entries()
                .await
                .unwrap()
                .iter()
                .filter(|e| matches!(e, SessionTreeEntry::ThinkingLevelChange { .. }))
                .count()
        }
        // Assembly persisted the initial medium entry; the explicit high is
        // the second. A high -> high model switch must not add a third.
        assert_eq!(
            thinking_entry_count(&session).await,
            2,
            "initial medium + explicit high; high -> high adds nothing"
        );

        // Non-thinking round trip keeps the high preference: switching back
        // restores high, not the assembly default.
        session
            .set_model(Model {
                thinking: crate::types::ThinkingKind::None,
                id: "flat".into(),
                ..test_model()
            })
            .await
            .unwrap();
        session
            .set_model(Model {
                thinking: crate::types::ThinkingKind::Enabled,
                id: "thinking-c".into(),
                ..test_model()
            })
            .await
            .unwrap();
        assert_eq!(
            session.harness.agent().state().thinking_level.as_deref(),
            Some("high"),
            "user preference, not the assembly default, survives a round trip"
        );

        // A fork inherits the preference (not the assembly default).
        let entries = session
            .harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap();
        let leaf = entries
            .iter()
            .rev()
            .find(|e| matches!(e, SessionTreeEntry::ThinkingLevelChange { .. }))
            .map(|e| e.id().to_string())
            .unwrap();
        // A fork needs a materialized session file: the deferred-first
        // assistant contract writes disk only once an assistant message
        // exists, so run one prompt before branching.
        let _ = session.prompt("branch here").await.unwrap();
        let mut fork = session
            .fork(&leaf, ForkPosition::AtEntry)
            .await
            .unwrap()
            .session;
        fork.set_model(Model {
            thinking: crate::types::ThinkingKind::None,
            id: "flat-2".into(),
            ..test_model()
        })
        .await
        .unwrap();
        fork.set_model(Model {
            thinking: crate::types::ThinkingKind::Enabled,
            id: "thinking-d".into(),
            ..test_model()
        })
        .await
        .unwrap();
        assert_eq!(
            fork.harness.agent().state().thinking_level.as_deref(),
            Some("high"),
            "fork inherits the preference"
        );
    }

    /// `patch_global_settings` preserves fields the typed `Settings` does not
    /// model (theme, packages, nested objects): only the patched keys change.
    #[tokio::test]
    async fn settings_patch_preserves_unknown_and_nested_fields() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(&agent_dir).await.unwrap();
        // A settings file with known + unknown + nested fields.
        tokio::fs::write(
            agent_dir.join("settings.json"),
            r##"{
                "defaultModel": "claude-sonnet-4-6",
                "defaultProvider": "anthropic",
                "theme": {"base": "dark", "accent": "#f0a"},
                "packages": ["rust", "python"],
                "extensions": {"foo": {"enabled": true}},
                "enabledModels": ["sonnet", "opus"]
            }"##,
        )
        .await
        .unwrap();
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .with_model(Model {
                thinking: crate::types::ThinkingKind::Enabled,
                ..test_model()
            })
            .build()
            .await
            .unwrap();
        // A model switch patches defaultModel/defaultProvider only.
        session
            .set_model(Model {
                provider: "openai".into(),
                api: "openai".into(),
                id: "gpt-5".into(),
                thinking: crate::types::ThinkingKind::Enabled,
                context_window: 200_000,
                max_tokens: 16_384,
                metadata: Default::default(),
            })
            .await
            .unwrap();
        let raw = tokio::fs::read_to_string(agent_dir.join("settings.json"))
            .await
            .unwrap();
        let root: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let obj = root.as_object().unwrap();
        // Patched keys updated.
        assert_eq!(obj["defaultModel"], "gpt-5");
        assert_eq!(obj["defaultProvider"], "openai");
        // Unknown scalar, nested object, array, and nested-boolean fields
        // all survive the patch.
        assert_eq!(obj["theme"]["base"], "dark");
        assert_eq!(obj["theme"]["accent"], "#f0a");
        assert_eq!(obj["packages"][0], "rust");
        assert_eq!(obj["packages"][1], "python");
        assert_eq!(obj["extensions"]["foo"]["enabled"], true);
        assert_eq!(obj["enabledModels"][0], "sonnet");
    }

    /// A corrupt global settings file is not silently wiped to `{}`: the
    /// parse error propagates and the on-disk content is left intact for the
    /// user to repair.
    #[tokio::test]
    async fn patch_global_settings_propagates_parse_error_without_wiping() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(&agent_dir).await.unwrap();
        // Build with no settings file so construction succeeds, then corrupt
        // the file mid-session (the realistic path to a bad file at patch
        // time).
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .with_model(Model {
                thinking: crate::types::ThinkingKind::Enabled,
                ..test_model()
            })
            .build()
            .await
            .unwrap();
        let corrupt = "{ not valid json";
        tokio::fs::write(agent_dir.join("settings.json"), corrupt)
            .await
            .unwrap();
        let result = session
            .set_model(Model {
                id: "x".into(),
                ..test_model()
            })
            .await;
        assert!(
            result.is_err(),
            "corrupt settings must error, got {result:?}"
        );
        let raw = tokio::fs::read_to_string(agent_dir.join("settings.json"))
            .await
            .unwrap();
        assert_eq!(raw, corrupt, "corrupt settings must not be wiped");
    }

    /// A first-time user whose agent dir does not yet exist can still switch
    /// models: `patch_global_settings` creates the parent dir before the
    /// temp-file write.
    #[tokio::test]
    async fn patch_global_settings_creates_missing_agent_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        // Deliberately do NOT create the agent dir; settings has no file.
        let agent_dir = dir.path().join("agent");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .with_model(Model {
                thinking: crate::types::ThinkingKind::Enabled,
                ..test_model()
            })
            .build()
            .await
            .unwrap();
        session
            .set_model(Model {
                provider: "openai".into(),
                api: "openai".into(),
                id: "gpt-5".into(),
                thinking: crate::types::ThinkingKind::Enabled,
                context_window: 200_000,
                max_tokens: 16_384,
                metadata: Default::default(),
            })
            .await
            .unwrap();
        assert!(agent_dir.join("settings.json").exists());
    }

    /// An explicit `off` preference on a thinking model survives a round
    /// trip through a non-thinking model (and a fork): the preference is
    /// stored as `Some("off")`, never folded to `None`, so switching back to
    /// a thinking model restores `off` (not `medium`).
    #[tokio::test]
    async fn explicit_off_preference_survives_non_thinking_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        let session_dir = dir.path().join("sessions");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(&agent_dir).await.unwrap();
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(&session_dir)
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .with_model(Model {
                thinking: crate::types::ThinkingKind::Enabled,
                ..test_model()
            })
            .build()
            .await
            .unwrap();
        // Explicit off on a thinking model.
        session.set_thinking_level(None).await.unwrap();
        assert_eq!(session.harness.agent().state().thinking_level, None);

        // Round trip through a non-thinking model: the off preference must
        // not be overwritten (a non-thinking model clamps to off and TS does
        // not store off for non-thinking models).
        session
            .set_model(Model {
                thinking: crate::types::ThinkingKind::None,
                id: "flat".into(),
                ..test_model()
            })
            .await
            .unwrap();
        // Switch back to a thinking model: off, not medium.
        session
            .set_model(Model {
                thinking: crate::types::ThinkingKind::Enabled,
                id: "thinking-back".into(),
                ..test_model()
            })
            .await
            .unwrap();
        assert_eq!(
            session.harness.agent().state().thinking_level,
            None,
            "explicit off survives the round trip"
        );
        // The persisted settings preference keeps the wire off.
        let raw = tokio::fs::read_to_string(agent_dir.join("settings.json"))
            .await
            .unwrap();
        let root: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(root["defaultThinkingLevel"], "off");

        // A fork inherits the off preference.
        let _ = session.prompt("branch").await.unwrap();
        let entries = session
            .harness
            .session()
            .storage()
            .get_entries()
            .await
            .unwrap();
        let leaf = entries
            .iter()
            .rev()
            .find(|e| matches!(e, SessionTreeEntry::Message { .. }))
            .map(|e| e.id().to_string())
            .unwrap();
        let mut fork = session
            .fork(&leaf, ForkPosition::AtEntry)
            .await
            .unwrap()
            .session;
        fork.set_model(Model {
            thinking: crate::types::ThinkingKind::None,
            id: "fork-flat".into(),
            ..test_model()
        })
        .await
        .unwrap();
        fork.set_model(Model {
            thinking: crate::types::ThinkingKind::Enabled,
            id: "fork-thinking".into(),
            ..test_model()
        })
        .await
        .unwrap();
        assert_eq!(
            fork.harness.agent().state().thinking_level,
            None,
            "fork inherits the off preference"
        );
    }

    /// A session-append failure propagates: the caller sees an error and the
    /// in-memory tier is not updated (no divergence between runtime and
    /// persisted state).
    #[cfg(unix)]
    #[tokio::test]
    async fn thinking_append_failure_propagates_and_leaves_state_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        let session_dir = dir.path().join("sessions");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(&agent_dir).await.unwrap();
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(&session_dir)
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .with_model(Model {
                thinking: crate::types::ThinkingKind::Enabled,
                ..test_model()
            })
            .build()
            .await
            .unwrap();
        // Materialize the session file so append writes to it directly.
        let _ = session.prompt("seed").await.unwrap();
        let before = session.harness.agent().state().thinking_level.clone();
        // Make the session file read-only so the append write fails.
        use std::os::unix::fs::PermissionsExt as _;
        let path = session.harness.session().storage().path().to_path_buf();
        let original_mode = tokio::fs::metadata(&path)
            .await
            .unwrap()
            .permissions()
            .mode();
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444))
            .await
            .unwrap();
        let result = session.set_thinking_level(Some("high".into())).await;
        // Restore writability for teardown.
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(original_mode))
            .await
            .unwrap();
        assert!(
            result.is_err(),
            "append failure must propagate, got {result:?}"
        );
        assert_eq!(
            session.harness.agent().state().thinking_level,
            before,
            "in-memory tier unchanged on append failure"
        );
    }

    #[tokio::test]
    async fn custom_catalog_restricts_thinking_levels() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        use crate::coding_agent::model_runtime::ModelCatalog;
        struct CappedCatalog;
        impl ModelCatalog for CappedCatalog {
            fn resolve(&self, provider: &str, _model_id: &str) -> Option<Model> {
                (provider == "test").then(test_model)
            }
            fn supported_thinking_levels(&self, _model: &Model) -> Option<Vec<String>> {
                Some(
                    ["off", "medium", "high", "max"]
                        .iter()
                        .map(|l| l.to_string())
                        .collect(),
                )
            }
        }
        let runtime = fake_runtime().with_catalog(Arc::new(CappedCatalog));
        let agent_dir = dir.path().join("agent");
        tokio::fs::create_dir_all(&agent_dir).await.unwrap();
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(&agent_dir)
            .with_model_runtime(runtime)
            .with_model(Model {
                thinking: crate::types::ThinkingKind::Enabled,
                ..test_model()
            })
            .build()
            .await
            .unwrap();
        // Desired xhigh: not supported -> clamps to max (TS up-search).
        session
            .set_thinking_level(Some("xhigh".into()))
            .await
            .unwrap();
        assert_eq!(
            session.harness.agent().state().thinking_level.as_deref(),
            Some("max")
        );
        // minimal is also unsupported -> clamps to the nearest supported.
        session
            .set_thinking_level(Some("minimal".into()))
            .await
            .unwrap();
        assert_eq!(
            session.harness.agent().state().thinking_level.as_deref(),
            Some("medium")
        );
    }

    #[tokio::test]
    async fn undecided_trust_skips_project_scoped_resources() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        tokio::fs::create_dir_all(cwd.join(".pi/skills"))
            .await
            .unwrap();
        tokio::fs::create_dir_all(agent_dir.join("skills"))
            .await
            .unwrap();
        tokio::fs::write(cwd.join(".pi/skills/proj.md"), "project skill")
            .await
            .unwrap();
        tokio::fs::write(agent_dir.join("skills/user.md"), "user skill")
            .await
            .unwrap();
        // No trust decision: the project is treated as untrusted.
        let session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(&agent_dir)
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        let names = session.tools();
        assert!(names.iter().any(|n| n == "bash"), "{names:?}");
        let skill_names: Vec<&str> = session
            .resources()
            .skills
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        assert_eq!(skill_names, vec!["user"], "{skill_names:?}");
    }

    /// A build without any model runtime fails with the typed
    /// missing-credential error when the environment has no keys; the custom
    /// runtime path (covered above) never consults the environment.
    /// A build without credentials fails with the typed missing-credential
    /// error for the default model's provider. Serializes the process-wide
    /// env mutation against other tests.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn build_fails_early_without_credentials() {
        let _guard = crate::coding_agent::model_runtime::tests::TEST_ENV_LOCK
            .lock()
            .unwrap();
        let restore: Vec<(String, Option<String>)> = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]
            .iter()
            .map(|v| {
                let prev = std::env::var(v).ok();
                // SAFETY: single-threaded test scope.
                unsafe { std::env::remove_var(v) };
                (v.to_string(), prev)
            })
            .collect();

        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        // Default model is Anthropic: without ANTHROPIC_API_KEY the build
        // fails with a typed missing-credential error.
        let err = match create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .build()
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("expected a missing-credential error"),
        };
        assert!(err.to_string().contains("missing credential"), "{err}");

        for (v, prev) in restore {
            match prev {
                Some(value) => unsafe { std::env::set_var(v, value) },
                None => unsafe { std::env::remove_var(v) },
            }
        }
    }
}
