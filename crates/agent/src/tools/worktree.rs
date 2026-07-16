//! `enter_worktree` / `exit_worktree` — harness-level git worktree management.
//!
//! These tools give the model the same worktree workflow Claude Code offers:
//! enter an isolated worktree on a fresh branch, work there with the session
//! cwd switched (so every tool automatically operates in the worktree without
//! manual `cd`), then exit — keeping or removing the worktree + branch.
//!
//! Entering a worktree rebuilds the owning `Thread`'s tool registry with a
//! worktree-aware [`SandboxPolicy`]: the bound repo's shared `.git` becomes
//! writable (so `git commit`/`rebase`/`push` against the main repo's `.git`
//! succeed) and network is enabled — a worktree is an approved isolation
//! context, and git workflows need `push`/`fetch` frictionless. The
//! c5aefe4d threat (unauthorized `cd` into a sibling repo's `.git`) stays
//! blocked: only the bound repo's `.git` is de-protected.
//!
//! The git shell-outs (`git worktree add`/`remove`, `git rev-parse`) run on
//! the tokio runtime and bridge back via `async_channel` (executor-agnostic,
//! so the gpui-side `cx.spawn` can await them) — they touch `.git` (protected
//! under the default sandbox) so they cannot go through the sandboxed `bash`
//! tool; approval gates the entry/exit instead. Model-facing strings
//! (descriptions, errors, confirmations) are English and never localized
//! (CLAUDE.md i18n).

use std::path::{Path, PathBuf};

use gpui::{App, AsyncApp, Task, WeakEntity};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::thread::Thread;
use crate::tool::{AgentTool, ToolContext};

/// The `enter_worktree` tool. Creates (or re-enters) a git worktree and
/// switches the session cwd to it. Main-thread-only — it mutates the owning
/// `Thread`'s cwd and rebuilds its tool registry.
pub struct EnterWorktreeTool {
    parent: WeakEntity<Thread>,
}

impl EnterWorktreeTool {
    pub fn new(parent: WeakEntity<Thread>) -> Self {
        Self { parent }
    }
}

/// The `exit_worktree` tool. Leaves the active worktree, optionally removing
/// it and its branch. Main-thread-only.
pub struct ExitWorktreeTool {
    parent: WeakEntity<Thread>,
}

