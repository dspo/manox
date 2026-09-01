// Agent dispatch — the `agent` tool that turns a registered agent
// definition into a running subagent.
//
// Definitions are static manifests (see `crate::core::ext_point_agent`); this module
// only provides the runtime half: resolving `subagent_type` against the
// registry, mounting the definition's tool subset, and collecting the
// subagent's final text.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::coding_agent::{AgentSession, ModelRuntime, create_agent_session};
use crate::core::ext_point_agent::{AgentDef, AgentRegistry};
use crate::core::tool::{AgentTool, ToolContext, ToolError};
use tokio_util::sync::CancellationToken;

/// The built-in Explore definition, embedded as a manifest.
pub fn explore_agent_def() -> AgentDef {
    AgentDef::parse_md(include_str!("../../../ext-agents/explore.md"))
        .expect("built-in Explore manifest must parse")
}

/// The built-in Sailor definition, embedded as a manifest. General-purpose
/// coding worker with the full env-backed tool snapshot (read/write/edit +
/// bash). Unlike the read-only Explore, Sailor can modify files and run
/// commands — it is the default dispatch target for parallel
/// implementation/review/build-verification subtasks.
pub fn sailor_agent_def() -> AgentDef {
    AgentDef::parse_md(include_str!("../../../ext-agents/sailor.md"))
        .expect("built-in Sailor manifest must parse")
}

/// Register the built-in agent definitions.
pub fn register_defaults(registry: &mut AgentRegistry) {
    registry.register(explore_agent_def());
    registry.register(sailor_agent_def());
}

/// Resolve a definition's `model` override (assembly-layer concern per
/// `AgentDef.model`): `None` when the definition declares nothing; the
/// resolved model when the injected registry resolves the reference; a loud
/// error when the override is declared but cannot be resolved (missing
/// registry or unknown reference) — never a silent fallback.
fn resolve_model_override(
    def: &AgentDef,
    subagent_type: &str,
    provider_registry: Option<&Arc<crate::core::ProviderRegistry>>,
) -> Result<Option<crate::core::types::Model>, ToolError> {
    Ok(match (def.model.as_deref(), provider_registry) {
        (Some(reference), Some(registry)) => {
            let reference = reference.trim();
            Some(
                crate::model_ref::resolve_model_ref(registry, reference).ok_or_else(|| {
                    ToolError::ExecutionFailed(format!(
                        "agent `{subagent_type}` model override `{reference}` did not resolve"
                    ))
                })?,
            )
        }
        (Some(reference), None) => {
            return Err(ToolError::ExecutionFailed(format!(
                "agent `{subagent_type}` declares model override `{reference}` but no provider registry is available to resolve it"
            )));
        }
        (None, _) => None,
    })
}
/// The `agent` tool — invoke an agent definition as a subagent.
pub struct SubagentTool {
    registry: Arc<AgentRegistry>,
    /// Snapshot of the caller's full tool set, used to resolve `def.tools`.
    tools: Vec<Arc<dyn AgentTool>>,
    /// Optional model runtime; without one the session is built from the
    /// default env-backed runtime.
    model_runtime: Option<ModelRuntime>,
    /// Optional explicit model; without one the session uses its default.
    model: Option<crate::core::types::Model>,
    /// Live view of the owner's current model. Subagents inherit the caller's
    /// model at dispatch time (not the snapshot pinned at assembly) so a
    /// mid-thread model switch is honored instead of falling back to the
    /// provider default.
    model_slot: Option<Arc<std::sync::Mutex<Option<crate::core::types::Model>>>>,
    /// Optional provider registry for resolving a definition's `model`
    /// override (id or alias). Assembly-layer concern per `AgentDef.model`;
    /// injected by the harness that owns the registry.
    provider_registry: Option<Arc<crate::core::ProviderRegistry>>,
    /// Dedicated per-type model specs (`subagent_type` → raw
    /// `provider::model::effort` string) injected by the host from the cx
    /// providers config. A configured entry wins over the definition's
    /// frontmatter override and the caller's inherited model; resolution
    /// happens at dispatch against the injected provider registry.
    model_overrides: HashMap<String, String>,
    /// Host-rendered description override. When set, `description()` returns
    /// this (a live-rendered list of registered subagent types) instead of
    /// the static default — so the model sees the available
    /// `subagent_type` values without probing the filesystem.
    description_override: Option<String>,
    /// Persistent directory for subagent session transcripts. When set,
    /// subagent sessions persist there and survive their run (usage
    /// accounting); unset keeps the throwaway tempdir lifecycle.
    session_dir: Option<PathBuf>,
}

