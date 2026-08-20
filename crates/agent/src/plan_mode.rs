//! Plan mode: structured plan proposal, research discipline, and write gating.
//!
//! The model researches read-only, writes the plan to a session-local file
//! (`~/.manox/plans/<slug>-plan.md`), and submits it for the user's verdict
//! through the [`ProposePlanTool`] — a structured tool call, not free-text
//! parsing. While plan mode is active a `ToolCall` hook hard-blocks mutating
//! tools (plan-file writes excepted, approval-free) and a `BeforeAgentStart`
//! hook injects the plan-mode instructions every turn. All wiring rides the
//! kernel's existing extension points; `crates/pi` stays untouched.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};

use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::thread::ThreadEvent;
use crate::thread_engine::BackendNotice;
/// Resolves whether a subagent type is read-only (eligible to dispatch under
/// plan mode). Built by the host from the live `AgentRegistry` so the gate
/// shares the `SailorRoutingTool`'s capability routing — write/bash
/// subagents (`Sailor`) stay blocked.
pub type ReadOnlySubagentResolver = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Tool name the model calls to submit a plan for review.
pub const PROPOSE_PLAN: &str = "ProposePlan";

/// Tools that stay available while plan mode is active. Read-only research
/// tools plus the interaction/proposal devices; everything else is blocked
/// (the working tree must stay untouched while planning). The `Agent` tool
/// is conditionally allowed for read-only subagents (e.g. `Explore`) — see
/// the gate's dispatch check; write/bash subagents (`Sailor`) and worktree
/// isolation stay blocked.
const PLAN_MODE_ALLOWED_TOOLS: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "Ls",
    "BashOutput",
    crate::tools::ASK_USER_QUESTION,
    PROPOSE_PLAN,
];

/// Tools allowed only when the target path is inside the plans dir.
const PLAN_MODE_PATH_GATED_TOOLS: &[&str] = &["Write", "Edit"];

/// Shared plan-mode state for one session: the on/off flag, the last
/// proposed plan file, and the rendered plan-mode instructions injected
/// every turn while active. The actor, the hooks, and the gate all read
/// through this single point.
#[derive(Debug, Default)]
pub struct PlanSessionState {
    inner: RwLock<PlanStateInner>,
}

#[derive(Debug, Default, Clone)]
struct PlanStateInner {
    enabled: bool,
    plan_file: Option<String>,
    active_instructions: Option<String>,
}

impl PlanSessionState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn enabled(&self) -> bool {
        self.inner.read().unwrap().enabled
    }

    pub fn plan_file(&self) -> Option<String> {
        self.inner.read().unwrap().plan_file.clone()
    }

    /// Replace the full state (enter/exit plan mode).
    pub fn set(&self, enabled: bool, plan_file: Option<String>) {
        let mut inner = self.inner.write().unwrap();
        inner.enabled = enabled;
        inner.plan_file = plan_file;
        if !enabled {
            inner.active_instructions = None;
        }
    }

    pub fn set_plan_file(&self, plan_file: Option<String>) {
        self.inner.write().unwrap().plan_file = plan_file;
    }

    pub fn active_instructions(&self) -> Option<String> {
        self.inner.read().unwrap().active_instructions.clone()
    }

    pub fn set_active_instructions(&self, text: Option<String>) {
        self.inner.write().unwrap().active_instructions = text;
    }
}

/// Validate a plan slug: letters, digits, underscores, hyphens only — the
/// slug becomes a filename, so no separators or dot segments.
pub fn validate_slug(slug: &str) -> Result<(), String> {
    if slug.is_empty() {
        return Err("slug must not be empty".to_string());
    }
    if slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Ok(())
    } else {
        Err(format!(
            "slug {slug:?} may contain only letters, numbers, underscores, and hyphens"
        ))
    }
}

/// The plan file for a slug under the plans dir.
pub fn plan_file_path(plans_dir: &Path, slug: &str) -> PathBuf {
    plans_dir.join(format!("{slug}-plan.md"))
}

