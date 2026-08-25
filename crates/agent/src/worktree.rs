//! `EnterWorktree` / `ExitWorktree` — git worktree management for the pi
//! path (ported from the retired manox harness).
//!
//! The model enters an isolated worktree on a fresh branch; the session's
//! cwd follows, so every tool operates in the worktree without manual `cd`.
//! Exit returns to the prior session (keep) and optionally deletes the
//! worktree + branch (remove).
//!
//! pi wiring (differs from the retired harness — deviation documented per
//! CLAUDE.md): the manox harness swapped the live thread's cwd + tool
//! registry in place. The pi kernel pins a session's cwd at assembly and
//! offers no runtime re-pin, so the swap rides the kernel-native `forkFrom`
//! primitive: entering forks the current session into a new file whose
//! header cwd is the worktree (transcript carried over verbatim,
//! `parentSession` chained); exiting reopens the original session file.
//! Worktree work therefore lives in a forked session — visible in the
//! session list with the original intact — rather than mutating one
//! transcript's cwd mid-flight.
//!
//! The session swap itself is actor-side (`SessionCmd::EnterWorktree` /
//! `ExitWorktree`), processed between turns; the tools do the git phase and
//! queue the swap. Both tools require approval: the retired harness left
//! enter ungated because its sandbox provided isolation — the pi path has
//! no bash sandbox yet, so entry/exit stay conservative.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use pi_extensions::session_meta::WorktreeMeta;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::pi_engine::SessionCmd;

/// Shared active-worktree state: the actor writes it on swap, the tools
/// read it for the nest guard and exit routing.
pub type WorktreeState = Arc<Mutex<Option<WorktreeMeta>>>;

pub fn new_state() -> WorktreeState {
    Arc::new(Mutex::new(None))
}

// ─── inputs ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EnterWorktreeInput {
    /// Name for a NEW worktree (and its branch). Auto-generated as
    /// `wt-<short>` when absent. Mutually exclusive with `path`.
    #[serde(default)]
    name: Option<String>,
    /// Path to an EXISTING worktree to re-enter (e.g. one left by a prior
    /// `ExitWorktree` with `action=keep`). Mutually exclusive with `name`.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ExitWorktreeInput {
    /// `keep` (default) leaves the worktree and branch on disk; `remove`
    /// deletes both.
    #[serde(default)]
    action: Option<String>,
    /// When `action=remove` and the worktree has uncommitted changes, set
    /// `true` to discard them. Otherwise removal is refused so work is not
    /// lost silently.
    #[serde(default)]
    discard_changes: Option<bool>,
}

fn schema<T: JsonSchema>() -> serde_json::Value {
    let mut value = serde_json::to_value(schemars::schema_for!(T)).expect("schema serialization");
    if let Some(obj) = value.as_object_mut() {
        obj.remove("$schema");
        obj.remove("$defs");
    }
    value
}

const ENTER_DESCRIPTION: &str = "Enter a git worktree on an isolated branch and switch the session working directory to it. \
     All subsequent tools (Read, Write, Edit, Bash, …) operate in the worktree automatically — no manual `cd`. \
     The context switch takes effect from the next turn (the current turn finishes in the original directory). \
     Use this when branching off for isolated work, or when explicitly told to work in a worktree. \
     Exit with `ExitWorktree` (keep or remove). Pass `name` to create a new worktree+branch under \
     `<project>/.claude/worktrees/`, or `path` to re-enter any existing git worktree, including one \
     belonging to a different (sibling) repository. The base ref is `origin/<default-branch>` \
     (fallback `HEAD` when no remote tracking branch exists).";

const EXIT_DESCRIPTION: &str = "Leave the active git worktree. `action=keep` (default) returns the session to the prior \
     directory but leaves the worktree and branch on disk — you can re-enter it later with `EnterWorktree` \
     (passing `path`). `action=remove` deletes the worktree and its branch; it is refused when the working \
     tree is dirty unless `discard_changes=true`. Only available while inside a worktree.";