impl SubagentTool {
    pub fn new(registry: Arc<AgentRegistry>, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        SubagentTool {
            registry,
            tools,
            model_runtime: None,
            model: None,
            model_slot: None,
            provider_registry: None,
            model_overrides: HashMap::new(),
            description_override: None,
            session_dir: None,
        }
    }

    /// Inject the model runtime the subagent session runs on (the caller's
    /// bridge into its own provider configuration).
    pub fn with_model_runtime(mut self, runtime: ModelRuntime) -> Self {
        self.model_runtime = Some(runtime);
        self
    }

    /// Pin the model the subagent session uses (wired to the caller's model).
    pub fn with_model(mut self, model: crate::core::types::Model) -> Self {
        self.model = Some(model);
        self
    }

    /// Share the owner's live model slot so dispatch inherits the caller's
    /// current model rather than the assembly-time snapshot.
    pub fn with_model_slot(
        mut self,
        slot: Arc<std::sync::Mutex<Option<crate::core::types::Model>>>,
    ) -> Self {
        self.model_slot = Some(slot);
        self
    }

    /// Inject the provider registry used to resolve a definition's `model`
    /// override. Without one, a definition declaring a model override fails
    /// loudly at dispatch (resolution is impossible, and silently running
    /// the caller's model would hide the manifest's intent).
    pub fn with_provider_registry(mut self, registry: Arc<crate::core::ProviderRegistry>) -> Self {
        self.provider_registry = Some(registry);
        self
    }

    /// Inject dedicated per-type model specs (raw `provider::model::effort`
    /// strings). Resolution happens at dispatch against the injected
    /// provider registry — a spec that cannot be honored fails loudly
    /// instead of silently falling back to the caller's model.
    pub fn with_model_overrides(mut self, overrides: HashMap<String, String>) -> Self {
        self.model_overrides = overrides;
        self
    }

    /// Override the tool description with a host-rendered string. The host
    /// renders the `AgentToolDescription` template against the live
    /// `AgentRegistry` (listing registered `subagent_type` values with their
    /// capability tags) so the model sees what it can dispatch without
    /// probing the filesystem. When unset, the static
    /// [`SUBAGENT_TOOL_DEFAULT_DESCRIPTION`] is returned.
    pub fn with_description(mut self, description: String) -> Self {
        self.description_override = Some(description);
        self
    }

    /// Persistent directory for subagent session transcripts. Without one the
    /// session lives in a throwaway tempdir removed on exit (examples/tests).
    pub fn with_session_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.session_dir = Some(dir.into());
        self
    }
}