/// Title resolution chain: supplied title → first markdown heading in the
/// plan file → slug fallback (mirrors oh-my-pi's `resolveApprovedPlan`).
pub fn resolve_plan_title(supplied: Option<&str>, content: &str, slug: &str) -> String {
    if let Some(title) = supplied.map(str::trim).filter(|t| !t.is_empty()) {
        return title.to_string();
    }
    for line in content.lines() {
        let trimmed = line.trim().trim_start_matches('#').trim();
        if line.trim_start().starts_with('#') && !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    slug.to_string()
}

/// Lexical path normalization (no filesystem access — plan files may not
/// exist yet): resolves `.`/`..` components so a containment check cannot be
/// fooled by `../` traversal.
fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Symlink-aware resolution of a (possibly not-yet-existing) path:
/// canonicalize the longest existing ancestor, then re-append the remaining
/// tail lexically. A symlinked directory inside (or escaping) the plans dir
/// therefore resolves to its real location before containment is checked.
fn resolve_with_existing_prefix(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if existing.symlink_metadata().is_ok() {
            break;
        }
        match existing.file_name() {
            Some(name) => {
                tail.push(name.to_os_string());
                if !existing.pop() {
                    break;
                }
            }
            None => break,
        }
    }
    let mut resolved = existing.canonicalize().unwrap_or(existing);
    for part in tail.into_iter().rev() {
        resolved.push(part);
    }
    normalize_lexical(&resolved)
}

/// True when `params.path` (Write/Edit) targets a file inside `plans_dir`,
/// resolved against the session cwd for relative paths. Containment is
/// checked symlink-aware: the longest existing prefix is canonicalized, so a
/// symlinked directory cannot smuggle a write out of (or into) the plans
/// dir; the not-yet-created tail (the plan file itself) is appended
/// lexically after resolution.
pub fn is_plan_dir_path_param(params: &serde_json::Value, plans_dir: &Path, cwd: &Path) -> bool {
    let Some(raw) = params.get("path").and_then(|v| v.as_str()) else {
        return false;
    };
    let candidate = Path::new(raw);
    let absolute = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    // Both sides resolve through the same symlink-aware path so a shared
    // existing prefix (e.g. `/home`) canonicalizes identically on each.
    let normalized = resolve_with_existing_prefix(&absolute);
    let root = resolve_with_existing_prefix(plans_dir);
    normalized == root || normalized.starts_with(&root)
}

/// Approval-gate policy input: while plan mode is active, Write/Edit calls
/// targeting the plans dir bypass the approval gate (the model drafts the
/// plan incrementally and must not be interrupted by an approval card per edit).
pub struct PlanGatePolicy {
    pub state: Arc<PlanSessionState>,
    pub plans_dir: PathBuf,
    pub cwd: PathBuf,
}

impl PlanGatePolicy {
    /// True when this call is a plan-file write during plan mode and the
    /// approval gate should stand down.
    pub fn is_exempt(&self, tool_name: &str, params: &serde_json::Value) -> bool {
        self.state.enabled()
            && PLAN_MODE_PATH_GATED_TOOLS.contains(&tool_name)
            && is_plan_dir_path_param(params, &self.plans_dir, &self.cwd)
    }
}

/// Whether an `Agent` tool call targets a read-only subagent eligible to run
/// under plan mode. Requires an explicit `subagent_type` that the resolver
/// resolves read-only, and rejects `isolation: "worktree"` (materializing a
/// working tree is a working-tree write, blocked under plan mode's read-only
/// guarantee).
fn is_read_only_subagent_dispatch(
    args: &serde_json::Value,
    is_read_only_subagent: &ReadOnlySubagentResolver,
) -> bool {
    let Some(subagent_type) = args.get("subagent_type").and_then(|v| v.as_str()) else {
        return false;
    };
    if args.get("isolation").and_then(|v| v.as_str()) == Some("worktree") {
        return false;
    }
    is_read_only_subagent(subagent_type)
}

