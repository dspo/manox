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

use pi::coding_agent::ModelRuntime;
use pi::coding_agent::create_agent_session;
use pi::ext_point_agent::{AgentDef, AgentRegistry};
use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError, ToolProgress};
use pi::tools::truncate::{self, TruncateConfig};
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

#[async_trait::async_trait]
impl AgentTool for SubagentTool {
    fn name(&self) -> &str {
        "Agent"
    }

    fn description(&self) -> &str {
        self.description_override
            .as_deref()
            .unwrap_or(SUBAGENT_TOOL_DEFAULT_DESCRIPTION)
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn requires_approval(&self, _params: &JsonValue) -> bool {
        false
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "subagent_type": {
                    "type": "string",
                    "description": "Name of the registered agent definition to invoke (e.g. Explore)"
                },
                "prompt": {
                    "type": "string",
                    "description": "The task for the subagent"
                },
                "isolation": {
                    "type": "string",
                    "description": "Optional dispatch-time isolation. Set to \"worktree\" to run the sub-agent in its own git worktree on a throwaway branch — full filesystem isolation from the parent's working tree (the child cannot write the parent's project root); a clean worktree is auto-removed when the sub-agent finishes. Omit for same-workspace collaborative work.",
                    "enum": ["worktree"]
                }
            },
            "required": ["subagent_type", "prompt"]
        })
    }

    async fn execute(
        &self,
        tool_call_id: &str,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        self.execute_inner(tool_call_id, params, signal, ctx, None)
            .await
    }

    async fn execute_with_progress(
        &self,
        tool_call_id: &str,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
        progress: &dyn ToolProgress,
    ) -> Result<AgentToolResult, ToolError> {
        self.execute_inner(tool_call_id, params, signal, ctx, Some(progress))
            .await
    }
}

impl SubagentTool {
    async fn execute_inner(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
        progress: Option<&dyn ToolProgress>,
    ) -> Result<AgentToolResult, ToolError> {
        let subagent_type = params["subagent_type"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("subagent_type is required".into()))?;
        let prompt = params["prompt"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("prompt is required".into()))?;
        let def = self.registry.get(subagent_type).ok_or_else(|| {
            ToolError::InvalidArguments(format!("unknown subagent_type: {subagent_type}"))
        })?;
        // A definition's `model` override wins over the caller's pinned
        // model — that is the manifest's intent. Resolution needs the
        // injected provider registry; refuse loudly when the override is
        // declared but cannot be resolved (missing registry or unknown
        // reference), never silently fall back.
        let model_override =
            resolve_model_override(def, subagent_type, self.provider_registry.as_ref())?;

        let selected = select_tools(&self.tools, def);
        // Optional dispatch-time worktree isolation. The subagent session's
        // cwd becomes the worktree path so every tool operates inside it;
        // the worktree is torn down after the session exits.
        let worktree = if params["isolation"].as_str() == Some("worktree") {
            Some(Worktree::prepare(ctx).await?)
        } else {
            None
        };
        let child_cwd = worktree
            .as_ref()
            .map(|w| w.path.as_path())
            .unwrap_or(ctx.cwd());
        // The subagent transcript lives in a throwaway temp directory so a
        // read-only Explore call does not litter the user's project.
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
        // Definition override first, then the caller's pinned model.
        if let Some(model) = model_override.or_else(|| self.model.clone()) {
            builder = builder.with_model(model);
        }
        let mut session = builder
            .build()
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("failed to start subagent: {e}")))?;

        // Live observation: the child session's streaming events are bridged
        // to the parent's ToolProgress channel (the host surfaces them as
        // the Agent tool call's drill-down transcript + rail activity).
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<pi::types::AgentEvent>();
        let _subscription = session.subscribe(Arc::new(move |event, _cancel| {
            let _ = ev_tx.send(event);
            Box::pin(async move {})
        }));
        let forward_child = |event: &pi::types::AgentEvent| {
            if let Some(progress) = progress
                && let Some(payload) = subagent_event_json(event)
            {
                progress.emit(payload);
            }
        };

        // The caller's abort signal must interrupt the subagent mid-run; the
        // inner session runs on its own token, so race it here.
        let sig = signal.clone();
        let mut prompt_fut = Box::pin(session.prompt(prompt));
        let run_outcome: Result<Vec<pi::types::AgentMessage>, ToolError> = loop {
            tokio::select! {
                r = prompt_fut.as_mut() => {
                    break r.map_err(|e| {
                        ToolError::ExecutionFailed(format!("subagent failed: {e}"))
                    });
                }
                Some(event) = ev_rx.recv() => forward_child(&event),
                _ = sig.cancelled() => {
                    drop(prompt_fut);
                    let _ = session.abort();
                    break Err(ToolError::Aborted);
                }
            }
        };
        // Drain child events queued before settlement so the observer sees
        // the whole run.
        while let Ok(event) = ev_rx.try_recv() {
            forward_child(&event);
        }
        // Tear down the worktree (if any) now that the session is done.
        // Best-effort: a cleanup failure never masks the run's outcome.
        if let Some(worktree) = worktree {
            worktree.clean_up(ctx).await;
        }
        let messages = run_outcome?;

        let text = collect_text(&messages);

        let config = TruncateConfig {
            max_bytes: DEFAULT_MAX_BYTES,
            max_lines: DEFAULT_MAX_LINES,
        };
        let truncated = truncate::truncate(&text, &config);
        let mut out = truncated.content;
        if truncated.was_truncated {
            let kept = out.lines().count();
            out.push_str(&format!(
                "\n\n[output truncated: {kept} of {} lines kept, {} bytes]",
                truncated.original_lines, truncated.original_bytes
            ));
        }
        Ok(AgentToolResult::text(out))
    }
}