impl SubagentTool {
    /// Build a child agent session from a registered definition. The caller
    /// (the host's `AgentBus`) drives `session.prompt(prompt)` and handles
    /// termination; this function only constructs the session.
    /// `extra_tools` are appended after `select_tools` (the host injects
    /// `SteerTool(from=Subagent(addr))` here).
    pub async fn spawn_subagent_session(
        &self,
        subagent_type: &str,
        isolation: Option<&str>,
        ctx: &dyn ToolContext,
        extra_tools: Vec<Arc<dyn AgentTool>>,
        parent_session: Option<&str>,
    ) -> Result<(AgentSession, Option<tempfile::TempDir>, Option<Worktree>), ToolError> {
        let def = self.registry.get(subagent_type).ok_or_else(|| {
            ToolError::InvalidArguments(format!("unknown subagent_type: {subagent_type}"))
        })?;
        // The dedicated config entry is authoritative: resolve it first and
        // skip the definition's frontmatter override entirely when it lands,
        // so a broken frontmatter `model:` cannot shadow a valid config
        // entry. Without a config entry the frontmatter override applies.
        let (configured_model, configured_effort) = match self
            .model_overrides
            .get(subagent_type)
            .map(|s| s.as_str())
        {
            Some(spec) => {
                let registry = self.provider_registry.as_ref().ok_or_else(|| {
                    ToolError::ExecutionFailed(format!(
                        "agent `{subagent_type}` has a dedicated model config `{spec}` but no provider registry is available to resolve it"
                    ))
                })?;
                let resolved = crate::model_ref::resolve_model_spec(registry, spec).map_err(
                    |inner| {
                        ToolError::ExecutionFailed(format!(
                            "agent `{subagent_type}` dedicated model config `{spec}` did not resolve: {inner}"
                        ))
                    },
                )?;
                (Some(resolved.model), resolved.effort)
            }
            None => (None, None),
        };
        let model_override = if configured_model.is_some() {
            None
        } else {
            resolve_model_override(def, subagent_type, self.provider_registry.as_ref())?
        };
        let mut selected = select_tools(&self.tools, def);
        selected.extend(extra_tools);
        let worktree = if isolation == Some("worktree") {
            Some(Worktree::prepare(ctx).await?)
        } else {
            None
        };
        let child_cwd = worktree
            .as_ref()
            .map(|w| w.path.as_path())
            .unwrap_or(ctx.cwd());
        let (session_dir, temp_guard): (PathBuf, Option<tempfile::TempDir>) =
            match &self.session_dir {
                Some(dir) => (dir.clone(), None),
                None => {
                    let guard = tempfile::tempdir()
                        .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;
                    (guard.path().to_path_buf(), Some(guard))
                }
            };
        let mut builder = create_agent_session()
            .with_cwd(child_cwd.to_path_buf())
            .with_session_dir(session_dir)
            .with_system_prompt_builder(crate::prompt::base_prompt_builder(
                def.system_prompt.clone(),
                child_cwd.to_path_buf(),
            ))
            .with_tools(selected);
        if let Some(runtime) = &self.model_runtime {
            builder = builder.with_model_runtime(runtime.clone());
        }
        // Subagent transcripts carry their dispatch lineage in the session
        // header; `parent` stays absent when the caller has no session id.
        let mut subagent_meta = serde_json::json!({ "type": subagent_type });
        if let Some(parent) = parent_session {
            subagent_meta["parent"] = serde_json::json!(parent);
        }
        builder = builder.with_metadata(serde_json::json!({ "subagent": subagent_meta }));
        let inherited = self
            .model_slot
            .as_ref()
            .and_then(|slot| slot.lock().unwrap().clone());
        if let Some(model) = effective_model(
            configured_model,
            model_override,
            inherited,
            self.model.clone(),
        ) {
            builder = builder.with_model(model);
        }
        let mut session = builder
            .build()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to start subagent: {e}")))?;
        if let Some(effort) = configured_effort {
            session
                .set_thinking_level_local(Some(effort))
                .await
                .map_err(|e| {
                    ToolError::ExecutionFailed(format!(
                        "failed to apply dedicated effort for `{subagent_type}`: {e}"
                    ))
                })?;
        }
        Ok((session, temp_guard, worktree))
    }
}

/// The dispatch-time model precedence: a dedicated config spec wins, then
/// the definition's frontmatter override, then the caller's inherited live
/// model, then the assembled explicit model.
fn effective_model(
    configured: Option<crate::core::types::Model>,
    frontmatter: Option<crate::core::types::Model>,
    inherited: Option<crate::core::types::Model>,
    explicit: Option<crate::core::types::Model>,
) -> Option<crate::core::types::Model> {
    configured.or(frontmatter).or(inherited).or(explicit)
}

/// An isolated git worktree a subagent runs in when the caller passes
/// `isolation: "worktree"`. Created at dispatch time on a throwaway branch
/// under the system temp dir; the subagent session's cwd is the worktree
/// path, so every tool operates inside it without manual `cd`. `clean_up`
/// auto-removes a pristine worktree (no commits, no uncommitted changes)
/// and its branch; a worktree with work is kept and its branch + path are
/// reported back so the caller never silently loses edits.
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub repo: PathBuf,
    /// The repo HEAD SHA captured at `prepare` time; `clean_up` compares the
    /// branch tip against it to detect committed work.
    pub base: String,
}