/// The `ToolCall` hook enforcing plan mode's read-only guarantee: research
/// tools and the proposal devices pass; Write/Edit pass only for plan-file
/// targets; an `Agent` call passes only for a read-only subagent
/// (e.g. `Explore`) without worktree isolation; every other tool (Bash,
/// Monitor, write/bash sub-agents, MCP, …) is blocked with a reason the
/// model can act on.
pub fn gate_handler(
    state: Arc<PlanSessionState>,
    plans_dir: PathBuf,
    cwd: PathBuf,
    is_read_only_subagent: ReadOnlySubagentResolver,
) -> pi::harness::HookHandler {
    Arc::new(move |mut ctx| {
        if !state.enabled() {
            return ctx;
        }
        let tool_name = ctx
            .data
            .get("tool_name")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let allowed = PLAN_MODE_ALLOWED_TOOLS.contains(&tool_name)
            || (PLAN_MODE_PATH_GATED_TOOLS.contains(&tool_name)
                && is_plan_dir_path_param(&ctx.data["args"], &plans_dir, &cwd))
            || (tool_name == crate::tools::AGENT
                && is_read_only_subagent_dispatch(&ctx.data["args"], &is_read_only_subagent));
        if !allowed {
            ctx.block_reason = Some(format!(
                "Plan mode is active: the working tree is read-only while planning. \
                 Only the plan file under {} may be written (Write/Edit); research with \
                 Read/Grep/Glob/Ls or a read-only `Explore` subagent (no worktree isolation), \
                 ask with AskUserQuestion, and submit the plan with {PROPOSE_PLAN}.",
                plans_dir.display()
            ));
        }
        ctx
    })
}

/// The `BeforeAgentStart` hook injecting the rendered plan-mode instructions
/// as a user message on every turn while plan mode is active.
pub fn injection_handler(state: Arc<PlanSessionState>) -> pi::harness::HookHandler {
    Arc::new(move |ctx| {
        let Some(instructions) = state.active_instructions() else {
            return ctx;
        };
        if !state.enabled() {
            return ctx;
        }
        ctx.with_inject_messages(vec![pi::types::AgentMessage::user(instructions)])
    })
}

/// The `ProposePlan` tool: the model's only plan-approval channel. Validates
/// the plan file exists, resolves the title, records the plan file, and
/// surfaces `ThreadEvent::PlanReady` for the workspace review card.
pub struct ProposePlanTool {
    notice_tx: mpsc::UnboundedSender<BackendNotice>,
    state: Arc<PlanSessionState>,
    plans_dir: PathBuf,
}

impl ProposePlanTool {
    pub fn new(
        notice_tx: mpsc::UnboundedSender<BackendNotice>,
        state: Arc<PlanSessionState>,
        plans_dir: PathBuf,
    ) -> Self {
        Self {
            notice_tx,
            state,
            plans_dir,
        }
    }
}

#[async_trait::async_trait]
impl AgentTool for ProposePlanTool {
    fn name(&self) -> &str {
        PROPOSE_PLAN
    }