/// An isolated git worktree a subagent runs in when the caller passes
/// `isolation: "worktree"`. Created at dispatch time on a throwaway branch
/// under the system temp dir; the subagent session's cwd is the worktree
/// path, so every tool operates inside it without manual `cd`. `clean_up`
/// removes the worktree and its branch on session exit, best-effort.
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub repo: PathBuf,
}

impl Worktree {
    pub async fn prepare(ctx: &dyn ToolContext) -> Result<Self, ToolError> {
        let repo = ctx.cwd().to_path_buf();
        let suffix = unique_suffix();
        let branch = format!("sailor-{suffix}");
        let path = std::env::temp_dir().join(format!("manox-sailor-{suffix}"));
        let cmd = format!(
            "git -C {repo_q} worktree add {path_q} -b {branch}",
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
        Ok(Worktree { path, branch, repo })
    }

    pub async fn clean_up(self, ctx: &dyn ToolContext) {
        let rm = format!(
            "git -C {repo_q} worktree remove --force {path_q}",
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
            .exec(&del, Duration::from_secs(30), CancellationToken::new())
            .await;
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// A dependency-free uniqueness suffix for parallel-Sailor branch names:
/// pid + low bits of a nanosecond timestamp. Two Sailors created in the same
/// nanosecond on the same process would collide, which is not realistic.
fn unique_suffix() -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() % 1_000_000_000)
        .unwrap_or(0);
    format!("{pid}{nanos}")
}

/// Double-quote a path for a shell command string. Handles spaces (the
/// common case for project roots and temp dirs).
fn shell_quote(path: &Path) -> String {
    format!("\"{}\"", path.display())
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
/// An empty `tools` list means the full snapshot, minus the `agent` tool
/// itself: a subagent must not inherit the ability to spawn subagents.
pub fn select_tools(tools: &[Arc<dyn AgentTool>], def: &AgentDef) -> Vec<Arc<dyn AgentTool>> {
    let selected: Vec<_> = tools
        .iter()
        .filter(|t| t.name() != "Agent")
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
pub fn collect_text(messages: &[pi::types::AgentMessage]) -> String {
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
        wt.clean_up(&ctx).await;

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