impl Worktree {
    pub async fn prepare(ctx: &dyn ToolContext) -> Result<Self, ToolError> {
        let repo = ctx.cwd().to_path_buf();
        let suffix = unique_suffix();
        let branch = format!("sailor-{suffix}");
        let path = std::env::temp_dir().join(format!("manox-sailor-{suffix}"));
        let cmd = format!(
            "git -C {repo_q} rev-parse --verify HEAD",
            repo_q = shell_quote(&repo),
        );
        let base = ctx
            .env()
            .exec(&cmd, Duration::from_secs(10), CancellationToken::new())
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("git rev-parse HEAD: {e}")))?;
        if base.exit_code != 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "cannot resolve repo HEAD (exit {}): {}",
                base.exit_code,
                base.stderr.trim()
            )));
        }
        let base = base.stdout.trim().to_string();
        let cmd = format!(
            "git -C {repo_q} worktree add {path_q} -b {branch} {base}",
            repo_q = shell_quote(&repo),
            path_q = shell_quote(&path),
        );
        let res = ctx
            .env()
            .exec(&cmd, Duration::from_secs(30), CancellationToken::new())
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("git worktree add: {e}")))?;
        if res.exit_code != 0 {
            return Err(ToolError::ExecutionFailed(format!(
                "git worktree add failed (exit {}): {}",
                res.exit_code,
                res.stderr.trim()
            )));
        }
        Ok(Worktree {
            path,
            branch,
            repo,
            base,
        })
    }

    /// Remove a pristine worktree + branch; keep a worktree with work and
    /// return a message naming the kept branch + path. Best-effort: a git
    /// failure during the pristine check falls back to keeping the worktree
    /// (never destroy work silently).
    pub async fn clean_up(self, ctx: &dyn ToolContext) -> Option<String> {
        if self.is_pristine(ctx).await {
            let rm = format!(
                "git -C {repo_q} worktree remove {path_q}",
                repo_q = shell_quote(&self.repo),
                path_q = shell_quote(&self.path),
            );
            let _ = ctx
                .env()
                .exec(&rm, Duration::from_secs(30), CancellationToken::new())
                .await;
            let del = format!(
                "git -C {repo_q} branch -D {branch}",
                repo_q = shell_quote(&self.repo),
                branch = self.branch,
            );
            let _ = ctx
                .env()
                .exec(&del, Duration::from_secs(10), CancellationToken::new())
                .await;
            let _ = std::fs::remove_dir(&self.path);
            None
        } else {
            Some(format!(
                "[worktree kept: branch={}, path={}]",
                self.branch,
                self.path.display()
            ))
        }
    }

    /// Pristine = no commits beyond the dispatch base AND no uncommitted
    /// changes in the worktree. Either check failing (git error) is treated
    /// as not-pristine so the worktree is kept rather than destroyed.
    async fn is_pristine(&self, ctx: &dyn ToolContext) -> bool {
        let commits = format!(
            "git -C {repo_q} rev-list --count {base}..{branch}",
            repo_q = shell_quote(&self.repo),
            base = self.base.as_str(),
            branch = self.branch.as_str(),
        );
        let res = ctx
            .env()
            .exec(&commits, Duration::from_secs(10), CancellationToken::new())
            .await;
        let committed = res
            .ok()
            .filter(|r| r.exit_code == 0)
            .map(|r| r.stdout.trim().parse::<u64>().unwrap_or(1))
            .unwrap_or(1);
        if committed > 0 {
            return false;
        }
        let dirty = format!(
            "git -C {path_q} status --porcelain",
            path_q = shell_quote(&self.path)
        );
        let res = ctx
            .env()
            .exec(&dirty, Duration::from_secs(10), CancellationToken::new())
            .await;
        res.ok()
            .filter(|r| r.exit_code == 0)
            .map(|r| r.stdout.trim().is_empty())
            .unwrap_or(false)
    }
}