impl ExitWorktreeTool {
    pub fn new(parent: WeakEntity<Thread>) -> Self {
        Self { parent }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EnterWorktreeInput {
    /// Name for a NEW worktree (and its branch). Auto-generated as
    /// `wt-<short>` when absent. Mutually exclusive with `path`.
    #[serde(default)]
    name: Option<String>,
    /// Path to an EXISTING worktree to re-enter (e.g. one left by a prior
    /// `exit_worktree` with `action=keep`). Mutually exclusive with `name`.
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

const ENTER_DESCRIPTION: &str = "Enter a git worktree on an isolated branch and switch the session working directory to it. \
     All subsequent tools (read_file, write_file, edit_file, bash, …) operate in the worktree \
     automatically — no manual `cd`. While in a worktree, git operations (commit, rebase, push, \
     fetch) run without approval: the bound repo's `.git` is writable and network is enabled, \
     so `git push` works frictionlessly. Use this when branching off for isolated work, or when \
     explicitly told to work in a worktree. Exit with `exit_worktree` (keep or remove). Enter \
     re-baselines the provider prefix cache (the cwd line in the system prompt changes) — that \
     is expected. Pass `name` to create a new worktree+branch under `<project>/.claude/worktrees/`, \
     or `path` to re-enter an existing one. The base ref is `origin/<default-branch>` (fallback \
     `HEAD` when no remote tracking branch exists).";

const EXIT_DESCRIPTION: &str = "Leave the active git worktree. `action=keep` (default) switches the session cwd back to the \
     prior directory but leaves the worktree and branch on disk — you can re-enter it later with \
     `enter_worktree` (passing `path`). `action=remove` deletes the worktree and its branch; it is \
     refused when the working tree is dirty unless `discard_changes=true`. Only available while \
     inside a worktree.";

impl AgentTool for EnterWorktreeTool {
    fn name(&self) -> &str {
        "enter_worktree"
    }
    fn description(&self) -> &str {
        ENTER_DESCRIPTION
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema::<EnterWorktreeInput>()
    }
    /// Creating a worktree is an unsandboxed git + FS mutation and switches the
    /// session cwd — gate on approval. A user who trusts the workflow can
    /// always-allow it via the permission cache.
    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parent = self.parent.clone();
        cx.spawn(async move |cx: &mut AsyncApp| {
            let parsed: EnterWorktreeInput = match serde_json::from_value(input) {
                Ok(p) => p,
                Err(e) => return Err(format!("enter_worktree input parse failed: {e}")),
            };
            if parsed.name.is_some() && parsed.path.is_some() {
                return Err(
                    "enter_worktree: `name` and `path` are mutually exclusive.".to_string(),
                );
            }

            // Refuse to nest: a second enter would overwrite the prior-cwd
            // restore point, losing the original project root on exit. Exit the
            // current worktree first.
            if parent
                .read_with(cx, |t, _| t.worktree().is_some())
                .unwrap_or(false)
            {
                return Err(
                    "Already in a worktree — call `exit_worktree` first before entering another."
                        .to_string(),
                );
            }

            let project_root = parent
                .read_with(cx, |t, _| {
                    t.project()
                        .cloned()
                        .unwrap_or_else(|| t.cwd().to_path_buf())
                })
                .map_err(|_| "thread dropped".to_string())?;

            let (worktree_path, known_branch, is_existing) = match (&parsed.name, &parsed.path) {
                (Some(name), None) => {
                    validate_worktree_name(name)?;
                    (worktree_dir(&project_root, name), Some(name.clone()), false)
                }
                (None, Some(path_str)) => {
                    // Restrict re-entry to worktrees under this project's
                    // `.claude/worktrees/` — otherwise an approval-gated
                    // `enter_worktree path=/foreign/repo` would de-protect a
                    // foreign repo's `.git` and enable network for it, a
                    // privilege escalation beyond the intended same-repo
                    // isolation context.
                    let candidate = PathBuf::from(path_str);
                    let anchor = project_root.join(".claude").join("worktrees");
                    let canon_candidate = canonicalize_best_effort(&candidate);
                    let canon_anchor = canonicalize_best_effort(&anchor);
                    if !canon_candidate.starts_with(&canon_anchor) {
                        return Err(format!(
                            "enter_worktree `path` must be under {}. Got: {}",
                            canon_anchor.display(),
                            candidate.display()
                        ));
                    }
                    (candidate, None, true)
                }
                (None, None) => {
                    let name = generate_name();
                    (worktree_dir(&project_root, &name), Some(name), false)
                }
                _ => unreachable!(),
            };

            // Git phase: create the worktree (when new), resolve the branch
            // (when re-entering), and resolve the bound repo's shared `.git`.
            let wt_for_git = worktree_path.clone();
            let project_for_git = project_root.clone();
            let git_result = spawn_git(async move {
                if !is_existing {
                    ensure_parent(&wt_for_git).await?;
                    let base_ref = resolve_base_ref(&project_for_git).await;
                    let path_str = wt_for_git.display().to_string();
                    let branch_arg = known_branch
                        .as_deref()
                        .unwrap_or("worktree")
                        .to_string();
                    let args: Vec<String> = if base_ref == "HEAD" {
                        vec![
                            "worktree".into(),
                            "add".into(),
                            "-b".into(),
                            branch_arg,
                            path_str,
                            "HEAD".into(),
                        ]
                    } else {
                        vec![
                            "worktree".into(),
                            "add".into(),
                            "-b".into(),
                            branch_arg,
                            path_str,
                            base_ref,
                        ]
                    };
                    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
                    run_git(&project_for_git, &refs).await?;
                }
                let branch = match &known_branch {
                    Some(b) => b.clone(),
                    None => run_git(&wt_for_git, &["branch", "--show-current"])
                        .await?
                        .trim()
                        .to_string(),
                };
                let git_dir = run_git(&wt_for_git, &["rev-parse", "--git-common-dir"]).await?;
                Ok::<_, anyhow::Error>((git_dir, branch))
            })
            .await;
            let (git_common_dir_str, branch) = match git_result {
                Ok(v) => v,
                Err(e) => return Err(e),
            };

            let git_common_dir = absolutize(&worktree_path, &git_common_dir_str);

            let _ = parent.update(cx, |t, cx| {
                t.enter_worktree(worktree_path.clone(), branch.clone(), git_common_dir, cx);
            });

            Ok(format!(
                "Entered worktree at {} on branch `{}`. Working directory switched; tools now operate here. Git operations (commit/push) run without approval.",
                worktree_path.display(),
                branch
            ))
        })
    }
}

impl AgentTool for ExitWorktreeTool {
    fn name(&self) -> &str {
        "exit_worktree"
    }
    fn description(&self) -> &str {
        EXIT_DESCRIPTION
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema::<ExitWorktreeInput>()
    }
    /// Both `keep` and `remove` switch the session cwd and rebuild the tool
    /// registry — a harness-level state mutation that must run in the serial
    /// approval queue, not the free-parallel batch (a parallel `read_file` would
    /// resolve paths against a cwd mid-transition). `remove` additionally
    /// deletes a branch + worktree. Gate both on approval.
    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parent = self.parent.clone();
        cx.spawn(async move |cx: &mut AsyncApp| {
            let parsed: ExitWorktreeInput = match serde_json::from_value(input) {
                Ok(p) => p,
                Err(e) => return Err(format!("exit_worktree input parse failed: {e}")),
            };
            let action = match parsed.action.as_deref() {
                None | Some("keep") => "keep",
                Some("remove") => "remove",
                Some(other) => {
                    return Err(format!(
                        "exit_worktree `action` must be `keep` or `remove`, got: {other:?}"
                    ));
                }
            }
            .to_string();
            let discard = parsed.discard_changes.unwrap_or(false);

            let snap = parent
                .read_with(cx, |t, _| {
                    t.worktree().map(|w| (w.path.clone(), w.branch.clone()))
                })
                .map_err(|_| "thread dropped".to_string())?;
            let Some((worktree_path, branch)) = snap else {
                return Err("Not in a worktree.".to_string());
            };
            let project_root = parent
                .read_with(cx, |t, _| {
                    t.project()
                        .cloned()
                        .unwrap_or_else(|| t.cwd().to_path_buf())
                })
                .map_err(|_| "thread dropped".to_string())?;

            if action == "remove" {
                let wt = worktree_path.clone();
                let branch_clone = branch.clone();
                let project = project_root.clone();
                spawn_git(async move {
                    let status = run_git(&wt, &["status", "--porcelain"]).await?;
                    let dirty = !status.trim().is_empty();
                    if dirty && !discard {
                        return Err(anyhow::anyhow!(
                            "Worktree has uncommitted changes. Set `discard_changes: true` to remove anyway, or commit first / exit with `action: keep`.\n\n{status}"
                        ));
                    }
                    let path_str = wt.display().to_string();
                    let remove_args: Vec<&str> = if discard {
                        vec!["worktree", "remove", "--force", &path_str]
                    } else {
                        vec!["worktree", "remove", &path_str]
                    };
                    run_git(&project, &remove_args).await?;
                    let _ = run_git(&project, &["branch", "-D", &branch_clone]).await;
                    Ok::<_, anyhow::Error>(())
                })
                .await?;
            // The worktree is still active on git failure — the `?` surfaces
            // the error so the model can recover (commit, then retry) without
            // exiting.
        }

            let _ = parent.update(cx, |t, cx| {
                let _ = t.exit_worktree(cx);
            });

            Ok(format!(
                "Exited worktree (action: {action}). Working directory restored."
            ))
        })
    }
}

// ─── tokio bridge + git helpers ────────────────────────────────────────────

/// Run a future on the global tokio runtime and await its result from the
/// gpui executor. `async_channel` is executor-agnostic, so the gpui-side
/// `rx.recv()` polls cleanly without needing `background_spawn` (which takes
/// `&mut App`, unavailable inside `cx.spawn`'s `&mut AsyncApp`).
async fn spawn_git<F, R>(f: F) -> Result<R, String>
where
    F: std::future::Future<Output = Result<R, anyhow::Error>> + Send + 'static,
    R: Send + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    crate::runtime::handle().spawn(async move {
        let r = f.await.map_err(|e| e.to_string());
        let _ = tx.send(r).await;
    });
    rx.recv()
        .await
        .map_err(|_| "git operation cancelled".to_string())
        .and_then(|r| r)
}

