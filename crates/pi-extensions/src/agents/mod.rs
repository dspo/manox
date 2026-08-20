// Agent dispatch — the `agent` tool that turns a registered agent
// definition into a running subagent.
//
// Definitions are static manifests (see `pi::ext_point_agent`); this module
// only provides the runtime half: resolving `subagent_type` against the
// registry, mounting the definition's tool subset, and collecting the
// subagent's final text.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pi::coding_agent::{AgentSession, ModelRuntime, create_agent_session};
use pi::ext_point_agent::{AgentDef, AgentRegistry};
use pi::tool::{AgentTool, ToolContext, ToolError};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

/// Default max bytes for the subagent's returned text.
const DEFAULT_MAX_BYTES: usize = 128 * 1024;
/// Default max lines for the subagent's returned text.
const DEFAULT_MAX_LINES: usize = 2000;

/// The built-in Explore definition, embedded as a manifest.
pub fn explore_agent_def() -> AgentDef {
    AgentDef::parse_md(include_str!("../../agents/explore.md"))
        .expect("built-in Explore manifest must parse")
}

/// The built-in Sailor definition, embedded as a manifest. General-purpose
/// coding worker with the full env-backed tool snapshot (read/write/edit +
/// bash). Unlike the read-only Explore, Sailor can modify files and run
/// commands — it is the default dispatch target for parallel
/// implementation/review/build-verification subtasks.
pub fn sailor_agent_def() -> AgentDef {
    AgentDef::parse_md(include_str!("../../agents/sailor.md"))
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
    provider_registry: Option<&Arc<pi::ProviderRegistry>>,
) -> Result<Option<pi::types::Model>, ToolError> {
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
    model: Option<pi::types::Model>,
    /// Optional provider registry for resolving a definition's `model`
    /// override (id or alias). Assembly-layer concern per `AgentDef.model`;
    /// injected by the harness that owns the registry.
    provider_registry: Option<Arc<pi::ProviderRegistry>>,
    /// Host-rendered description override. When set, `description()` returns
    /// this (a live-rendered list of registered subagent types) instead of
    /// the static default — so the model sees the available
    /// `subagent_type` values without probing the filesystem.
    description_override: Option<String>,
}

/// Static fallback for [`SubagentTool::description`] when no host override is
/// injected (e.g. unit tests constructing the tool directly).
const SUBAGENT_TOOL_DEFAULT_DESCRIPTION: &str = "Spawn a subagent from a registered \
    agent definition to handle a focused task in isolation. Returns the subagent's \
    final text.";

impl SubagentTool {
    pub fn new(registry: Arc<AgentRegistry>, tools: Vec<Arc<dyn AgentTool>>) -> Self {
        SubagentTool {
            registry,
            tools,
            model_runtime: None,
            model: None,
            provider_registry: None,
            description_override: None,
        }
    }

    /// Inject the model runtime the subagent session runs on (the caller's
    /// bridge into its own provider configuration).
    pub fn with_model_runtime(mut self, runtime: ModelRuntime) -> Self {
        self.model_runtime = Some(runtime);
        self
    }

    /// Pin the model the subagent session uses (wired to the caller's model).
    pub fn with_model(mut self, model: pi::types::Model) -> Self {
        self.model = Some(model);
        self
    }