    fn description(&self) -> &str {
        "Submit the finished plan for the user's approval verdict. Call only \
         after the plan file is complete and decision-complete: pass the \
         <slug> of your <slug>-plan.md in the plans directory (and optionally \
         a title). The user then chooses an execution option; do not start \
         implementing before approval. Never use this tool to ask questions — \
         use AskUserQuestion for that."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "slug": {
                    "type": "string",
                    "description": "Slug of the plan file (<slug>-plan.md) in the plans directory"
                },
                "title": {
                    "type": "string",
                    "description": "Optional short plan title; defaults to the plan's first heading"
                }
            },
            "required": ["slug"]
        })
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let slug = params
            .get("slug")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim()
            .to_string();
        if let Err(err) = validate_slug(&slug) {
            return Err(ToolError::InvalidArguments(err));
        }
        let title_supplied = params
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let path = plan_file_path(&self.plans_dir, &slug);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(_) => {
                return Err(ToolError::ExecutionFailed(format!(
                    "plan file not found at {} — write the plan to that file first, then call \
                     {PROPOSE_PLAN} again with the same slug",
                    path.display()
                )));
            }
        };
        if content.trim().is_empty() {
            return Err(ToolError::ExecutionFailed(format!(
                "plan file {} is empty — fill in the plan before proposing it",
                path.display()
            )));
        }
        let title = resolve_plan_title(title_supplied.as_deref(), &content, &slug);
        let plan_file = path.to_string_lossy().to_string();
        self.state.set_plan_file(Some(plan_file.clone()));
        let _ = self
            .notice_tx
            .send(BackendNotice::Event(Box::new(ThreadEvent::PlanReady {
                plan_file: plan_file.clone(),
                title: title.clone(),
            })));
        Ok(AgentToolResult {
            content: vec![pi::types::ContentBlock::Text {
                text: format!(
                    "Plan submitted for review: {title} ({plan_file}). The turn ends here; wait for the user's verdict and do not implement before approval."
                ),
                signature: None,
            }],
            details: Some(serde_json::json!({
                "plan_file": plan_file,
                "title": title,
            })),
            is_error: false,
            usage: None,
            added_tool_names: None,
            // End the turn at the proposal so the review card stays the
            // conversation's last item; without this the model appends a
            // trailing summary after the card.
            terminate: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NullEnv;

    #[async_trait::async_trait]
    impl pi::env::ExecutionEnv for NullEnv {
        fn cwd(&self) -> &std::path::Path {
            std::path::Path::new("/")
        }
        fn join_path(&self, parts: &[&str]) -> std::path::PathBuf {
            parts.iter().collect()
        }
        async fn absolute_path(
            &self,
            path: &std::path::Path,
        ) -> Result<std::path::PathBuf, pi::env::FileError> {
            Ok(path.to_path_buf())
        }
        async fn read_file(
            &self,
            _path: &std::path::Path,
            _offset: Option<usize>,
            _limit: Option<usize>,
        ) -> Result<String, pi::env::FileError> {
            Ok(String::new())
        }
        async fn write_file(
            &self,
            _path: &std::path::Path,
            _content: &str,
        ) -> Result<(), pi::env::FileError> {
            Ok(())
        }
        async fn exists(&self, _path: &std::path::Path) -> Result<bool, pi::env::FileError> {
            Ok(false)
        }
        async fn file_info(
            &self,
            path: &std::path::Path,
        ) -> Result<pi::env::FileInfo, pi::env::FileError> {
            Ok(pi::env::FileInfo {
                path: path.to_path_buf(),
                is_dir: false,
                size: 0,
            })
        }
        async fn list_dir(
            &self,
            _path: &std::path::Path,
        ) -> Result<Vec<pi::env::FileInfo>, pi::env::FileError> {
            Ok(Vec::new())
        }
        async fn create_dir(&self, _path: &std::path::Path) -> Result<(), pi::env::FileError> {
            Ok(())
        }
        async fn remove(&self, _path: &std::path::Path) -> Result<(), pi::env::FileError> {
            Ok(())
        }
        async fn exec(
            &self,
            _command: &str,
            _timeout: std::time::Duration,
            _signal: tokio_util::sync::CancellationToken,
        ) -> Result<pi::env::CommandResult, pi::env::ExecutionError> {
            Err(pi::env::ExecutionError::Other("null env".into()))
        }
    }

    // The agent loop stops a turn only when every finalized tool call reports
    // `terminate`; this locks the field so the review card stays the
    // conversation's last item (a silent revert to `false` would let the model
    // append a trailing summary after the card).
    #[tokio::test]
    async fn propose_plan_ends_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("audit-plan.md"), "# Audit\n\nbody\n").unwrap();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tool = ProposePlanTool::new(tx, PlanSessionState::new(), dir.path().to_path_buf());
        let ctx = pi::tool::LocalToolContext::new(
            std::sync::Arc::new(NullEnv),
            dir.path().to_path_buf(),
            std::sync::Arc::new(pi::tool::ToolState::new()),
        );
        let result = tool
            .execute(
                "call",
                serde_json::json!({ "slug": "audit" }),
                tokio_util::sync::CancellationToken::new(),
                &ctx,
            )
            .await
            .expect("propose succeeds");
        assert!(result.terminate);
        assert!(!result.is_error);
    }

    #[test]
    fn slug_validation() {
        assert!(validate_slug("auth-token-refresh").is_ok());
        assert!(validate_slug("plan_1").is_ok());
        assert!(validate_slug("").is_err());
        assert!(validate_slug("a/b").is_err());
        assert!(validate_slug("a b").is_err());
        assert!(validate_slug("../evil").is_err());
    }

    #[test]
    fn title_resolution_chain() {
        // Supplied wins.
        assert_eq!(
            resolve_plan_title(Some("My Plan"), "# Heading", "slug"),
            "My Plan"
        );
        // Else first heading.
        assert_eq!(
            resolve_plan_title(None, "intro\n## Steps\n- x", "slug"),
            "Steps"
        );
        // Bare '#' with no text is not a heading.
        assert_eq!(resolve_plan_title(None, "#\n## Real", "slug"), "Real");
        // Else slug.
        assert_eq!(resolve_plan_title(None, "no heading here", "slug"), "slug");
        // Whitespace-only supplied falls through.
        assert_eq!(resolve_plan_title(Some("  "), "# H", "slug"), "H");
    }

    #[test]
    fn plan_dir_containment() {
        let plans = Path::new("/home/u/.manox/plans");
        let cwd = Path::new("/home/u/proj");
        let params = |p: &str| serde_json::json!({ "path": p });

        // Absolute inside.
        assert!(is_plan_dir_path_param(
            &params("/home/u/.manox/plans/auth-plan.md"),
            plans,
            cwd
        ));
        // Traversal cannot escape.
        assert!(!is_plan_dir_path_param(
            &params("/home/u/.manox/plans/../../proj/src/main.rs"),
            plans,
            cwd
        ));
        // Relative resolves against cwd → outside.
        assert!(!is_plan_dir_path_param(&params("src/main.rs"), plans, cwd));
        // A sibling prefix dir is not contained.
        assert!(!is_plan_dir_path_param(
            &params("/home/u/.manox/plans-evil/x.md"),
            plans,
            cwd
        ));
        // Missing path param.
        assert!(!is_plan_dir_path_param(&serde_json::json!({}), plans, cwd));
    }

    #[test]
    fn plan_dir_containment_is_symlink_aware() {
        let tmp = tempfile::tempdir().unwrap();
        let plans = tmp.path().join("plans");
        std::fs::create_dir_all(&plans).unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        // A symlink inside the plans dir escaping to the outside dir.
        std::os::unix::fs::symlink(&outside, plans.join("escape")).unwrap();
        // A symlink inside the plans dir pointing deeper inside it.
        std::fs::create_dir_all(plans.join("real")).unwrap();
        std::os::unix::fs::symlink(plans.join("real"), plans.join("inner")).unwrap();

        let params = |p: &std::path::Path| serde_json::json!({ "path": p });

        // Through the escaping symlink → outside → blocked.
        assert!(!is_plan_dir_path_param(
            &params(&plans.join("escape").join("evil.md")),
            &plans,
            tmp.path()
        ));
        // Through the inside-pointing symlink → still inside → allowed.
        assert!(is_plan_dir_path_param(
            &params(&plans.join("inner").join("ok-plan.md")),
            &plans,
            tmp.path()
        ));
    }

    #[test]
    fn gate_blocks_mutating_tools_while_active() {
        let state = PlanSessionState::new();
        let plans = PathBuf::from("/home/u/.manox/plans");
        let cwd = PathBuf::from("/home/u/proj");
        // Read-only resolver: Explore is read-only; Sailor is not.
        let read_only: ReadOnlySubagentResolver = Arc::new(|name: &str| name == "Explore");
        let hook = gate_handler(Arc::clone(&state), plans.clone(), cwd.clone(), read_only);

        let run = |tool: &str, args: serde_json::Value| {
            let ctx = pi::harness::HookContext::new(pi::harness::HookPoint::ToolCall).with_data(
                serde_json::json!({ "tool_call_id": "c1", "tool_name": tool, "args": args }),
            );
            hook(ctx)
        };

        // Inactive: nothing blocked.
        assert!(
            run("Bash", serde_json::json!({"command": "ls"}))
                .block_reason
                .is_none()
        );

        state.set(true, None);
        // Research + devices pass.
        assert!(
            run("Read", serde_json::json!({"path": "src/main.rs"}))
                .block_reason
                .is_none()
        );
        assert!(run("Grep", serde_json::json!({})).block_reason.is_none());
        assert!(
            run("AskUserQuestion", serde_json::json!({}))
                .block_reason
                .is_none()
        );
        assert!(
            run(PROPOSE_PLAN, serde_json::json!({"slug": "x"}))
                .block_reason
                .is_none()
        );
        // Plan-file writes pass; working-tree writes don't.
        assert!(
            run(
                "Write",
                serde_json::json!({"path": "/home/u/.manox/plans/x-plan.md"})
            )
            .block_reason
            .is_none()
        );
        assert!(
            run("Write", serde_json::json!({"path": "src/main.rs"}))
                .block_reason
                .is_some()
        );
        assert!(
            run(
                "Edit",
                serde_json::json!({"path": "/home/u/.manox/plans/../../x.md"})
            )
            .block_reason
            .is_some()
        );
        // Bash / other mutating tools blocked.
        assert!(
            run("Bash", serde_json::json!({"command": "ls"}))
                .block_reason
                .is_some()
        );
        assert!(run("Monitor", serde_json::json!({})).block_reason.is_some());
        assert!(
            run("mcp__srv__tool", serde_json::json!({}))
                .block_reason
                .is_some()
        );
        // Read-only subagent (Explore) passes; write/bash (Sailor) and
        // worktree-isolated dispatch are blocked.
        assert!(
            run(
                "Agent",
                serde_json::json!({"subagent_type": "Explore", "prompt": "x"})
            )
            .block_reason
            .is_none()
        );
        assert!(
            run(
                "Agent",
                serde_json::json!({"subagent_type": "Sailor", "prompt": "x"})
            )
            .block_reason
            .is_some()
        );
        assert!(
            run(
                "Agent",
                serde_json::json!({"subagent_type": "Explore", "prompt": "x", "isolation": "worktree"})
            )
            .block_reason
            .is_some()
        );
        assert!(
            run("Agent", serde_json::json!({"prompt": "x"}))
                .block_reason
                .is_some()
        );

        // Exit plan mode: everything passes again.
        state.set(false, None);
        assert!(
            run("Bash", serde_json::json!({"command": "ls"}))
                .block_reason
                .is_none()
        );
    }

    #[test]
    fn injection_only_while_active() {
        let state = PlanSessionState::new();
        let hook = injection_handler(Arc::clone(&state));
        let ctx = || pi::harness::HookContext::new(pi::harness::HookPoint::BeforeAgentStart);

        // Inactive: no injection even with instructions staged.
        state.set_active_instructions(Some("PLAN MODE".to_string()));
        assert!(hook(ctx()).inject_messages.is_empty());

        // Active: instructions injected as a user message.
        state.set(true, None);
        state.set_active_instructions(Some("PLAN MODE".to_string()));
        let out = hook(ctx());
        assert_eq!(out.inject_messages.len(), 1);

        // Exit clears the staged instructions.
        state.set(false, None);
        assert!(state.active_instructions().is_none());
        assert!(hook(ctx()).inject_messages.is_empty());
    }
}
