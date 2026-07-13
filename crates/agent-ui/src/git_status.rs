//! Git change/branch status for the context rail.
//!
//! Shells out to the system `git` binary (never `git2` — banned by project
//! rule) to read the working-tree diff stats and the current branch, then
//! parses the output into pure value types the UI renders from. All subprocess
//! work runs on the global tokio runtime via [`agent::runtime::handle`]; the
//! results are delivered back to the gpui-side caller through an
//! executor-agnostic `async_channel`, the same bridge the worktree tool uses.
//!
//! Parsing is split from IO so the pure functions are unit-testable without a
//! real git repo.

use std::path::PathBuf;

use agent::runtime;

/// Working-tree change stats relative to `HEAD`.
///
/// `added`/`deleted` count lines from `git diff --numstat HEAD` (binary files
/// contribute nothing — their numstat line is `-`/`-` and is skipped).
/// `untracked` counts `git ls-files --others --exclude-standard` lines.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitChangeStats {
    pub added: u64,
    pub deleted: u64,
    pub untracked: u64,
}

/// Resolved branch identity for the rail's branch row.
///
/// `branch` is `Some` on a normal checked-out branch; `detached_sha` is `Some`
/// in detached-HEAD (caller queries the short sha via `git rev-parse`). Both
/// are `None` when the cwd is not inside a git repo. `is_worktree` is flagged
/// by the caller from `Thread::worktree()` so the rail can suffix the label.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitBranchDisplay {
    pub branch: Option<String>,
    pub detached_sha: Option<String>,
    pub is_worktree: bool,
}

impl GitBranchDisplay {
    /// Whether the cwd is not under git at all.
    pub fn is_no_repo(&self) -> bool {
        self.branch.is_none() && self.detached_sha.is_none()
    }
}

/// Parse `git diff --numstat HEAD` output into line counters.
///
/// Each line is `<added>\t<deleted>\t<path>`. Binary files show `-` for both
/// counters and contribute nothing. Empty output means a clean tree (all zero).
pub fn parse_numstat(output: &str) -> GitChangeStats {
    let mut stats = GitChangeStats::default();
    for line in output.lines() {
        let mut parts = line.split('\t');
        let added = parts.next();
        let deleted = parts.next();
        // No path column → malformed line, skip rather than panic.
        if parts.next().is_none() {
            continue;
        }
        let (Some(a), Some(d)) = (added, deleted) else {
            continue;
        };
        // Binary file rows are `-`/`-`; `-` is not a count, skip it.
        if let Ok(av) = a.parse::<u64>() {
            stats.added += av;
        }
        if let Ok(dv) = d.parse::<u64>() {
            stats.deleted += dv;
        }
    }
    stats
}

/// Parse `git branch --show-current` output. Empty output means detached HEAD
/// or a non-git cwd; the caller resolves which via `git rev-parse --short HEAD`.
pub fn parse_branch(output: &str) -> GitBranchDisplay {
    let branch = output.trim();
    if branch.is_empty() {
        GitBranchDisplay::default()
    } else {
        GitBranchDisplay {
            branch: Some(branch.to_string()),
            detached_sha: None,
            is_worktree: false,
        }
    }
}