    /// Inject the provider registry used to resolve a definition's `model`
    /// override. Without one, a definition declaring a model override fails
    /// loudly at dispatch (resolution is impossible, and silently running
    /// the caller's model would hide the manifest's intent).
    pub fn with_provider_registry(mut self, registry: Arc<pi::ProviderRegistry>) -> Self {
        self.provider_registry = Some(registry);
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
    ) -> Result<(AgentSession, tempfile::TempDir, Option<Worktree>), ToolError> {
        let def = self.registry.get(subagent_type).ok_or_else(|| {
            ToolError::InvalidArguments(format!("unknown subagent_type: {subagent_type}"))
        })?;
        let model_override =
            resolve_model_override(def, subagent_type, self.provider_registry.as_ref())?;
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
        let session_dir =
            tempfile::tempdir().map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;
        let mut builder = create_agent_session()
            .with_cwd(child_cwd.to_path_buf())
            .with_session_dir(session_dir.path())
            .with_system_prompt(def.system_prompt.clone())
            .with_tools(selected);
        if let Some(runtime) = &self.model_runtime {
            builder = builder.with_model_runtime(runtime.clone());
        }
        if let Some(model) = model_override.or_else(|| self.model.clone()) {
            builder = builder.with_model(model);
        }
        let session = builder
            .build()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to start subagent: {e}")))?;
        Ok((session, session_dir, worktree))
    }
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
fn subagent_event_json(event: &pi::types::AgentEvent) -> Option<JsonValue> {
    use pi::types::{AgentEvent, AssistantMessageEvent};
    let inner = match event {
        AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } => match assistant_message_event {
            AssistantMessageEvent::TextDelta { delta, .. } if !delta.is_empty() => {
                serde_json::json!({ "kind": "text", "text": delta })
            }
            AssistantMessageEvent::ThinkingDelta { delta, .. } if !delta.is_empty() => {
                serde_json::json!({ "kind": "thinking", "text": delta })
            }
            _ => return None,
        },
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            arguments,
        } => {
            // A one-field hint (path / command / pattern) keeps the rail
            // activity line informative without shipping full arguments;
            // the child call id lets observers pair start/end under
            // parallel child tool execution.
            let (summary_key, summary) = ["path", "command", "pattern", "query"]
                .iter()
                .find_map(|key| {
                    arguments.get(*key).and_then(|v| v.as_str()).map(|s| {
                        let trimmed = s.trim();
                        let value = if trimmed.len() > 80 {
                            format!("{}…", &trimmed[..trimmed.floor_char_boundary(80)])
                        } else {
                            trimmed.to_string()
                        };
                        ((*key).to_string(), value)
                    })
                })
                .unzip();
            serde_json::json!({
                "kind": "tool_start",
                "id": tool_call_id,
                "tool": tool_name,
                "summary_key": summary_key,
                "summary": summary
            })
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            is_error,
            ..
        } => {
            serde_json::json!({ "kind": "tool_end", "id": tool_call_id, "tool": tool_name, "is_error": is_error })
        }
        _ => return None,
    };
    Some(serde_json::json!({ "subagent_event": inner }))
}

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
fn collect_text(messages: &[pi::types::AgentMessage]) -> String {
    use pi::types::{AgentMessage, ContentBlock};
    for message in messages.iter().rev() {
        let AgentMessage::Assistant { content, .. } = message else {
            continue;
        };
        if content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
        {
            continue;
        }
        let mut out = String::new();
        for block in content {
            if let ContentBlock::Text { text, .. } = block {
                out.push_str(text);
                out.push('\n');
            }
        }
        return out.trim().to_string();
    }
    String::new()
}

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
        let read = pi::tools::read::ReadTool;
        let write = pi::tools::write::WriteTool;
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
    fn select_tools_never_inherits_the_agent_tool() {
        // A subagent must not be able to spawn subagents implicitly.
        let subagent = SubagentTool::new(Arc::new(AgentRegistry::new()), vec![]);
        let tools: Vec<Arc<dyn AgentTool>> =
            vec![Arc::new(pi::tools::read::ReadTool), Arc::new(subagent)];
        let def = AgentDef {
            name: "X".into(),
            description: "d".into(),
            tools: vec![],
            model: None,
            system_prompt: "p".into(),
        };
        let selected = select_tools(&tools, &def);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name(), "Read");
    }

    #[test]
    fn select_tools_empty_means_all() {
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(pi::tools::read::ReadTool)];
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
    #[test]
    fn select_tools_sailor_gets_full_snapshot_explorer_stays_readonly() {
        let tools: Vec<Arc<dyn AgentTool>> = vec![
            Arc::new(pi::tools::read::ReadTool),
            Arc::new(pi::tools::grep::GrepTool),
            Arc::new(pi::tools::glob::GlobTool),
            Arc::new(pi::tools::ls::LsTool),
            Arc::new(pi::tools::bash::BashTool::new(None)),
            Arc::new(pi::tools::write::WriteTool),
            Arc::new(pi::tools::edit::EditTool),
            Arc::new(SubagentTool::new(Arc::new(AgentRegistry::new()), vec![])),
        ];
        let sailor = AgentDef {
            name: "Sailor".into(),
            description: "d".into(),
            tools: vec![],
            model: None,
            system_prompt: "p".into(),
        };
        let sailor_tools = select_tools(&tools, &sailor);
        let sailor_names: Vec<&str> = sailor_tools.iter().map(|t| t.name()).collect();
        assert_eq!(sailor_names.len(), 7, "full snapshot minus Agent");
        assert!(
            sailor_names.contains(&"Bash"),
            "Sailor gets Bash: {sailor_names:?}"
        );
        assert!(
            sailor_names.contains(&"Write"),
            "Sailor gets Write: {sailor_names:?}"
        );
        assert!(
            sailor_names.contains(&"Edit"),
            "Sailor gets Edit: {sailor_names:?}"
        );
        assert!(!sailor_names.contains(&"Agent"), "Agent is never inherited");

        let explore = explore_agent_def();
        let explore_tools = select_tools(&tools, &explore);
        let explore_names: Vec<&str> = explore_tools.iter().map(|t| t.name()).collect();
        assert_eq!(
            explore_names,
            &["Read", "Grep", "Glob", "Ls"],
            "Explore stays read-only despite the expanded snapshot: {explore_names:?}"
        );
    }

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

        let env: Arc<dyn pi::env::ExecutionEnv> =
            Arc::new(pi::env::TokioExecutionEnv::new(repo.to_path_buf()));
        let ctx = pi::tool::LocalToolContext::new(
            env,
            repo.to_path_buf(),
            Arc::new(pi::tool::ToolState::new()),
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

        let env: Arc<dyn pi::env::ExecutionEnv> =
            Arc::new(pi::env::TokioExecutionEnv::new(repo.to_path_buf()));
        let ctx = pi::tool::LocalToolContext::new(
            env,
            repo.to_path_buf(),
            Arc::new(pi::tool::ToolState::new()),
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

    #[test]
    fn subagent_tool_contract() {
        let tool = SubagentTool::new(Arc::new(AgentRegistry::new()), vec![]);
        assert_eq!(tool.name(), "Agent");
        assert!(
            tool.parameters_schema()["required"]
                .as_array()
                .unwrap()
                .len()
                == 2
        );
    }

    /// Minimal `ExecutionEnv` standing in for the harness environment.
    struct MockEnv {
        cwd: PathBuf,
    }

    #[async_trait::async_trait]
    impl pi::env::ExecutionEnv for MockEnv {
        fn cwd(&self) -> &Path {
            &self.cwd
        }
        fn join_path(&self, parts: &[&str]) -> PathBuf {
            parts.iter().collect()
        }
        async fn absolute_path(&self, path: &Path) -> Result<PathBuf, pi::env::FileError> {
            Ok(path.to_path_buf())
        }
        async fn read_file(
            &self,
            _path: &Path,
            _offset: Option<usize>,
            _limit: Option<usize>,
        ) -> Result<String, pi::env::FileError> {
            Ok(String::new())
        }
        async fn write_file(&self, _path: &Path, _content: &str) -> Result<(), pi::env::FileError> {
            Ok(())
        }
        async fn exists(&self, _path: &Path) -> Result<bool, pi::env::FileError> {
            Ok(true)
        }
        async fn file_info(&self, _path: &Path) -> Result<pi::env::FileInfo, pi::env::FileError> {
            Ok(pi::env::FileInfo {
                path: _path.to_path_buf(),
                is_dir: false,
                size: 0,
            })
        }
        async fn list_dir(
            &self,
            _path: &Path,
        ) -> Result<Vec<pi::env::FileInfo>, pi::env::FileError> {
            Ok(vec![])
        }
        async fn create_dir(&self, _path: &Path) -> Result<(), pi::env::FileError> {
            Ok(())
        }
        async fn remove(&self, _path: &Path) -> Result<(), pi::env::FileError> {
            Ok(())
        }
        async fn exec(
            &self,
            _command: &str,
            _timeout: std::time::Duration,
            _signal: CancellationToken,
        ) -> Result<pi::env::CommandResult, pi::env::ExecutionError> {
            Ok(pi::env::CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
            })
        }
    }

    fn ctx(cwd: &str) -> pi::tool::LocalToolContext {
        pi::tool::LocalToolContext::new(
            Arc::new(MockEnv {
                cwd: PathBuf::from(cwd),
            }),
            PathBuf::from(cwd),
            Arc::new(pi::tool::ToolState::new()),
        )
    }

    #[tokio::test]
    async fn unknown_subagent_type_errors_before_spawning() {
        let tool = SubagentTool::new(Arc::new(AgentRegistry::new()), vec![]);
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"subagent_type": "nope", "prompt": "hi"}),
                CancellationToken::new(),
                &ctx("/tmp"),
            )
            .await;
        assert!(result.is_err());
        let err = format!("{}", result.unwrap_err());
        assert!(err.contains("unknown subagent_type: nope"), "got: {err}");
    }

    #[tokio::test]
    async fn declared_model_override_without_registry_is_loud() {
        let mut registry = AgentRegistry::new();
        registry.register(AgentDef {
            name: "M".into(),
            description: "d".into(),
            tools: vec![],
            model: Some("some-model".into()),
            system_prompt: "p".into(),
        });
        let tool = SubagentTool::new(Arc::new(registry), vec![]);
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"subagent_type": "M", "prompt": "hi"}),
                CancellationToken::new(),
                &ctx("/tmp"),
            )
            .await;
        let err = format!("{}", result.unwrap_err());
        assert!(
            err.contains("no provider registry"),
            "refuses loudly instead of silently running the caller's model: {err}"
        );
    }

    #[tokio::test]
    async fn missing_arguments_are_rejected() {
        let tool = SubagentTool::new(Arc::new(AgentRegistry::new()), vec![]);
        let result = tool
            .execute(
                "c1",
                serde_json::json!({}),
                CancellationToken::new(),
                &ctx("/tmp"),
            )
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn collect_text_takes_assistant_text_blocks() {
        use pi::types::{AgentMessage, ContentBlock};
        let messages = vec![
            AgentMessage::User {
                content: vec![ContentBlock::Text {
                    text: "user text".into(),
                    signature: None,
                }],
                timestamp: chrono::Utc::now(),
            },
            AgentMessage::Assistant {
                content: vec![
                    ContentBlock::Text {
                        text: "answer".into(),
                        signature: None,
                    },
                    ContentBlock::Text {
                        text: " part 2".into(),
                        signature: None,
                    },
                ],
                model: "m".into(),
                provider: "p".into(),
                api: "anthropic".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                stop_reason: None,
                raw_stop_reason: None,
                usage: Box::default(),
                error_message: None,
                timestamp: chrono::Utc::now(),
            },
        ];
        assert_eq!(collect_text(&messages), "answer\n part 2");
    }

    #[test]
    fn subagent_event_json_forwards_observer_events_only() {
        use pi::types::{AgentEvent, AssistantMessageEvent};

        // Text delta → forwarded.
        let payload = subagent_event_json(&AgentEvent::MessageUpdate {
            message: Box::new(pi::types::AgentMessage::Assistant {
                content: vec![],
                model: "m".into(),
                provider: "p".into(),
                api: "anthropic".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                stop_reason: None,
                raw_stop_reason: None,
                usage: Box::default(),
                error_message: None,
                timestamp: chrono::Utc::now(),
            }),
            assistant_message_event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "hello".into(),
            },
        })
        .expect("text delta forwarded");
        assert_eq!(payload["subagent_event"]["kind"], "text");
        assert_eq!(payload["subagent_event"]["text"], "hello");

        // Tool start carries the one-field hint.
        let payload = subagent_event_json(&AgentEvent::ToolExecutionStart {
            tool_call_id: "c1".into(),
            tool_name: "Read".into(),
            arguments: serde_json::json!({ "path": "src/main.rs" }),
        })
        .expect("tool start forwarded");
        assert_eq!(payload["subagent_event"]["kind"], "tool_start");
        assert_eq!(payload["subagent_event"]["summary"], "src/main.rs");

        // Tool end carries the error flag.
        let payload = subagent_event_json(&AgentEvent::ToolExecutionEnd {
            tool_call_id: "c1".into(),
            tool_name: "Read".into(),
            result: pi::tool::AgentToolResult::text("ok"),
            is_error: false,
        })
        .expect("tool end forwarded");
        assert_eq!(payload["subagent_event"]["kind"], "tool_end");
        assert_eq!(payload["subagent_event"]["is_error"], false);

        // Lifecycle noise (turn start) is not forwarded.
        assert!(subagent_event_json(&AgentEvent::TurnStart).is_none());
    }

    fn registry_with_model(id: &str) -> Arc<pi::ProviderRegistry> {
        use pi::provider_registry::{Api, Cost, ProviderConfig, ProviderModelConfig};
        let registry = Arc::new(pi::ProviderRegistry::new());
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
                        input: vec![pi::provider_registry::InputModality::Text],
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
}