// ─── tools ──────────────────────────────────────────────────────────────────

pub struct EnterWorktreeTool {
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
    state: WorktreeState,
}

impl EnterWorktreeTool {
    pub(crate) fn new(cmd_tx: mpsc::UnboundedSender<SessionCmd>, state: WorktreeState) -> Self {
        Self { cmd_tx, state }
    }
}

#[async_trait::async_trait]
impl AgentTool for EnterWorktreeTool {
    fn name(&self) -> &str {
        crate::tools::ENTER_WORKTREE
    }
    fn description(&self) -> &str {
        ENTER_DESCRIPTION
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<EnterWorktreeInput>()
    }
    // No sandbox on the pi path yet to provide isolation — gate entry.
    fn requires_approval(&self, _params: &serde_json::Value) -> bool {
        true
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: EnterWorktreeInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        if input.name.is_some() && input.path.is_some() {
            return Err(ToolError::InvalidArguments(
                "enter_worktree: `name` and `path` are mutually exclusive.".into(),
            ));
        }
        // Refuse to nest: a second enter would overwrite the prior-cwd
        // restore point, losing the original project root on exit.
        if self.state.lock().unwrap().is_some() {
            return Err(ToolError::ExecutionFailed(
                "Already in a worktree — call `exit_worktree` first before entering another."
                    .into(),
            ));
        }
        let project_root = ctx.cwd().to_path_buf();
        // An existing-path enter must land on a real git working tree: the
        // explicit check keeps a non-git directory from failing deep in the
        // branch resolution, and admits worktrees of OTHER repositories
        // (multi-repo orchestration enters sibling checkouts).
        if let Some(path_str) = &input.path {
            let is_dir = tokio::fs::metadata(path_str)
                .await
                .ok()
                .is_some_and(|m| m.is_dir());
            if !is_dir {
                return Err(ToolError::ExecutionFailed(format!(
                    "enter_worktree: path does not exist or is not a directory: {path_str}"
                )));
            }
            let inside =
                run_git(Path::new(path_str), &["rev-parse", "--is-inside-work-tree"]).await;
            match inside {
                Ok(out) if out.trim() == "true" => {}
                _ => {
                    return Err(ToolError::ExecutionFailed(format!(
                        "enter_worktree: {path_str} is not a git worktree (not inside a git working tree)"
                    )));
                }
            }
        }
        let (worktree_path, known_branch, is_existing) = match (&input.name, &input.path) {
            (Some(name), None) => {
                validate_worktree_name(name)?;
                (worktree_dir(&project_root, name), Some(name.clone()), false)
            }
            (None, Some(path_str)) => (PathBuf::from(path_str), None, true),
            (None, None) => {
                let name = generate_name();
                (worktree_dir(&project_root, &name), Some(name), false)
            }
            _ => unreachable!(),
        };

        // Git phase: create the worktree (when new) or resolve the branch
        // (when re-entering an existing one). A NEW worktree is created
        // under the session's project root, so that root must itself be a
        // git working tree — validated up front for the same clear-error
        // reason the `path` branch validates its target.
        if !is_existing {
            let inside = run_git(&project_root, &["rev-parse", "--is-inside-work-tree"]).await;
            match inside {
                Ok(out) if out.trim() == "true" => {}
                _ => {
                    return Err(ToolError::ExecutionFailed(format!(
                        "enter_worktree: {} is not a git working tree; cannot create a worktree under it",
                        project_root.display()
                    )));
                }
            }
            ensure_parent(&worktree_path)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("{e:#}")))?;
            let base_ref = resolve_base_ref(&project_root).await;
            let branch_arg = known_branch.clone().unwrap_or_else(|| "worktree".into());
            run_git(
                &project_root,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch_arg,
                    &worktree_path.display().to_string(),
                    &base_ref,
                ],
            )
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e:#}")))?;
        }
        let branch = match &known_branch {
            Some(b) => b.clone(),
            None => run_git(&worktree_path, &["branch", "--show-current"])
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("{e:#}")))?
                .trim()
                .to_string(),
        };
        // The owning repo's git common dir: linked-worktree commits write
        // there and exit-time removal runs against it. Resolved from the
        // worktree itself so a worktree of ANOTHER repository carries its
        // own repo binding (relative output joins against the worktree).
        let common = run_git(&worktree_path, &["rev-parse", "--git-common-dir"])
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e:#}")))?;
        let common_path = PathBuf::from(common.trim());
        let git_common_dir =
            crate::sandbox::canonicalize_best_effort(&if common_path.is_relative() {
                worktree_path.join(&common_path)
            } else {
                common_path
            });

        self.cmd_tx
            .send(SessionCmd::EnterWorktree {
                worktree_path: worktree_path.clone(),
                branch: branch.clone(),
                original_cwd: project_root.clone(),
                git_common_dir,
            })
            .map_err(|_| ToolError::ExecutionFailed("engine actor gone".into()))?;

        Ok(AgentToolResult::text(format!(
            "Entered worktree at {} on branch `{}`. The session switches to it from the next turn; tools will operate there automatically.",
            worktree_path.display(),
            branch
        )))
    }
}

