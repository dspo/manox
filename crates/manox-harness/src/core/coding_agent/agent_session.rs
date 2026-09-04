// AgentSession: the coding-agent facade over an AgentHarness — a session in
// a per-cwd folder, a model runtime, loaded resources, and the mounted tools.
// It composes the harness rather than reimplementing loop/compaction/session:
// `build()` assembles settings / trust / resources / system prompt / tools /
// model runtime (builder overrides win), `open()` restores the session's own
// model / thinking / active tools before returning, and the S4 control surface
// is forwarded straight through. `fork` replaces the session with a
// root→leaf path fork; `close` settles the harness before returning the path.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::Context;
use tokio_util::sync::CancellationToken;

use crate::harness::{AgentHarness, HarnessResources, NavigateTreeOptions};
use crate::session::SessionTreeEntry;
use crate::session::jsonl::JsonlSessionMetadata;
use crate::session::repository::SessionRepository;
use crate::tool::AgentTool;
use crate::tools::bash::BashTool;
use crate::tools::edit::EditTool;
use crate::tools::glob::GlobTool;
use crate::tools::grep::GrepTool;
use crate::tools::ls::LsTool;
use crate::tools::read::ReadTool;
use crate::tools::write::WriteTool;
use crate::trust::{TrustManager, TrustStatus};
use crate::types::{AgentMessage, Model, StopReason};

use super::model_runtime::{ModelCatalog, ModelRuntime};

/// Ceiling for a user-run shell command, matching the bash tool's default.
const BASH_EXECUTION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
use super::resource_loader::ResourceLoader;

/// Per-agent-dir mutex registry so concurrent `patch_global_settings` calls
/// on the same agent dir serialize their read-modify-write. The registry
/// itself is a brief `std::sync::Mutex` over a path map; the per-path lock
/// is a `tokio::sync::Mutex` held across the await points of the patch.
static SETTINGS_LOCKS: OnceLock<std::sync::Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    OnceLock::new();

/// Per-process counter for unique temp-file names (paired with the pid).
static SETTINGS_TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Acquire (get-or-create) the per-path settings mutex. Returns the
/// `Arc`; the caller awaits `arc.lock()`.
fn settings_lock(path: &std::path::Path) -> Arc<tokio::sync::Mutex<()>> {
    let registry = SETTINGS_LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut guard = registry.lock().expect("settings lock registry poisoned");
    if let Some(m) = guard.get(path).cloned() {
        return m;
    }
    let m = Arc::new(tokio::sync::Mutex::new(()));
    guard.insert(path.to_path_buf(), Arc::clone(&m));
    m
}

/// How much of the context window a conversation occupies. `tokens` and
/// `percent` are `None` when the figure is unknowable — see
/// [`AgentSession::context_usage`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextUsage {
    pub tokens: Option<u64>,
    pub context_window: u64,
    pub percent: Option<f64>,
}

/// A live coding session: an open JSONL session plus a harness bound to it.
pub struct AgentSession {
    harness: AgentHarness<crate::session::jsonl::JsonlSessionStorage>,
    session_path: PathBuf,
    session_dir: PathBuf,
    cwd: PathBuf,
    runtime: ModelRuntime,
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
    /// The agent config dir; `set_model`/`set_thinking_level` persist the
    /// default model / thinking level to its global settings file (TS
    /// `setDefaultModelAndProvider`/`setDefaultThinkingLevel`).
    agent_dir: PathBuf,
    /// Prepended to every `execute_bash` command, so a user-run command sees
    /// the same shell setup as the bash tool's.
    shell_command_prefix: Option<String>,
    /// Cancellation tokens of the in-flight `execute_bash` calls; `abort_bash`
    /// cancels them all, killing each command's process tree. Held by `Arc` so
    /// a finishing call can identify and drop its own entry.
    bash_signals: Arc<std::sync::Mutex<Vec<Arc<CancellationToken>>>>,
}

impl AgentSession {
    /// Prompt the agent and return the produced messages. Expands the TS
    /// default prompt forms: `/skill:name args` becomes the skill invocation
    /// block (with `args` appended), and `/name args` expands a prompt
    /// template by name with `substituteArgs`.
    pub async fn prompt(&mut self, text: &str) -> Result<Vec<AgentMessage>, anyhow::Error> {
        self.prompt_with_images(text, Vec::new()).await
    }

    /// Register a hook handler (TS extension `on(event, handler)` parity).
    /// Handlers fire at their [`crate::harness::HookPoint`] for every prompt
    /// this session runs.
    pub fn on(&mut self, hook: crate::harness::HookPoint, handler: crate::harness::HookHandler) {
        self.harness.on(hook, handler);
    }