/// Parse `git rev-parse --short HEAD` output for the detached-HEAD short sha.
pub fn parse_short_sha(output: &str) -> Option<String> {
    let s = output.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Count untracked files from `git ls-files --others --exclude-standard` output.
pub fn count_untracked(output: &str) -> u64 {
    output.lines().filter(|l| !l.is_empty()).count() as u64
}

// ── IO: tokio-bridged git shell-outs ───────────────────────────────────────

/// Run `git` with `args` in `cwd` on the global tokio runtime, returning the
/// trimmed stdout. A non-zero exit or a missing `git` binary yields `None`
/// (the UI falls back to its "not a repo" / "unavailable" labels).
async fn run_git(cwd: &str, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Gather change stats + branch display for `cwd` in a single background task.
///
/// Branch resolution prefers a worktree's recorded branch (passed in via
/// `worktree_branch`) when the thread is inside a worktree; otherwise it
/// shells out to `git branch --show-current`, falling back to the short sha
/// for detached HEAD. Returns `None` entirely when `cwd` is not a git repo.
pub async fn gather(
    cwd: PathBuf,
    worktree_branch: Option<String>,
) -> Option<(GitChangeStats, GitBranchDisplay)> {
    let cwd_str = cwd.to_string_lossy().to_string();
    // A single `git rev-parse --show-toplevel` gates everything else: if it
    // fails the cwd is not under git, so the rail shows its not-a-repo label.
    run_git(&cwd_str, &["rev-parse", "--show-toplevel"]).await?;

    let is_worktree = worktree_branch.is_some();
    let branch = if let Some(b) = worktree_branch {
        Some(b)
    } else {
        match run_git(&cwd_str, &["branch", "--show-current"]).await {
            Some(s) if !s.is_empty() => Some(s),
            _ => None,
        }
    };
    let detached_sha = if branch.is_none() {
        run_git(&cwd_str, &["rev-parse", "--short", "HEAD"])
            .await
            .and_then(|s| parse_short_sha(&s))
    } else {
        None
    };
    if branch.is_none() && detached_sha.is_none() {
        // `rev-parse --show-toplevel` succeeded but neither branch nor sha
        // resolved — treat as no-repo so the label is honest.
        return None;
    }
    let display = GitBranchDisplay {
        branch,
        detached_sha,
        is_worktree,
    };

    let numstat = run_git(&cwd_str, &["diff", "--numstat", "HEAD"])
        .await
        .unwrap_or_default();
    let untracked = run_git(&cwd_str, &["ls-files", "--others", "--exclude-standard"])
        .await
        .unwrap_or_default();
    let mut stats = parse_numstat(&numstat);
    stats.untracked = count_untracked(&untracked);
    Some((stats, display))
}

/// Spawn [`gather`] on the global tokio runtime and deliver its result back to
/// the gpui executor via an `async_channel` of capacity 1. Returns the result
/// (or `None` on cancellation) so a `cx.spawn` caller can `.await` it without
/// touching `&mut App`.
pub async fn gather_bridged(
    cwd: PathBuf,
    worktree_branch: Option<String>,
) -> Option<(GitChangeStats, GitBranchDisplay)> {
    let (tx, rx) = async_channel::bounded(1);
    runtime::handle().spawn(async move {
        let r = gather(cwd, worktree_branch).await;
        let _ = tx.send(r).await;
    });
    rx.recv().await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numstat_empty_is_clean() {
        assert_eq!(parse_numstat(""), GitChangeStats::default());
    }

    #[test]
    fn numstat_normal_lines() {
        let out = "10\t2\tsrc/a.rs\n3\t0\tsrc/b.rs\n";
        assert_eq!(
            parse_numstat(out),
            GitChangeStats {
                added: 13,
                deleted: 2,
                untracked: 0,
            }
        );
    }

    #[test]
    fn numstat_binary_rows_skip() {
        let out = "-\t-\tsrc/binary.png\n5\t1\tsrc/c.rs\n";
        assert_eq!(
            parse_numstat(out),
            GitChangeStats {
                added: 5,
                deleted: 1,
                untracked: 0,
            }
        );
    }

    #[test]
    fn numstat_malformed_row_skipped() {
        let out = "garbage-no-tabs\n7\t0\tsrc/d.rs\n";
        assert_eq!(
            parse_numstat(out),
            GitChangeStats {
                added: 7,
                deleted: 0,
                untracked: 0,
            }
        );
    }

    #[test]
    fn branch_normal() {
        let d = parse_branch("main\n");
        assert_eq!(d.branch.as_deref(), Some("main"));
        assert!(d.detached_sha.is_none());
        assert!(!d.is_worktree);
    }

    #[test]
    fn branch_empty_is_detached_or_no_repo() {
        let d = parse_branch("");
        assert!(d.branch.is_none());
        assert!(d.detached_sha.is_none());
        assert!(d.is_no_repo());
    }

    #[test]
    fn branch_whitespace_only_is_detached() {
        let d = parse_branch("   \n");
        assert!(d.branch.is_none());
    }

    #[test]
    fn short_sha_normal() {
        assert_eq!(parse_short_sha("abc1234\n"), Some("abc1234".to_string()));
    }

    #[test]
    fn short_sha_empty() {
        assert_eq!(parse_short_sha(""), None);
        assert_eq!(parse_short_sha("   \n"), None);
    }

    #[test]
    fn untracked_count() {
        assert_eq!(count_untracked("a\nb\n\nc\n"), 3);
        assert_eq!(count_untracked(""), 0);
    }

    #[test]
    fn is_no_repo_true_when_both_none() {
        assert!(GitBranchDisplay::default().is_no_repo());
        assert!(
            !GitBranchDisplay {
                branch: Some("main".into()),
                detached_sha: None,
                is_worktree: false,
            }
            .is_no_repo()
        );
        assert!(
            !GitBranchDisplay {
                branch: None,
                detached_sha: Some("abc1234".into()),
                is_worktree: false,
            }
            .is_no_repo()
        );
    }
}