pub struct ExitWorktreeTool {
    cmd_tx: mpsc::UnboundedSender<SessionCmd>,
    state: WorktreeState,
}

impl ExitWorktreeTool {
    pub(crate) fn new(cmd_tx: mpsc::UnboundedSender<SessionCmd>, state: WorktreeState) -> Self {
        Self { cmd_tx, state }
    }
}

#[async_trait::async_trait]
impl AgentTool for ExitWorktreeTool {
    fn name(&self) -> &str {
        crate::tools::EXIT_WORKTREE
    }
    fn description(&self) -> &str {
        EXIT_DESCRIPTION
    }
    fn parameters_schema(&self) -> serde_json::Value {
        schema::<ExitWorktreeInput>()
    }
    // Switches the session and (optionally) deletes a branch + worktree.
    fn requires_approval(&self, _params: &serde_json::Value) -> bool {
        true
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: ExitWorktreeInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let action = match input.action.as_deref() {
            None | Some("keep") => "keep",
            Some("remove") => "remove",
            Some(other) => {
                return Err(ToolError::InvalidArguments(format!(
                    "exit_worktree `action` must be `keep` or `remove`, got: {other:?}"
                )));
            }
        };
        let discard = input.discard_changes.unwrap_or(false);

        let snap = self.state.lock().unwrap().clone();
        let Some(meta) = snap else {
            return Err(ToolError::ExecutionFailed("Not in a worktree.".into()));
        };
        let worktree_path = PathBuf::from(&meta.worktree_path);
        // The owning repo root derives from the worktree's git common dir —
        // the worktree may belong to another repository than the session cwd.
        let repo_root = Path::new(&meta.git_common_dir)
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                ToolError::ExecutionFailed(
                    "exit_worktree: cannot derive the owning repo root from git_common_dir".into(),
                )
            })?;

        if action == "remove" {
            let status = run_git(&worktree_path, &["status", "--porcelain"])
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("{e:#}")))?;
            let dirty = !status.trim().is_empty();
            if dirty && !discard {
                return Err(ToolError::ExecutionFailed(format!(
                    "Worktree has uncommitted changes. Set `discard_changes: true` to remove \
                     anyway, or commit first / exit with `action: keep`.\n\n{status}"
                )));
            }
            let path_str = worktree_path.display().to_string();
            let remove_args: Vec<&str> = if discard {
                vec!["worktree", "remove", "--force", &path_str]
            } else {
                vec!["worktree", "remove", &path_str]
            };
            // On git failure the worktree stays active — the error surfaces
            // so the model can recover (commit, then retry) without exiting.
            run_git(&repo_root, &remove_args)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("{e:#}")))?;
            let _ = run_git(&repo_root, &["branch", "-D", &meta.branch]).await;
        }

        self.cmd_tx
            .send(SessionCmd::ExitWorktree)
            .map_err(|_| ToolError::ExecutionFailed("engine actor gone".into()))?;

        Ok(AgentToolResult::text(format!(
            "Exited worktree (action: {action}). The session returns to the original directory from the next turn."
        )))
    }
}