    /// Prompt with attached images (TS `prompt(text, { images })` parity).
    /// Image blocks ride the prompt's own user message; providers translate
    /// them per wire API.
    pub async fn prompt_with_images(
        &mut self,
        text: &str,
        images: Vec<crate::types::ContentBlock>,
    ) -> Result<Vec<AgentMessage>, anyhow::Error> {
        // TS extension `input` event parity: handlers may transform the
        // input or mark it fully handled (then no turn runs). Fires before
        // skill/template expansion, exactly like the TS session.
        let hook_ctx = self.harness.run_input_hook(text, &images);
        if hook_ctx.input_handled {
            return Ok(Vec::new());
        }
        let (text, images) = match hook_ctx.input_transform {
            Some(transform) => (transform.text, transform.images.unwrap_or(images)),
            None => (text.to_string(), images),
        };
        let expanded = self.expand_prompt(&text);
        // The facade delivers its pending next-turn messages as asides AFTER
        // the prompt's own user message (user-first); the harness's own
        // queue keeps pi-agent-core queued-first semantics.
        let asides = std::mem::take(&mut *self.pending_next_turn.lock().unwrap());
        self.harness
            .prompt_input(crate::harness::PromptInput {
                text: expanded,
                images,
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

    /// Queue a steering message. Returns the queue-local id, which
    /// [`Self::cancel_steer`] accepts to retract it.
    pub fn steer(&self, message: AgentMessage) -> String {
        self.harness.handle().steer(message)
    }

    /// Retract a queued steer by id before the loop drains it.
    pub fn cancel_steer(&self, id: &str) -> bool {
        self.harness.handle().cancel_steer(id)
    }

    /// Queue a follow-up message.
    pub fn follow_up(&self, message: AgentMessage) -> String {
        self.harness.handle().follow_up(message)
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

    /// Run a shell command the user typed and record it in the transcript.
    ///
    /// The command runs against the session cwd through the same execution
    /// environment the bash tool uses, and its result is recorded so the model
    /// sees what the user did to the working tree. `exclude_from_context`
    /// keeps the execution in the session while withholding it from the model.
    ///
    /// Output is truncated tail-first — a shell's verdict is on its last
    /// lines — and the untruncated text spills next to the session file, where
    /// the recorded message points at it.
    pub async fn execute_bash(
        &mut self,
        command: &str,
        exclude_from_context: bool,
    ) -> Result<AgentMessage, anyhow::Error> {
        let signal = Arc::new(CancellationToken::new());
        self.bash_signals.lock().unwrap().push(Arc::clone(&signal));
        let resolved = match &self.shell_command_prefix {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command.to_string(),
        };

        let env = Arc::clone(self.harness.agent().tool_context());
        let outcome = env
            .env()
            .exec(&resolved, BASH_EXECUTION_TIMEOUT, (*signal).clone())
            .await;
        self.bash_signals
            .lock()
            .unwrap()
            .retain(|t| !Arc::ptr_eq(t, &signal));

        let cancelled = signal.is_cancelled();
        let (raw, exit_code) = match outcome {
            Ok(result) => {
                let mut combined = result.stdout;
                if !result.stderr.is_empty() {
                    if !combined.is_empty() && !combined.ends_with('\n') {
                        combined.push('\n');
                    }
                    combined.push_str(&result.stderr);
                }
                let code = (!cancelled).then_some(result.exit_code);
                (combined, code)
            }
            // A cancelled or timed-out command has no status to report; the
            // recorded message says so rather than inventing an exit code.
            // `ExecutionEnv::exec` discards whatever the process had already
            // written, so the output is empty rather than partial.
            Err(crate::env::ExecutionError::Aborted) => (String::new(), None),
            Err(crate::env::ExecutionError::Timeout(_)) => (String::new(), None),
            Err(err) => return Err(err.into()),
        };

        let truncation = crate::tools::truncate::truncate_tail(&raw, &Default::default());
        let full_output_path = if truncation.was_truncated {
            let path = self
                .session_dir
                .join(format!("bash-{}.log", uuid::Uuid::new_v4()));
            match env.env().write_file(&path, &raw).await {
                Ok(()) => Some(path.to_string_lossy().into_owned()),
                // The spill is a convenience pointer; losing it must not lose
                // the execution itself.
                Err(err) => {
                    tracing::warn!("failed to spill full bash output to {path:?}: {err}");
                    None
                }
            }
        } else {
            None
        };

        let message = AgentMessage::BashExecution {
            command: command.to_string(),
            output: truncation.content,
            exit_code,
            cancelled,
            truncated: truncation.was_truncated,
            full_output_path,
            exclude_from_context: exclude_from_context.then_some(true),
            timestamp: chrono::Utc::now(),
        };
        self.append_message(message.clone()).await?;
        Ok(message)
    }

    /// Cancel every in-flight [`AgentSession::execute_bash`], killing each
    /// command's process tree.
    pub fn abort_bash(&self) {
        for signal in self.bash_signals.lock().unwrap().drain(..) {
            signal.cancel();
        }
    }

    /// Whether a [`AgentSession::execute_bash`] is in flight.
    pub fn is_bash_running(&self) -> bool {
        !self.bash_signals.lock().unwrap().is_empty()
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
        // TS checkAuth: preflight the target model's credentials before any
        // mutation. The harness resolves streams through the same resolver at
        // prompt time, so a `MissingCredential` surfaced here is exactly the
        // error the next prompt would hit; failing now leaves the model, the
        // session JSONL, and settings all unchanged.
        (self.runtime.resolver())(&model).context("preflight model credentials")?;

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

        // TS order: update agent (append model_change + in-memory), then
        // append the thinking entry, THEN persist settings LAST. A switch or
        // entry failure cannot leave settings ahead of the session JSONL,
        // which is authoritative on reopen.
        self.harness.set_model(model.clone()).await?;
        // TS `setThinkingLevel`: persist only when the effective level
        // actually changes; the entry write is synchronous and throws on
        // failure — propagate it so the caller knows the state did not land.
        if effective != current_level {
            self.harness
                .session()
                .append_thinking_level_change(&wire)
                .await?;
            if model.thinking != crate::types::ThinkingKind::None || wire != "off" {
                self.settings_thinking_default = Some(wire.clone());
            }
        }
        self.harness.set_initial_thinking_level(effective);

        // Settings last: the declared default (provider/model, and the
        // thinking level when the new model supports it or the level is not
        // off) for future sessions. The switch and thinking entry have
        // already landed, so a write failure is logged rather than failing an
        // otherwise-successful switch.
        let provider = model.provider.clone();
        let model_id = model.id.clone();
        let thinking_default =
            if model.thinking != crate::types::ThinkingKind::None || wire != "off" {
                Some(wire.clone())
            } else {
                None
            };
        if let Err(e) = self
            .patch_global_settings(|obj| {
                obj.insert("defaultProvider".into(), provider.into());
                obj.insert("defaultModel".into(), model_id.into());
                if let Some(d) = thinking_default {
                    obj.insert("defaultThinkingLevel".into(), d.into());
                }
            })
            .await
        {
            tracing::warn!("failed to persist model switch default: {e:#}");
        }
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
        self.apply_thinking(Some(level.unwrap_or_else(|| "off".into())), true)
            .await
    }

    /// Apply a reasoning tier to this session only — the same clamp and
    /// `thinking_level_change` entry as [`Self::set_thinking_level`], minus
    /// the write to the agent's global settings default. A subagent session
    /// applying its pinned effort must not touch the shared
    /// `~/.pi/agent/settings.json`.
    pub async fn set_thinking_level_local(
        &mut self,
        level: Option<String>,
    ) -> Result<(), anyhow::Error> {
        self.apply_thinking(Some(level.unwrap_or_else(|| "off".into())), false)
            .await
    }

    /// Shared reasoning-tier application: clamp the desired level against
    /// the model's capability, append a `thinking_level_change` entry when
    /// it changes, and apply it in memory. `persist` additionally writes the
    /// effective level to the agent's global settings default (the
    /// persisting setter's contract); the session-local path skips that.
    async fn apply_thinking(
        &mut self,
        desired: Option<String>,
        persist: bool,
    ) -> Result<(), anyhow::Error> {
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
            if persist && (model.thinking != crate::types::ThinkingKind::None || wire != "off") {
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
    /// TS merges only the modified fields.
    ///
    /// The whole read-modify-write is serialized per agent dir by a
    /// process-local mutex so two `AgentSession` instances sharing one agent
    /// dir (e.g. two windows) cannot lose updates or race on the temp file.
    /// manox ships single-process, so no cross-process file lock is taken;
    /// concurrent writes from OTHER processes are not guarded.
    async fn patch_global_settings(
        &self,
        patch: impl FnOnce(&mut serde_json::Map<String, serde_json::Value>),
    ) -> Result<(), anyhow::Error> {
        let path = crate::settings::Settings::global_path(&self.agent_dir);
        // Serialize per-path: hold the lock across read, patch, write, rename
        // so a concurrent patch on the same agent dir sees this write's
        // result, not a stale snapshot.
        let lock = settings_lock(&path);
        let _guard = lock.lock().await;
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
        // Unique temp name per call: concurrent patches (even within this
        // process) cannot clobber each other's temp file before the rename.
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "settings.json".into());
        let n = SETTINGS_TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_file_name(format!("{file_name}.tmp.{}.{}", std::process::id(), n));
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

    /// Whether the model is producing a message right now.
    pub fn is_streaming(&self) -> bool {
        self.harness.agent().state().is_streaming
    }

    /// Whether nothing is in flight. The synchronous counterpart to
    /// [`AgentSession::wait_for_idle`], for a caller that needs to ask rather
    /// than wait.
    pub fn is_idle(&self) -> bool {
        self.harness.phase() == crate::harness::AgentHarnessPhase::Idle
            && !self.harness.agent().state().is_streaming
    }

    /// The session's id, as recorded in its file header.
    pub fn session_id(&self) -> &str {
        &self.harness.session().storage().metadata.id
    }

    /// The session's display name, if one was set.
    pub async fn session_name(&self) -> Result<Option<String>, anyhow::Error> {
        self.harness.session().name().await
    }

    /// The queued steering messages, in delivery order.
    pub fn steering_messages(&self) -> Vec<AgentMessage> {
        self.harness.agent().queued_steering_messages()
    }

    /// The queued follow-up messages, in delivery order.
    pub fn follow_up_messages(&self) -> Vec<AgentMessage> {
        self.harness.agent().queued_follow_up_messages()
    }

    /// Empty both queues, returning what was discarded so a caller can put
    /// undelivered input back.
    pub fn clear_queues(&self) -> (Vec<AgentMessage>, Vec<AgentMessage>) {
        self.harness.agent().clear_queues()
    }

    /// Session statistics aggregated over the full active-branch entries —
    /// the TS `getSessionStats`. Assistant and tool-result usage plus
    /// compaction/branch-summary usage all count, so totals reflect what was
    /// actually billed across the session, not just the live context.
    pub async fn session_stats(
        &self,
    ) -> Result<crate::coding_agent::usage::SessionStats, anyhow::Error> {
        let entries = self.harness.session().get_branch().await?;
        Ok(crate::coding_agent::usage::session_stats_from_entries(
            &entries,
        ))
    }

    /// How much of the context window the conversation occupies.
    ///
    /// `None` when the model declares no window. `tokens`/`percent` are `None`
    /// while the figure is unknowable: right after a compaction the newest
    /// assistant usage describes the pre-compaction context, so reporting it
    /// would count the whole summarized history that no longer exists. The
    /// figure becomes known again once an assistant answers past the boundary.
    pub async fn context_usage(&self) -> Result<Option<ContextUsage>, anyhow::Error> {
        let context_window = self.harness.model().context_window as u64;
        if context_window == 0 {
            return Ok(None);
        }

        let branch = self.harness.session().get_branch().await?;
        let latest_compaction = branch
            .iter()
            .rposition(|e| matches!(e, SessionTreeEntry::Compaction { .. }));
        if let Some(boundary) = latest_compaction {
            let trusted = branch[boundary + 1..].iter().any(|e| {
                matches!(
                    e,
                    SessionTreeEntry::Message {
                        message: AgentMessage::Assistant { stop_reason, usage, .. },
                        ..
                    } if !matches!(
                        stop_reason,
                        Some(StopReason::Aborted) | Some(StopReason::Error)
                    ) && crate::compaction::calculate_context_tokens(usage) > 0
                )
            });
            if !trusted {
                return Ok(Some(ContextUsage {
                    tokens: None,
                    context_window,
                    percent: None,
                }));
            }
        }

        let tokens = crate::compaction::estimate_context_tokens(self.harness_messages()).tokens;
        Ok(Some(ContextUsage {
            tokens: Some(tokens),
            context_window,
            percent: Some(tokens as f64 / context_window as f64 * 100.0),
        }))
    }

    /// Subscribe to harness-level events. Delivery stops when the returned
    /// subscription is dropped.
    pub fn subscribe_harness(
        &mut self,
        listener: crate::harness::HarnessListener,
    ) -> crate::harness::HarnessSubscription {
        self.harness.subscribe_harness(listener)
    }

    /// Subscribe to the agent loop's streaming content events (`AgentEvent`).
    ///
    /// Unlike [`Self::subscribe_harness`] (lifecycle only: queues, settle,
    /// abort), this carries the actual transcript traffic — message
    /// start/update/end, tool execution start/update/end, turn boundaries —
    /// which is what a UI consumer needs to render a live session. Listeners
    /// are awaited inline with each emission, so all events for a run are
    /// observed by the time the run settles. Delivery stops when the returned
    /// subscription is dropped.
    pub fn subscribe(&self, listener: crate::agent::AgentListener) -> crate::agent::Subscription {
        self.harness.agent().subscribe(listener)
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
    pub async fn compact(
        &mut self,
        custom_instructions: Option<&str>,
    ) -> Result<(), anyhow::Error> {
        self.harness.compact(custom_instructions).await?;
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

    /// Append a `custom` entry whose payload the harness does not interpret
    /// (host extension data riding the session tree in append order).
    pub async fn append_custom(
        &self,
        custom_type: &str,
        data: Option<serde_json::Value>,
    ) -> Result<String, anyhow::Error> {
        self.harness
            .session()
            .append_custom(custom_type, data)
            .await
    }

    /// Append a typed v4 journal entry by wire kind (§C.2); id, parent and
    /// timestamp are assigned at the session's single append point.
    pub async fn append_typed(
        &self,
        kind: &str,
        payload: serde_json::Value,
    ) -> Result<String, anyhow::Error> {
        self.harness.session().append_typed(kind, payload).await
    }

    /// The compaction-aware active entry list (every entry type, in tree
    /// order) — the display projection source for host UI mirrors.
    pub async fn context_entries(
        &self,
    ) -> Result<Vec<crate::session::SessionTreeEntry>, anyhow::Error> {
        self.harness.session().build_context_entries().await
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

    /// Close the session: request shutdown, settle, and return the path. The
    /// The effective working directory the session path carries — the
    /// header cwd, or the latest `cwd_change` entry when a tool call moved
    /// the sticky cwd — falling back to the assembly cwd when projection
    /// fails.
    pub async fn projected_cwd(&self) -> PathBuf {
        self.harness
            .session()
            .build_session_context()
            .await
            .ok()
            .and_then(|context| context.cwd)
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| self.cwd.clone())
    }

    /// Move the session's working directory (host-driven `SetCwd`): sticky
    /// advance + an immediately durable `cwd_change` entry. See
    /// [`AgentHarness::set_session_cwd`].
    pub async fn set_session_cwd(&mut self, cwd: PathBuf) -> Result<(), anyhow::Error> {
        self.harness.set_session_cwd(cwd).await
    }

    /// JSONL file remains openable and resumable.
    pub async fn close(self) -> Result<PathBuf, anyhow::Error> {
        self.harness.request_shutdown();
        self.harness.wait_for_shutdown().await;
        Ok(self.session_path)
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
    /// Initial active tool subset; `None` when the full mounted set is in
    /// play or the TS default four applies.
    initial_active_tools: Option<Vec<String>>,
    model: Option<Model>,
    system_prompt: Option<String>,
    /// Consumer system-prompt builder; when set, it renders the initial
    /// system prompt and every rebuild, replacing the default fold.
    system_prompt_builder: Option<crate::harness::SystemPromptBuilder>,
    settings: Option<crate::settings::Settings>,
    trust: Option<TrustManager>,
    /// Session id pinned by the caller for a fresh `build()`; `None` draws a
    /// fresh uuid. `open()` ignores it — the file's own id wins.
    session_id: Option<String>,
    /// Free-form session header metadata written by `build()`.
    metadata: Option<serde_json::Value>,
    /// Directory persisting hashline snapshots so a rebuilt store rehydrates
    /// tags across restarts/forks. `None` keeps snapshots memory-only.
    snapshot_dir: Option<PathBuf>,
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

    /// Persist hashline snapshots under `dir` so tags survive a rebuilt store
    /// (app restart, session fork, worktree re-entry).
    pub fn with_snapshot_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.snapshot_dir = Some(dir.into());
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

    /// Install a consumer system-prompt builder. The builder renders the
    /// initial system prompt over the active tools and resources, and the
    /// harness re-invokes it on every active-tool/resource change. When set,
    /// it fully owns the system prompt; a `with_system_prompt` value given
    /// alongside is ignored.
    pub fn with_system_prompt_builder(
        mut self,
        builder: crate::harness::SystemPromptBuilder,
    ) -> Self {
        self.system_prompt_builder = Some(builder);
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

    /// Pin the initial active tool subset the model sees. `None` (default)
    /// falls through to the TS default four (when no custom tools are given)
    /// or the full mounted set (when custom tools are given).
    pub fn with_initial_active_tools(mut self, names: Vec<String>) -> Self {
        self.initial_active_tools = Some(names);
        self
    }

    /// Pin the session id for the session a fresh `build()` creates; the
    /// default is a fresh uuid. A host facade that keeps its own thread id
    /// (manox `ThreadId`) pins it here so the persisted session and the
    /// in-memory thread share one identity — the sidebar keys rows by it.
    /// `open()` ignores this: the file's own header id wins.
    pub fn with_session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    /// Free-form session header metadata written by `build()`; `open()`
    /// ignores it (the file's own header wins).
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = Some(metadata);
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
        // ACTIVE subset is the TS default four, so grep/glob/ls remain
        // enableable later via set_active_tools.
        let tools: Vec<Arc<dyn AgentTool>> = if self.tools.is_empty() {
            vec![
                Arc::new(ReadTool) as Arc<dyn AgentTool>,
                Arc::new(BashTool::new(settings.shell_command_prefix.clone()))
                    as Arc<dyn AgentTool>,
                Arc::new(EditTool::default()) as Arc<dyn AgentTool>,
                Arc::new(WriteTool) as Arc<dyn AgentTool>,
                Arc::new(GrepTool) as Arc<dyn AgentTool>,
                Arc::new(GlobTool) as Arc<dyn AgentTool>,
                Arc::new(LsTool) as Arc<dyn AgentTool>,
            ]
        } else {
            self.tools.clone()
        };
        // The facade's initial active subset: the TS default four when no
        // custom tool list is given. It drives BOTH the initial system
        // prompt and the in-memory harness state, and is the restore default
        // when a reopened path carries no `active_tools_change` entry.
        let facade_initial_active: Option<Vec<String>> =
            self.initial_active_tools.clone().or_else(|| {
                if self.tools.is_empty() {
                    Some(vec![
                        "Read".into(),
                        "Bash".into(),
                        "Edit".into(),
                        "Write".into(),
                    ])
                } else {
                    None
                }
            });
        let active_tools: Vec<String> = facade_initial_active
            .clone()
            .unwrap_or_else(|| tools.iter().map(|t| t.name().to_string()).collect());
        let base_prompt = self
            .system_prompt
            .clone()
            .unwrap_or_else(|| crate::system_prompt::DEFAULT_BASE_PROMPT.into());
        let custom_prompt = self.system_prompt.is_some();
        let system_prompt = match &self.system_prompt_builder {
            Some(builder) => builder(&active_tools, &resources),
            None => crate::system_prompt::build_harness_prompt(
                &base_prompt,
                &cwd,
                &active_tools,
                &resources.context_files,
                &resources.skills,
                custom_prompt,
            ),
        };

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
        if let Some(snapshot_dir) = self.snapshot_dir.clone() {
            harness.set_snapshot_dir(snapshot_dir);
        }
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
        match &self.system_prompt_builder {
            Some(builder) => {
                let builder = builder.clone();
                harness.set_system_prompt_builder(
                    move |active_tools: &[String], resources: &HarnessResources| {
                        builder(active_tools, resources)
                    },
                );
            }
            None => {
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
            }
        }
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
            pending_next_turn: Arc::new(std::sync::Mutex::new(Vec::new())),
            model_fallback_notice: fallback_notice,
            settings_thinking_default,
            agent_dir,
            shell_command_prefix: settings.shell_command_prefix.clone(),
            bash_signals: Arc::new(std::sync::Mutex::new(Vec::new())),
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
                id: self
                    .session_id
                    .clone()
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                cwd: cwd.to_string_lossy().into_owned(),
                created_at: chrono::Utc::now(),
                parent_session_path: None,
                metadata: self.metadata.clone(),
            })
            .await?;
        self.assemble_common(session, cwd, session_dir, false).await
    }

    /// Open an existing session file, restoring its model / thinking /
    /// active tools / effective cwd and its full assembly (settings, trust,
    /// resources, system prompt, tools, runtime) before returning — callers
    /// must not restore manually. Tools and the returned session cwd follow
    /// the session's own project directory.
    pub async fn open(self, path: PathBuf) -> Result<AgentSession, anyhow::Error> {
        let session_dir = path
            .parent()
            .map(std::path::Path::to_path_buf)
            .unwrap_or_default();
        let repo = SessionRepository::new(&session_dir);
        let session = repo.open(&path).await?;
        // The effective cwd is the latest `cwd_change` on the path, falling
        // back to the header (launch) directory. A sticky cwd pointing at a
        // directory that no longer exists (a removed worktree) falls back to
        // the header rather than assembling tools that cannot run anywhere.
        let header_cwd = std::path::PathBuf::from(session.storage().metadata.cwd.clone());
        let cwd = session
            .build_session_context()
            .await
            .ok()
            .and_then(|context| context.cwd)
            .map(std::path::PathBuf::from)
            .filter(|cwd| cwd.is_dir())
            .unwrap_or(header_cwd);
        self.assemble_common(session, cwd, session_dir, true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_loop::StreamFn;
    use crate::coding_agent::create_agent_session;
    use crate::session::SessionStorage;
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
                // Non-zero, so context-usage reporting has an anchor to trust
                // the way a real provider response would give it one.
                usage: Box::new(crate::types::Usage {
                    input_tokens: 100,
                    output_tokens: 10,
                    ..Default::default()
                }),
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
            .find(|t| t.name() == "Read")
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

    #[tokio::test]
    async fn context_usage_is_unknown_until_an_assistant_answers_past_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();

        // A keep-recent window small enough that this short transcript has a
        // cut point at all.
        session.set_compaction_settings(crate::compaction::CompactionSettings {
            keep_recent_tokens: 1,
            ..Default::default()
        });
        for i in 0..4 {
            session.prompt(&format!("turn {i}")).await.unwrap();
        }
        let before = session.context_usage().await.unwrap().unwrap();
        assert!(before.tokens.is_some(), "plain turns report their usage");

        session.compact(None).await.unwrap();
        let after = session.context_usage().await.unwrap().unwrap();
        // The newest assistant usage describes the pre-compaction context, so
        // reporting it would count history that no longer exists.
        assert_eq!(after.tokens, None);
        assert_eq!(after.percent, None);
        assert_eq!(after.context_window, test_model().context_window as u64);

        session.prompt("after compaction").await.unwrap();
        let recovered = session.context_usage().await.unwrap().unwrap();
        assert!(
            recovered.tokens.is_some(),
            "an assistant past the boundary makes the figure knowable again"
        );
        assert!(recovered.percent.unwrap() > 0.0);
    }

    #[tokio::test]
    async fn context_usage_is_none_without_a_context_window() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(Model {
                context_window: 0,
                ..test_model()
            })
            .build()
            .await
            .unwrap();
        assert_eq!(session.context_usage().await.unwrap(), None);
    }

    #[tokio::test]
    async fn queued_messages_are_readable_and_clearable() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();

        session.steer(AgentMessage::user("steer one"));
        session.steer(AgentMessage::user("steer two"));
        session.follow_up(AgentMessage::user("follow one"));
        assert_eq!(session.steering_messages().len(), 2);
        assert_eq!(session.follow_up_messages().len(), 1);

        let (steer, follow_up) = session.clear_queues();
        assert_eq!(steer.len(), 2);
        assert_eq!(follow_up.len(), 1);
        assert!(session.steering_messages().is_empty());
        assert!(session.follow_up_messages().is_empty());
    }

    #[tokio::test]
    async fn idle_and_streaming_predicates_track_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();

        assert!(session.is_idle(), "a fresh session is idle");
        assert!(!session.is_streaming());
        session.prompt("go").await.unwrap();
        session.wait_for_idle().await;
        assert!(session.is_idle(), "settled again after the turn");
        assert!(!session.is_streaming());
    }

    #[tokio::test]
    async fn session_id_and_name_match_what_was_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();

        // The id is the one in the file name the session writes to.
        let id = session.session_id().to_string();
        assert!(!id.is_empty());
        assert!(
            session.path().to_string_lossy().contains(&id),
            "path {:?} carries id {id}",
            session.path()
        );

        assert_eq!(session.session_name().await.unwrap(), None);
        session.set_session_name("my session").await.unwrap();
        assert_eq!(
            session.session_name().await.unwrap().as_deref(),
            Some("my session")
        );
    }

    #[tokio::test]
    async fn execute_bash_records_the_execution_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let sessions = dir.path().join("sessions");

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(&sessions)
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();

        let recorded = session.execute_bash("echo hi", false).await.unwrap();
        match &recorded {
            AgentMessage::BashExecution {
                command,
                output,
                exit_code,
                cancelled,
                ..
            } => {
                assert_eq!(command, "echo hi");
                assert_eq!(output.trim(), "hi");
                assert_eq!(*exit_code, Some(0));
                assert!(!cancelled);
            }
            other => panic!("expected BashExecution, got {other:?}"),
        }
        assert!(!session.is_bash_running());
        // A session file materializes on its first assistant message, so the
        // turn is what puts the already-recorded execution on disk.
        session.prompt("go").await.unwrap();
        let path = session.close().await.unwrap();

        // The execution is part of the transcript a reopened session restores.
        let reopened = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(&sessions)
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .open(path)
            .await
            .unwrap();
        assert!(
            reopened.harness_messages().iter().any(
                |m| matches!(m, AgentMessage::BashExecution { command, .. } if command == "echo hi")
            ),
            "the restored transcript carries the execution"
        );
    }

    #[tokio::test]
    async fn execute_bash_runs_against_the_session_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::write(cwd.join("marker.txt"), "x").await.unwrap();

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();

        let recorded = session.execute_bash("ls marker.txt", false).await.unwrap();
        let AgentMessage::BashExecution {
            output, exit_code, ..
        } = &recorded
        else {
            panic!("expected BashExecution");
        };
        assert_eq!(*exit_code, Some(0), "output: {output}");
        assert!(output.contains("marker.txt"), "{output}");
    }

    #[tokio::test]
    async fn excluded_execution_persists_but_stays_out_of_the_model_context() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();

        session.execute_bash("echo secret", true).await.unwrap();
        // Recorded in the transcript…
        let messages = session.harness_messages();
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, AgentMessage::BashExecution { .. })),
            "the execution is recorded"
        );
        // …and withheld from what the provider would receive.
        let prepared = crate::core::provider::transform::prepare_for_wire(messages);
        let wire = serde_json::to_string(&prepared).unwrap();
        assert!(!wire.contains("secret"), "{wire}");
    }

    #[tokio::test]
    async fn bash_output_over_budget_spills_beside_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("project");
        tokio::fs::create_dir_all(&cwd).await.unwrap();

        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();

        // More lines than the tail budget keeps.
        let recorded = session.execute_bash("seq 1 5000", false).await.unwrap();
        let AgentMessage::BashExecution {
            output,
            truncated,
            full_output_path,
            ..
        } = &recorded
        else {
            panic!("expected BashExecution");
        };
        assert!(truncated, "5000 lines exceed the tail budget");
        // The tail is what survives — a shell's verdict is at the end.
        assert!(output.contains("5000"), "the tail is kept");
        assert!(!output.starts_with("1\n"), "the head is dropped");
        let spill = full_output_path.as_deref().expect("spill path recorded");
        let full = tokio::fs::read_to_string(spill).await.unwrap();
        assert!(full.starts_with("1\n"), "the spill holds the whole output");
        assert!(full.contains("5000"), "the spill holds the tail too");
    }

    /// build → close → open with a WRONG cwd → fork: the session metadata
    /// cwd wins over the caller-provided one, so tools/resources stay in the
    /// session project.
    #[tokio::test]
    async fn reopen_uses_the_session_cwd_over_a_wrong_one() {
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
            .find(|t| t.name() == "Read")
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
    }

    /// The host-driven directory switch: sticky advances, the move is
    /// immediately durable, and the next turn's flush does not duplicate
    /// the entry.
    #[tokio::test]
    async fn set_session_cwd_is_durable_without_flush_duplication() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let work = dir.path().join("work");
        tokio::fs::create_dir_all(&project).await.unwrap();
        tokio::fs::create_dir_all(&work).await.unwrap();
        std::fs::write(work.join("marker.txt"), "w").unwrap();

        let mut session = create_agent_session()
            .with_cwd(&project)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        session.prompt("first").await.unwrap();

        session.set_session_cwd(work.clone()).await.unwrap();
        assert_eq!(
            session.projected_cwd().await,
            work,
            "the switch is immediately durable"
        );

        // A following turn flushes pending mutations — the switch must not
        // produce a second `cwd_change` entry for the same directory.
        session.prompt("second").await.unwrap();
        let cwd_changes = session
            .harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap()
            .iter()
            .filter(|e| matches!(e, SessionTreeEntry::CwdChange { cwd, .. } if cwd.as_str() == work.to_string_lossy()))
            .count();
        assert_eq!(
            cwd_changes, 1,
            "one durable entry per move, not one per turn"
        );

        // Tools resolve in the switched directory without an explicit cwd.
        let read = session
            .harness
            .agent()
            .tools()
            .iter()
            .find(|t| t.name() == "Read")
            .unwrap()
            .clone();
        let ctx = std::sync::Arc::clone(session.harness.agent().tool_context());
        let result = read
            .execute(
                "t1",
                serde_json::json!({ "path": "marker.txt" }),
                tokio_util::sync::CancellationToken::new(),
                &*ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error, "sticky cwd follows the switch");
    }

    /// A durable `cwd_change` tail projects onto a reopen: tools resolve in
    /// the moved-to directory even when the reopen's own cwd is wrong.
    #[tokio::test]
    async fn reopen_projects_the_latest_cwd_change() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let work = dir.path().join("work");
        tokio::fs::create_dir_all(&project).await.unwrap();
        tokio::fs::create_dir_all(&work).await.unwrap();
        tokio::fs::write(work.join("only-in-work.txt"), "work file")
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
        // A tool call moved the sticky cwd into the work directory; the move
        // is durable as a `cwd_change` entry on the session path.
        session
            .harness
            .session()
            .append_cwd_change(work.to_string_lossy().as_ref())
            .await
            .unwrap();
        let path = session.close().await.unwrap();

        // Reopen with a wrong cwd: the projected `cwd_change` wins, so a
        // relative read resolves inside the work directory.
        let elsewhere = dir.path().join("elsewhere");
        tokio::fs::create_dir_all(&elsewhere).await.unwrap();
        let resumed = create_agent_session()
            .with_cwd(&elsewhere)
            .with_model_runtime(fake_runtime())
            .open(path)
            .await
            .unwrap();
        let read = resumed
            .harness
            .agent()
            .tools()
            .iter()
            .find(|t| t.name() == "Read")
            .unwrap()
            .clone();
        let ctx = Arc::clone(resumed.harness.agent().tool_context());
        let result = read
            .execute(
                "t1",
                serde_json::json!({ "path": "only-in-work.txt" }),
                tokio_util::sync::CancellationToken::new(),
                &*ctx,
            )
            .await
            .unwrap();
        assert!(
            !result.is_error,
            "reopen resolves in the projected cwd_change directory"
        );
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
        session.set_active_tools(vec!["Bash".into()]).await.unwrap();
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
            vec!["Read", "Bash", "Edit", "Write"]
        );
        // The system prompt advertises the four, not the seven.
        let prompt = session.harness.agent().state().system_prompt.clone();
        assert!(prompt.contains("- Read: Read a file"));
        assert!(!prompt.contains("- Grep:"), "{prompt}");
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
            vec!["Read", "Bash", "Edit", "Write"],
            "reopen keeps the default four"
        );
        let prompt = resumed.harness.agent().state().system_prompt.clone();
        assert!(!prompt.contains("- Grep:"), "{prompt}");

        // grep stays enableable after the fact.
        let mut resumed = resumed;
        resumed.set_active_tools(vec!["Grep".into()]).await.unwrap();
        assert_eq!(resumed.active_tool_names().unwrap(), vec!["Grep"]);
        let prompt = resumed.harness.agent().state().system_prompt.clone();
        assert!(prompt.contains("- Grep: Search file contents"), "{prompt}");
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

    /// A consumer-installed builder owns the initial prompt and every
    /// rebuild, replacing the kernel default fold.
    #[tokio::test]
    async fn custom_system_prompt_builder_drives_initial_and_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let builder: crate::harness::SystemPromptBuilder =
            Arc::new(|tools: &[String], _resources: &HarnessResources| {
                format!("CUSTOM[{}]", tools.join(","))
            });
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .with_system_prompt_builder(builder)
            .build()
            .await
            .unwrap();
        assert_eq!(
            session.harness.agent().state().system_prompt,
            "CUSTOM[Read,Bash,Edit,Write]"
        );
        session.set_active_tools(vec!["Read".into()]).await.unwrap();
        assert_eq!(
            session.harness.agent().state().system_prompt,
            "CUSTOM[Read]"
        );
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
            .get_entries(Default::default())
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
                .get_entries(Default::default())
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

    /// A corrupt global settings file neither blocks a model switch nor gets
    /// silently wiped: `patch_global_settings` reads it, the parse error
    /// aborts the patch before any write, and the switch (which writes the
    /// session JSONL, not settings) proceeds. The on-disk content is left
    /// intact for the user to repair.
    #[tokio::test]
    async fn corrupt_settings_survive_a_model_switch() {
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
            result.is_ok(),
            "switch proceeds despite corrupt settings: {result:?}"
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

    /// A model whose credentials cannot be resolved is rejected by the
    /// preflight BEFORE any mutation: the harness model, the session JSONL,
    /// and settings.json are all left unchanged.
    #[tokio::test]
    async fn set_model_rejected_by_credential_preflight_leaves_state_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let agent_dir = dir.path().join("agent");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        tokio::fs::create_dir_all(&agent_dir).await.unwrap();
        // A runtime whose resolver errors for the "unauthed" model and serves
        // the rest with the scripted stream.
        use crate::agent_loop::StreamFn;
        let stream: Arc<dyn StreamFn> =
            Arc::new(Scripted(Arc::new(std::sync::Mutex::new(Vec::new()))));
        let ok_stream = Arc::clone(&stream);
        let resolver: crate::agent_loop::StreamResolver = Arc::new(move |model: &Model| {
            if model.id == "unauthed" {
                return Err(anyhow::anyhow!("missing credential for {}", model.id));
            }
            Ok(Arc::clone(&ok_stream))
        });
        struct AuthCatalog;
        impl crate::coding_agent::model_runtime::ModelCatalog for AuthCatalog {
            fn resolve(&self, provider: &str, _id: &str) -> Option<Model> {
                (provider == "test").then(test_model)
            }
        }
        let runtime = ModelRuntime::new(resolver).with_catalog(Arc::new(AuthCatalog));
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(&agent_dir)
            .with_model_runtime(runtime)
            .with_model(test_model())
            .build()
            .await
            .unwrap();
        // Seed a settings file with a sentinel unknown field; it must survive
        // a rejected switch untouched.
        let sentinel = r#"{"defaultModel":"test-1","theme":"dark"}"#;
        tokio::fs::write(agent_dir.join("settings.json"), sentinel)
            .await
            .unwrap();
        let model_before = session.harness.model().id.clone();
        let entries_before = session
            .harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap()
            .len();
        let result = session
            .set_model(Model {
                id: "unauthed".into(),
                ..test_model()
            })
            .await;
        assert!(result.is_err(), "preflight must reject, got {result:?}");
        assert_eq!(
            session.harness.model().id,
            model_before,
            "harness model unchanged on rejected switch"
        );
        let entries_after = session
            .harness
            .session()
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap()
            .len();
        assert_eq!(
            entries_before, entries_after,
            "session JSONL unchanged on rejected switch"
        );
        let raw = tokio::fs::read_to_string(agent_dir.join("settings.json"))
            .await
            .unwrap();
        assert_eq!(raw, sentinel, "settings unchanged on rejected switch");
    }

    /// Two `AgentSession` instances sharing one agent dir can patch different
    /// settings fields concurrently without losing updates: the per-path
    /// mutex serializes the read-modify-write, so both keys land.
    #[tokio::test]
    async fn concurrent_settings_patches_on_shared_agent_dir_do_not_lose_updates() {
        // Run several rounds; the race window is real I/O, so a missing
        // lock would flakily drop a key on at least one round.
        for round in 0..20usize {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().join("proj");
            let agent_dir = dir.path().join("agent");
            tokio::fs::create_dir_all(&cwd).await.unwrap();
            tokio::fs::create_dir_all(&agent_dir).await.unwrap();
            let mut a = create_agent_session()
                .with_cwd(&cwd)
                .with_session_dir(dir.path().join("sessions-a"))
                .with_agent_dir(&agent_dir)
                .with_model_runtime(fake_runtime())
                .with_model(Model {
                    thinking: crate::types::ThinkingKind::Enabled,
                    ..test_model()
                })
                .build()
                .await
                .unwrap();
            let mut b = create_agent_session()
                .with_cwd(&cwd)
                .with_session_dir(dir.path().join("sessions-b"))
                .with_agent_dir(&agent_dir)
                .with_model_runtime(fake_runtime())
                .with_model(Model {
                    thinking: crate::types::ThinkingKind::Enabled,
                    ..test_model()
                })
                .build()
                .await
                .unwrap();
            // A switches the model; B changes the thinking level. Both patch
            // the shared settings file on different keys. A's target model is
            // non-thinking, so its patch writes ONLY defaultProvider/
            // defaultModel — set_model conditionally declares a thinking
            // default too, and with it the final defaultThinkingLevel would
            // depend on patch ORDER (last writer wins), making the assertions
            // below flaky by construction.
            let (ra, rb) = tokio::join!(
                a.set_model(Model {
                    id: format!("beta-{round}"),
                    thinking: crate::types::ThinkingKind::None,
                    ..test_model()
                }),
                b.set_thinking_level(Some("high".into()))
            );
            ra.unwrap();
            rb.unwrap();
            let raw = tokio::fs::read_to_string(agent_dir.join("settings.json"))
                .await
                .unwrap();
            let root: serde_json::Value = serde_json::from_str(&raw).unwrap();
            assert_eq!(
                root["defaultModel"],
                format!("beta-{round}"),
                "round {round}: model patch lost"
            );
            assert_eq!(
                root["defaultThinkingLevel"], "high",
                "round {round}: thinking patch lost"
            );
        }
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
        assert!(names.iter().any(|n| n == "Bash"), "{names:?}");
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

    /// `subscribe` forwards the agent loop's streaming content events — the
    /// UI-facing surface. A plain scripted stream still exercises the
    /// lifecycle backstops (MessageStart/MessageEnd the loop emits when the
    /// stream itself sent none), so the listener must see turn + message
    /// boundaries by the time `prompt` settles.
    #[tokio::test]
    async fn subscribe_forwards_agent_loop_events() {
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

        let seen: Arc<std::sync::Mutex<Vec<&'static str>>> = Arc::default();
        let record = Arc::clone(&seen);
        let _subscription = session.subscribe(Arc::new(move |event, _cancel| {
            let kind = match event {
                AgentEvent::AgentStart => "agent_start",
                AgentEvent::TurnStart => "turn_start",
                AgentEvent::MessageStart { .. } => "message_start",
                AgentEvent::MessageUpdate { .. } => "message_update",
                AgentEvent::MessageEnd { .. } => "message_end",
                AgentEvent::ToolExecutionStart { .. } => "tool_start",
                AgentEvent::ToolExecutionEnd { .. } => "tool_end",
                _ => "other",
            };
            record.lock().unwrap().push(kind);
            Box::pin(async {})
        }));

        session.prompt("hello").await.unwrap();

        let seen = seen.lock().unwrap();
        assert!(seen.contains(&"turn_start"), "saw turn start: {seen:?}");
        assert!(
            seen.contains(&"message_start"),
            "saw message start: {seen:?}"
        );
        assert!(seen.contains(&"message_end"), "saw message end: {seen:?}");
    }

    async fn input_hook_session(dir: &tempfile::TempDir) -> AgentSession {
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_model_runtime(fake_runtime())
            .with_model(test_model())
            .build()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn input_hook_transforms_prompt_text() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = input_hook_session(&dir).await;
        session.on(
            crate::harness::HookPoint::Input,
            std::sync::Arc::new(|mut ctx| {
                let text = ctx.data["text"].as_str().unwrap_or_default().to_string();
                ctx.input_transform = Some(crate::harness::InputHookTransform {
                    text: format!("rewritten: {text}"),
                    images: None,
                });
                ctx
            }),
        );
        session.prompt("original").await.unwrap();
        let crate::types::AgentMessage::User { content, .. } = &session.harness_messages()[0]
        else {
            panic!("first message must be the user prompt");
        };
        assert!(matches!(
            &content[0],
            crate::types::ContentBlock::Text { text, .. } if text == "rewritten: original"
        ));
    }

    #[tokio::test]
    async fn input_hook_handled_skips_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = input_hook_session(&dir).await;
        session.on(
            crate::harness::HookPoint::Input,
            std::sync::Arc::new(|mut ctx| {
                ctx.input_handled = true;
                ctx
            }),
        );
        let messages = session.prompt("never runs").await.unwrap();
        assert!(messages.is_empty(), "handled input runs no turn");
        assert!(
            session.harness_messages().is_empty(),
            "no transcript entry for a handled input"
        );
    }

    #[tokio::test]
    async fn input_hook_transforms_images_and_keeps_original_when_omitted() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = input_hook_session(&dir).await;
        // Replace the attachments with a marker image.
        session.on(
            crate::harness::HookPoint::Input,
            std::sync::Arc::new(|mut ctx| {
                ctx.input_transform = Some(crate::harness::InputHookTransform {
                    text: ctx.data["text"].as_str().unwrap_or_default().to_string(),
                    images: Some(vec![crate::types::ContentBlock::Image {
                        data: "bmV3".to_string(),
                        mime_type: "image/png".to_string(),
                    }]),
                });
                ctx
            }),
        );
        let original = vec![crate::types::ContentBlock::Image {
            data: "b2xk".to_string(),
            mime_type: "image/png".to_string(),
        }];
        session
            .prompt_with_images("with image", original)
            .await
            .unwrap();
        let crate::types::AgentMessage::User { content, .. } = &session.harness_messages()[0]
        else {
            panic!("first message must be the user prompt");
        };
        assert!(matches!(
            content.last(),
            Some(crate::types::ContentBlock::Image { data, .. }) if data == "bmV3"
        ));

        // A transform without `images` keeps the original attachments (TS
        // `images ?? currentImages`).
        let dir2 = tempfile::tempdir().unwrap();
        let mut session = input_hook_session(&dir2).await;
        session.on(
            crate::harness::HookPoint::Input,
            std::sync::Arc::new(|mut ctx| {
                ctx.input_transform = Some(crate::harness::InputHookTransform {
                    text: "text-only rewrite".to_string(),
                    images: None,
                });
                ctx
            }),
        );
        let original = vec![crate::types::ContentBlock::Image {
            data: "a2VwdA==".to_string(),
            mime_type: "image/png".to_string(),
        }];
        session
            .prompt_with_images("keep my image", original)
            .await
            .unwrap();
        let crate::types::AgentMessage::User { content, .. } = &session.harness_messages()[0]
        else {
            panic!("first message must be the user prompt");
        };
        assert!(matches!(
            content.last(),
            Some(crate::types::ContentBlock::Image { data, .. }) if data == "a2VwdA=="
        ));
    }

    #[tokio::test]
    async fn input_hook_continue_is_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = input_hook_session(&dir).await;
        // A hook that only inspects the payload changes nothing.
        session.on(
            crate::harness::HookPoint::Input,
            std::sync::Arc::new(|ctx| {
                assert_eq!(ctx.data["text"], "untouched");
                assert_eq!(ctx.data["source"], "interactive");
                ctx
            }),
        );
        session.prompt("untouched").await.unwrap();
        let crate::types::AgentMessage::User { content, .. } = &session.harness_messages()[0]
        else {
            panic!("first message must be the user prompt");
        };
        assert!(matches!(
            &content[0],
            crate::types::ContentBlock::Text { text, .. } if text == "untouched"
        ));
        assert_eq!(session.harness_messages().len(), 2, "the turn still ran");
    }

    #[tokio::test]
    async fn input_hook_chains_transforms_and_short_circuits_on_handled() {
        // TS runner parity: each handler sees the accumulated transform of
        // all earlier handlers, and a `handled` result stops the chain.
        let dir = tempfile::tempdir().unwrap();
        let mut session = input_hook_session(&dir).await;
        session.on(
            crate::harness::HookPoint::Input,
            std::sync::Arc::new(|mut ctx| {
                let text = ctx.data["text"].as_str().unwrap_or_default().to_string();
                ctx.input_transform = Some(crate::harness::InputHookTransform {
                    text: format!("[one {text}]"),
                    images: None,
                });
                ctx
            }),
        );
        session.on(
            crate::harness::HookPoint::Input,
            std::sync::Arc::new(|mut ctx| {
                let text = ctx.data["text"].as_str().unwrap_or_default().to_string();
                ctx.input_transform = Some(crate::harness::InputHookTransform {
                    text: format!("[two {text}]"),
                    images: None,
                });
                ctx
            }),
        );
        session.prompt("seed").await.unwrap();
        let crate::types::AgentMessage::User { content, .. } = &session.harness_messages()[0]
        else {
            panic!("first message must be the user prompt");
        };
        // Handler two transformed handler one's output, not the original prompt.
        assert!(matches!(
            &content[0],
            crate::types::ContentBlock::Text { text, .. } if text == "[two [one seed]]"
        ));

        // `handled` short-circuits the chain: a later handler never runs.
        let dir2 = tempfile::tempdir().unwrap();
        let mut session = input_hook_session(&dir2).await;
        let later_ran = std::sync::Arc::new(std::sync::Mutex::new(false));
        session.on(
            crate::harness::HookPoint::Input,
            std::sync::Arc::new(|mut ctx| {
                ctx.input_handled = true;
                ctx
            }),
        );
        let later_ran_clone = later_ran.clone();
        session.on(
            crate::harness::HookPoint::Input,
            std::sync::Arc::new(move |ctx| {
                *later_ran_clone.lock().unwrap() = true;
                ctx
            }),
        );
        let messages = session.prompt("never runs").await.unwrap();
        assert!(messages.is_empty(), "handled input runs no turn");
        assert!(!*later_ran.lock().unwrap(), "handled stops the chain");
    }

    fn test_thinking_model() -> Model {
        Model {
            thinking: crate::types::ThinkingKind::Enabled,
            ..test_model()
        }
    }

    /// The local setter clamps like the persisting one but never writes the
    /// agent's global settings: a pinned subagent effort must not leak into
    /// `~/.pi/agent/settings.json`.
    #[tokio::test]
    async fn set_thinking_level_local_applies_without_global_write() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        tokio::fs::create_dir_all(&cwd).await.unwrap();
        let mut session = create_agent_session()
            .with_cwd(&cwd)
            .with_session_dir(dir.path().join("sessions"))
            .with_agent_dir(dir.path().join("agent"))
            .with_model_runtime(fake_runtime())
            .with_model(test_thinking_model())
            .build()
            .await
            .unwrap();
        session
            .set_thinking_level_local(Some("max".into()))
            .await
            .unwrap();
        assert_eq!(session.thinking_level().as_deref(), Some("max"));
        assert!(
            !dir.path().join("agent/settings.json").exists(),
            "the local setter must not write the agent's global settings"
        );
    }

    /// A non-thinking model clamps any pinned effort to off, so the session
    /// level stays unset.
    #[tokio::test]
    async fn set_thinking_level_local_clamps_non_thinking_models() {
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
        session
            .set_thinking_level_local(Some("high".into()))
            .await
            .unwrap();
        assert_eq!(session.thinking_level(), None);
    }
}