/// Run `git` with `args` in `cwd`, returning trimmed stdout. The error string
/// carries stderr so the model sees git's own diagnostic.
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

/// Ensure the parent directory of `worktree_path` exists so `git worktree add`
/// can create the leaf.
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

/// Validate a user-supplied worktree `name` (used as both the directory leaf
/// and the git branch name). Rejects path separators, traversal, null, and a
/// leading dash/dot — git's `check-ref-format` would reject most of these for
/// the branch name anyway, but validating up front avoids stray directory
/// creation via `ensure_parent` for a name that git will then refuse.
fn validate_worktree_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("enter_worktree `name` cannot be empty".to_string());
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return Err(format!(
            "enter_worktree `name` must not contain path separators: {name:?}"
        ));
    }
    if name.contains("..") {
        return Err(format!(
            "enter_worktree `name` must not contain `..`: {name:?}"
        ));
    }
    if name.starts_with('-') || name.starts_with('.') {
        return Err(format!(
            "enter_worktree `name` must not start with `-` or `.`: {name:?}"
        ));
    }
    Ok(())
}

/// Best-effort canonicalize that resolves the longest existing ancestor and
/// rejoins the tail — `Path::canonicalize` fails for non-existent paths, and a
/// not-yet-created worktree leaf must still be compared against the anchor.
/// Mirrors `sandbox::canonicalize_best_effort` without reaching across modules.
fn canonicalize_best_effort(path: &Path) -> PathBuf {
    if path.exists() {
        return path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    }
    let Some(parent) = path.parent() else {
        return path.to_path_buf();
    };
    if parent == Path::new("") {
        return path.to_path_buf();
    }
    let canon_parent = canonicalize_best_effort(parent);
    match path.file_name() {
        Some(name) => canon_parent.join(name),
        None => canon_parent,
    }
}

/// `wt-` + first 8 hex chars of a fresh UUID — short, unique, valid as both a
/// directory and branch name.
fn generate_name() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let short = &id[..8];
    format!("wt-{short}")
}

/// `git rev-parse --git-common-dir` may return a relative path (`.git`); when
/// run from a worktree it is normally absolute, but resolve relative results
/// against the worktree dir so the sandbox de-protects the right path.
fn absolutize(worktree_dir: &Path, git_common_dir: &str) -> PathBuf {
    let p = PathBuf::from(git_common_dir);
    if p.is_absolute() {
        p
    } else {
        worktree_dir.join(p)
    }
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
    fn absolutize_keeps_absolute_and_joins_relative() {
        let wt = Path::new("/tmp/proj/.claude/worktrees/wt-1");
        assert_eq!(
            absolutize(wt, "/tmp/proj/.git"),
            PathBuf::from("/tmp/proj/.git")
        );
        assert_eq!(
            absolutize(wt, ".git"),
            PathBuf::from("/tmp/proj/.claude/worktrees/wt-1/.git")
        );
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
}