/// A dependency-free uniqueness suffix for parallel-Sailor branch names:
/// pid + the full nanosecond timestamp. An atomic counter guarantees no two
/// dispatches in the same process collide even if the clock stalls.
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}-{}", std::process::id(), nanos, n)
}

/// Single-quote a path for a shell command string with the standard `'\''`
/// escape, so a cwd containing `$`, backticks, `"`, or `'` cannot break or
/// inject into the command.
fn shell_quote(path: &Path) -> String {
    let s = path.display().to_string();
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Map a child-session event to the host-facing progress payload
/// (`{"subagent_event": {...}}`). Only observer-relevant events are
/// forwarded: assistant text/thinking deltas and tool start/end. Everything
/// else returns `None` (no emit).
/// Resolve a definition's tool names against the caller's tool snapshot.
/// An empty `tools` list means the full snapshot, minus the `Steer` tool
/// itself: a subagent must not inherit the parent's full-privilege Steer
/// (the host injects a limited `SteerTool(from=Subagent)` via `extra_tools`).
fn select_tools(tools: &[Arc<dyn AgentTool>], def: &AgentDef) -> Vec<Arc<dyn AgentTool>> {
    let selected: Vec<_> = tools
        .iter()
        .filter(|t| t.name() != "Steer")
        .filter(|t| def.tools.is_empty() || def.tools.iter().any(|n| n == t.name()))
        .cloned()
        .collect();
    if !def.tools.is_empty() {
        for name in &def.tools {
            if !selected.iter().any(|t| t.name() == name) {
                tracing::warn!(
                    agent = %def.name,
                    tool = %name,
                    "agent definition names a tool not in the caller's snapshot"
                );
            }
        }
    }
    selected
}

/// Concatenate the subagent's final answer: the text blocks of the last
/// assistant message that carried no tool calls, skipping intermediate
/// narration and tool-result turns.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explore_manifest_parses() {
        let def = explore_agent_def();
        assert_eq!(def.name, "Explore");
        assert_eq!(def.tools, vec!["Read", "Grep", "Glob", "Ls"]);
        assert!(def.system_prompt.contains("read-only codebase"));
        assert!(def.description.to_lowercase().contains("read-only"));
    }

    #[test]
    fn sailor_manifest_parses() {
        let def = sailor_agent_def();
        assert_eq!(def.name, "Sailor");
        assert!(def.tools.is_empty(), "empty tools means full snapshot");
        assert!(def.model.is_none(), "inherits the Captain's model");
        assert!(
            def.description.to_lowercase().contains("general-purpose"),
            "description advertises the general-purpose role"
        );
        assert!(
            def.system_prompt.contains("concise summary"),
            "system prompt requires a summary on completion"
        );
    }

    #[test]
    fn select_tools_filters_by_definition() {
        let read = crate::core::tools::read::ReadTool;
        let write = crate::core::tools::write::WriteTool;
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(read), Arc::new(write)];
        let def = AgentDef {
            name: "X".into(),
            description: "d".into(),
            tools: vec!["Read".into()],
            model: None,
            system_prompt: "p".into(),
        };
        let selected = select_tools(&tools, &def);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name(), "Read");
    }
    #[test]
    fn select_tools_empty_means_all() {
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(crate::core::tools::read::ReadTool)];
        let def = AgentDef {
            name: "X".into(),
            description: "d".into(),
            tools: vec![],
            model: None,
            system_prompt: "p".into(),
        };
        assert_eq!(select_tools(&tools, &def).len(), 1);
    }

    /// The host snapshot now carries Bash/Write/Edit alongside the read-only
    /// four. A definition with `tools: []` (Sailor) inherits the full set
    /// minus `Agent`; a read-only definition (Explore) names an explicit
    /// allowlist that keeps the write/exec axis out.
    /// A real-git round-trip: `Worktree::prepare` creates a worktree on a
    /// throwaway branch; `clean_up` removes it and the branch. Guards against
    /// the two Step-4 invariants: the child cwd lands in the worktree, and
    /// no worktree lingers after the subagent exits.
    #[tokio::test]
    async fn worktree_prepare_and_clean_up_round_trip() {
        let dir = tempfile::tempdir().expect("temp repo dir");
        let repo = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .expect("git runs")
        };
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"][..],
            &["config", "user.name", "t"][..],
            &["commit", "-q", "-m", "init", "--allow-empty"][..],
        ] {
            assert!(git(args).status.success(), "git {:?} failed", args);
        }

        let env: Arc<dyn crate::core::env::ExecutionEnv> =
            Arc::new(crate::core::env::TokioExecutionEnv::new(repo.to_path_buf()));
        let ctx = crate::core::tool::LocalToolContext::new(
            env,
            repo.to_path_buf(),
            Arc::new(crate::core::tool::ToolState::new()),
        );

        let wt = Worktree::prepare(&ctx).await.expect("worktree prepares");
        assert!(wt.path.is_dir(), "worktree working tree exists");
        assert!(
            wt.path.join(".git").is_file(),
            "worktree has a .git file (linked worktree)"
        );
        let path = wt.path.clone();
        let branch = wt.branch.clone();
        let kept = wt.clean_up(&ctx).await;
        assert!(kept.is_none(), "pristine worktree is removed, not kept");

        assert!(!path.exists(), "worktree removed after clean_up");
        let branches = String::from_utf8_lossy(
            &std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(["branch", "--list"])
                .output()
                .expect("git branch --list")
                .stdout,
        )
        .to_string();
        assert!(
            !branches.contains(&branch),
            "branch {branch} deleted: {branches}"
        );
    }

    /// A worktree with committed work is kept (not destroyed) and its branch +
    /// path are reported back — the B2 invariant: edits never vanish.
    #[tokio::test]
    async fn worktree_with_commits_is_kept_not_destroyed() {
        let dir = tempfile::tempdir().expect("temp repo dir");
        let repo = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(args)
                .output()
                .expect("git runs")
        };
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"][..],
            &["config", "user.name", "t"][..],
            &["commit", "-q", "-m", "init", "--allow-empty"][..],
        ] {
            assert!(git(args).status.success(), "git {:?} failed", args);
        }

        let env: Arc<dyn crate::core::env::ExecutionEnv> =
            Arc::new(crate::core::env::TokioExecutionEnv::new(repo.to_path_buf()));
        let ctx = crate::core::tool::LocalToolContext::new(
            env,
            repo.to_path_buf(),
            Arc::new(crate::core::tool::ToolState::new()),
        );

        let wt = Worktree::prepare(&ctx).await.expect("worktree prepares");
        // Commit on the worktree's branch so it carries work past the base.
        let commit = std::process::Command::new("git")
            .arg("-C")
            .arg(&wt.path)
            .args(["commit", "-q", "-m", "sailor work", "--allow-empty"])
            .output()
            .expect("commit in worktree");
        assert!(commit.status.success(), "commit in worktree");

        let path = wt.path.clone();
        let branch = wt.branch.clone();
        let kept = wt.clean_up(&ctx).await;
        assert!(
            kept.is_some(),
            "a worktree with commits is kept, not removed"
        );
        assert!(
            kept.as_deref().unwrap().contains(&branch),
            "the kept message names the branch: {:?}",
            kept
        );
        assert!(
            path.exists(),
            "the working tree still exists after clean_up"
        );
        // Clean up the kept worktree so the test leaves nothing behind.
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["worktree", "remove", "--force"])
            .arg(&path)
            .output();
        let _ = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["branch", "-D", &branch])
            .output();
    }

    fn registry_with_model(id: &str) -> Arc<crate::core::ProviderRegistry> {
        use crate::core::provider_registry::{Api, Cost, ProviderConfig, ProviderModelConfig};
        let registry = Arc::new(crate::core::ProviderRegistry::new());
        registry
            .register_provider(
                &format!("p-{id}"),
                ProviderConfig {
                    name: Some("P".into()),
                    base_url: Some("https://p.example".into()),
                    api_key: Some("k".into()),
                    api: Some(Api::AnthropicMessages),
                    headers: None,
                    auth_header: false,
                    models: vec![ProviderModelConfig {
                        id: id.into(),
                        name: id.into(),
                        reasoning: false,
                        input: vec![crate::core::provider_registry::InputModality::Text],
                        context_window: 1000,
                        max_tokens: 100,
                        cost: Cost::default(),
                        api: None,
                        base_url: None,
                        metadata: std::collections::HashMap::new(),
                    }],
                },
            )
            .unwrap();
        registry
    }

    fn def_with_model(model: Option<&str>) -> AgentDef {
        AgentDef {
            name: "worker".into(),
            description: "d".into(),
            tools: vec![],
            model: model.map(|m| m.to_string()),
            system_prompt: "sp".into(),
        }
    }

    #[test]
    fn model_override_absent_yields_none() {
        let def = def_with_model(None);
        let registry = registry_with_model("haiku");
        assert!(
            resolve_model_override(&def, "worker", Some(&registry))
                .unwrap()
                .is_none()
        );
        // Also fine without a registry — nothing to resolve.
        assert!(
            resolve_model_override(&def, "worker", None)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn model_override_resolves_against_registry() {
        let def = def_with_model(Some("haiku"));
        let registry = registry_with_model("haiku");
        let resolved = resolve_model_override(&def, "worker", Some(&registry))
            .unwrap()
            .expect("override resolves");
        assert_eq!(resolved.id, "haiku");
    }

    #[test]
    fn model_override_unresolvable_is_loud() {
        let def = def_with_model(Some("no-such-model"));
        let registry = registry_with_model("haiku");
        let err = resolve_model_override(&def, "worker", Some(&registry)).unwrap_err();
        assert!(err.to_string().contains("did not resolve"), "{err}");
    }

    #[test]
    fn model_override_without_registry_is_loud() {
        let def = def_with_model(Some("haiku"));
        let err = resolve_model_override(&def, "worker", None).unwrap_err();
        assert!(err.to_string().contains("no provider registry"), "{err}");
    }

    // ── dedicated per-type model config ──

    fn test_model(id: &str) -> crate::core::types::Model {
        crate::core::types::Model {
            provider: "test".into(),
            api: "test".into(),
            id: id.into(),
            context_window: 100_000,
            max_tokens: 8_192,
            thinking: crate::core::types::ThinkingKind::Enabled,
            metadata: Default::default(),
        }
    }

    struct StaticStream;

    #[async_trait::async_trait]
    impl crate::core::agent_loop::StreamFn for StaticStream {
        async fn stream(
            &self,
            _context: &crate::core::types::AgentContext,
            _signal: tokio_util::sync::CancellationToken,
            _event_tx: tokio::sync::mpsc::Sender<crate::core::types::AgentEvent>,
        ) -> Result<crate::core::types::AgentMessage, anyhow::Error> {
            Ok(crate::core::types::AgentMessage::Assistant {
                content: vec![crate::core::types::ContentBlock::Text {
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
                stop_reason: Some(crate::core::types::StopReason::Stop),
                usage: Box::new(crate::core::types::Usage::default()),
                error_message: None,
                timestamp: chrono::Utc::now(),
            })
        }
    }

    fn fake_runtime() -> ModelRuntime {
        let resolver: crate::core::agent_loop::StreamResolver =
            Arc::new(|_m: &crate::core::types::Model| {
                Ok(Arc::new(StaticStream) as Arc<dyn crate::core::agent_loop::StreamFn>)
            });
        ModelRuntime::new(resolver)
    }

    fn spawn_ctx(cwd: &std::path::Path) -> crate::core::tool::LocalToolContext {
        crate::core::tool::LocalToolContext::new(
            Arc::new(crate::core::env::TokioExecutionEnv::new(cwd.to_path_buf())),
            cwd.to_path_buf(),
            Arc::new(crate::core::tool::ToolState::new()),
        )
    }

    #[test]
    fn effective_model_precedence() {
        let m = |id| Some(test_model(id));
        // The dedicated config spec wins, then frontmatter, then inherited,
        // then the explicit assembled model.
        assert_eq!(
            effective_model(m("cfg"), m("fm"), m("inh"), m("exp"))
                .unwrap()
                .id,
            "cfg"
        );
        assert_eq!(
            effective_model(None, m("fm"), m("inh"), m("exp"))
                .unwrap()
                .id,
            "fm"
        );
        assert_eq!(
            effective_model(None, None, m("inh"), m("exp")).unwrap().id,
            "inh"
        );
        assert_eq!(
            effective_model(None, None, None, m("exp")).unwrap().id,
            "exp"
        );
        assert!(effective_model(None, None, None, None).is_none());
    }

    #[tokio::test]
    async fn configured_spec_wins_and_applies_effort() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = spawn_ctx(dir.path());
        let registry = registry_with_model("glm-5.3");
        let mut agent_registry = AgentRegistry::new();
        // Frontmatter pins the same model; the inherited slot pins another.
        agent_registry.register(def_with_model(Some("glm-5.3")));
        let tool = SubagentTool::new(Arc::new(agent_registry), vec![])
            .with_model_runtime(fake_runtime())
            .with_provider_registry(registry)
            .with_model_slot(Arc::new(std::sync::Mutex::new(Some(test_model(
                "inherited",
            )))))
            .with_model_overrides(HashMap::from([(
                "worker".to_string(),
                "P::glm-5.3::high".to_string(),
            )]));
        let (session, _guard, _worktree) = tool
            .spawn_subagent_session("worker", None, &ctx, vec![], None)
            .await
            .unwrap();
        // The configured spec pins the model over the frontmatter override
        // and the inherited slot...
        assert_eq!(session.model().id, "glm-5.3");
        assert_eq!(session.model().provider, "p-glm-5.3");
        // ...and applies its effort as the session thinking level.
        assert_eq!(session.thinking_level().as_deref(), Some("high"));
    }
    #[tokio::test]
    async fn unresolvable_configured_spec_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = spawn_ctx(dir.path());
        let registry = registry_with_model("glm-5.3");
        let mut agent_registry = AgentRegistry::new();
        agent_registry.register(def_with_model(None));
        let tool = SubagentTool::new(Arc::new(agent_registry), vec![])
            .with_model_runtime(fake_runtime())
            .with_provider_registry(registry)
            .with_model_overrides(HashMap::from([(
                "worker".to_string(),
                "P::no-such::high".to_string(),
            )]));
        let err = match tool
            .spawn_subagent_session("worker", None, &ctx, vec![], None)
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("an unresolvable dedicated spec must fail the dispatch"),
        };
        assert!(
            err.to_string()
                .contains("dedicated model config `P::no-such::high` did not resolve"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn configured_spec_without_registry_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = spawn_ctx(dir.path());
        let mut agent_registry = AgentRegistry::new();
        agent_registry.register(def_with_model(None));
        let tool = SubagentTool::new(Arc::new(agent_registry), vec![]).with_model_overrides(
            HashMap::from([("worker".to_string(), "P::glm-5.3::high".to_string())]),
        );
        let err = match tool
            .spawn_subagent_session("worker", None, &ctx, vec![], None)
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("a dedicated spec without a registry must fail the dispatch"),
        };
        assert!(
            err.to_string()
                .contains("no provider registry is available to resolve it"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn valid_configured_spec_rescues_broken_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = spawn_ctx(dir.path());
        let registry = registry_with_model("glm-5.3");
        let mut agent_registry = AgentRegistry::new();
        // A broken frontmatter override must not shadow a valid config entry.
        agent_registry.register(def_with_model(Some("no-such-model")));
        let tool = SubagentTool::new(Arc::new(agent_registry), vec![])
            .with_model_runtime(fake_runtime())
            .with_provider_registry(registry)
            .with_model_overrides(HashMap::from([(
                "worker".to_string(),
                "P::glm-5.3::high".to_string(),
            )]));
        let (session, _guard, _worktree) = tool
            .spawn_subagent_session("worker", None, &ctx, vec![], None)
            .await
            .unwrap();
        assert_eq!(session.model().id, "glm-5.3");
        assert_eq!(session.model().provider, "p-glm-5.3");
    }
}