// ─── git helpers (ported) ───────────────────────────────────────────────────

/// Run `git` with `args` in `cwd`, returning trimmed stdout. The error
/// carries git's stderr for model-facing recovery.
async fn run_git(cwd: &Path, args: &[&str]) -> Result<String, anyhow::Error> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await?;
    if !out.status.success() {
        return Err(anyhow::anyhow!(
            "git {} failed (exit {}):\n{}",
            args.join(" "),
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve the base ref for a new worktree: `origin/<default-branch>` when a
/// remote HEAD is configured, else `HEAD` (offline / no-remote fallback).
async fn resolve_base_ref(project_root: &Path) -> String {
    match run_git(project_root, &["rev-parse", "--abbrev-ref", "origin/HEAD"]).await {
        Ok(s) => {
            let s = s.trim();
            if s.is_empty() || s == "origin/HEAD" {
                "HEAD".to_string()
            } else {
                s.to_string()
            }
        }
        Err(_) => "HEAD".to_string(),
    }
}

/// Ensure the parent directory of `worktree_path` exists so `git worktree
/// add` can create the leaf.
async fn ensure_parent(worktree_path: &Path) -> Result<(), anyhow::Error> {
    if let Some(parent) = worktree_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    Ok(())
}

/// `<project_root>/.claude/worktrees/<name>`.
fn worktree_dir(project_root: &Path, name: &str) -> PathBuf {
    project_root.join(".claude").join("worktrees").join(name)
}

/// Validate a user-supplied worktree `name` (used as both the directory
/// leaf and the git branch name). Rejects path separators, traversal, null,
/// and a leading dash/dot — git's `check-ref-format` would reject most of
/// these for the branch name anyway, but validating up front avoids stray
/// directory creation via `ensure_parent` for a name that git will then
/// refuse.
fn validate_worktree_name(name: &str) -> Result<(), ToolError> {
    if name.is_empty() {
        return Err(ToolError::InvalidArguments(
            "enter_worktree `name` cannot be empty".into(),
        ));
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(ToolError::InvalidArguments(format!(
            "enter_worktree `name` must not contain path separators: {name:?}"
        )));
    }
    if name.contains("..") {
        return Err(ToolError::InvalidArguments(format!(
            "enter_worktree `name` must not contain `..`: {name:?}"
        )));
    }
    if name.starts_with('-') || name.starts_with('.') {
        return Err(ToolError::InvalidArguments(format!(
            "enter_worktree `name` must not start with `-` or `.`: {name:?}"
        )));
    }
    Ok(())
}

/// `wt-` + first 8 hex chars of a fresh UUID — short, unique, valid as both
/// a directory and branch name.
fn generate_name() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let short = &id[..8];
    format!("wt-{short}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_name_is_short_wt_prefixed() {
        let n = generate_name();
        assert!(n.starts_with("wt-"), "{n}");
        assert_eq!(n.len(), 3 + 8, "{n}");
        assert!(n[3..].chars().all(|c| c.is_ascii_hexdigit()), "{n}");
    }

    #[test]
    fn worktree_dir_lives_under_claude_worktrees() {
        let root = Path::new("/tmp/proj");
        let d = worktree_dir(root, "feat-x");
        assert_eq!(d, PathBuf::from("/tmp/proj/.claude/worktrees/feat-x"));
    }

    #[test]
    fn validate_name_rejects_traversal_and_separators() {
        // A valid branch/dir name — accepted.
        assert!(validate_worktree_name("feat-x").is_ok());
        assert!(validate_worktree_name("wt-abc123").is_ok());
        // Path separators, traversal, null, and leading dash/dot rejected so
        // `name` cannot escape `.claude/worktrees/` or form a bad branch.
        assert!(validate_worktree_name("").is_err());
        assert!(validate_worktree_name("a/b").is_err());
        assert!(validate_worktree_name(r"a\b").is_err());
        assert!(validate_worktree_name("..").is_err());
        assert!(validate_worktree_name("foo/../bar").is_err());
        assert!(validate_worktree_name("a\0b").is_err());
        assert!(validate_worktree_name("-branch").is_err());
        assert!(validate_worktree_name(".hidden").is_err());
    }

    #[tokio::test]
    async fn run_git_reports_stderr_on_failure() {
        let err = run_git(
            Path::new("/tmp"),
            &["rev-parse", "--verify", "definitely-not-a-ref-xyz"],
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("git rev-parse"), "{err}");
    }

    fn enter_tool() -> (EnterWorktreeTool, mpsc::UnboundedReceiver<SessionCmd>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (EnterWorktreeTool::new(tx, new_state()), rx)
    }

    fn local_ctx(cwd: PathBuf) -> pi::tool::LocalToolContext {
        pi::tool::LocalToolContext::new(
            std::sync::Arc::new(pi::env::TokioExecutionEnv::new(std::env::temp_dir())),
            cwd,
            std::sync::Arc::new(pi::tool::ToolState::new()),
        )
    }

    #[tokio::test]
    async fn enter_rejects_non_git_path() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _rx) = enter_tool();
        let ctx = local_ctx(dir.path().to_path_buf());
        // Existing non-git directory → explicit worktree validation error.
        let err = tool
            .execute(
                "call",
                serde_json::json!({ "path": dir.path().display().to_string() }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("is not a git worktree"), "{err}");
        // Missing path → existence error before git is ever consulted.
        let missing = dir.path().join("no-such-dir");
        let err = tool
            .execute(
                "call",
                serde_json::json!({ "path": missing.display().to_string() }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("does not exist or is not a directory"),
            "{err}"
        );
    }

    /// Creating a new worktree requires the session's project root to be a
    /// git working tree — a non-git cwd gets the same clear error as a
    /// non-git `path` target.
    #[tokio::test]
    async fn enter_name_rejects_non_git_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _rx) = enter_tool();
        let ctx = local_ctx(dir.path().to_path_buf());
        let err = tool
            .execute(
                "call",
                serde_json::json!({ "name": "wt-x" }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("cannot create a worktree under it"),
            "{err}"
        );
    }

    /// A linked worktree of ANY repository enters and carries its owning
    /// repo's git common dir (the multi-repo group scenario).
    #[tokio::test]
    async fn enter_resolves_git_common_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "base",
        ]);
        let wt = dir.path().join("wt");
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "feat",
            &wt.display().to_string(),
        ]);

        let (tool, mut rx) = enter_tool();
        let ctx = local_ctx(repo.clone());
        tool.execute(
            "call",
            serde_json::json!({ "path": wt.display().to_string() }),
            CancellationToken::new(),
            &ctx,
        )
        .await
        .expect("linked worktree enters");
        let SessionCmd::EnterWorktree {
            worktree_path,
            branch,
            git_common_dir,
            ..
        } = rx.try_recv().expect("enter cmd queued")
        else {
            panic!("expected EnterWorktree");
        };
        // The command carries the path as spelled; only the common dir is
        // canonicalized by the tool.
        assert_eq!(worktree_path, wt);
        assert_eq!(branch, "feat");
        assert_eq!(
            git_common_dir,
            crate::sandbox::canonicalize_best_effort(&repo.join(".git"))
        );
    }
}
