//! Session-scoped extra writable roots derived from the per-call cwd.
//!
//! Two families enforce file-write containment — the fs fence
//! (`pi_approval`'s workspace-write verdict) and the bash seatbelt
//! (`sandbox::SandboxPolicy`) — and both must admit the same roots, or a
//! call one family passed the other denies. One derivation, shared:
//!
//! - A linked worktree of the session workspace is admitted automatically:
//!   its `git rev-parse --git-common-dir` matches the workspace's, so
//!   branch/commit work in a worktree is the same repository by
//!   construction. The worktree itself and the common dir are roots (linked
//!   worktree commits write into the shared git object store).
//! - Any other directory is admitted once approved through a
//!   `sandbox_permissions` escalation and stays for the session (approve
//!   once, never re-ask for the same root).
//!
//! The same-repo derivation spawns `git rev-parse` once per distinct cwd and
//! caches the result — a verdict or seatbelt render must never pay the
//! process spawn on the hot path twice for the same directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use manox_harness::sandbox::canonicalize_best_effort;

/// The shared extra-root store for one session. Cheap to clone (`Arc` bump);
/// every enforcing surface (each gated tool, the bash backends) holds the
/// same handle so an approval lands everywhere at once.
#[derive(Clone)]
pub struct GrantedRoots(Arc<Inner>);

struct Inner {
    /// The session workspace root (the session cwd at build time).
    workspace: PathBuf,
    /// Roots admitted by an approved escalation, canonical, deduplicated.
    approved: Mutex<Vec<PathBuf>>,
    /// Same-repo worktree roots keyed by canonical cwd — one `git
    /// rev-parse` per distinct directory.
    same_repo_cache: Mutex<HashMap<PathBuf, Vec<PathBuf>>>,
}

impl GrantedRoots {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        GrantedRoots(Arc::new(Inner {
            workspace: canonicalize_best_effort(&workspace.into()),
            approved: Mutex::new(Vec::new()),
            same_repo_cache: Mutex::new(HashMap::new()),
        }))
    }

    /// Record a root an approved escalation covers. Canonicalized here so
    /// later comparisons are plain prefix checks.
    pub fn approve(&self, root: impl AsRef<Path>) {
        let canon = canonicalize_best_effort(root.as_ref());
        if canon.parent().is_none() {
            // The filesystem root would admit the whole disk — never
            // accumulatable (mirrors the seatbelt's own root filter).
            return;
        }
        let mut approved = self.0.approved.lock().expect("granted roots poisoned");
        if !approved.contains(&canon) {
            approved.push(canon);
        }
    }

    /// Every extra root in force for a call running in `cwd`: the
    /// escalation-approved roots (session-wide) plus the same-repo worktree
    /// roots for `cwd` (directory-relative).
    pub fn roots_for(&self, cwd: &Path) -> Vec<PathBuf> {
        let mut roots = self
            .0
            .approved
            .lock()
            .expect("granted roots poisoned")
            .clone();
        for root in self.same_repo_roots(cwd) {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        roots
    }

    /// Same-repo worktree roots for `cwd`, cached per canonical directory.
    fn same_repo_roots(&self, cwd: &Path) -> Vec<PathBuf> {
        let canon = canonicalize_best_effort(cwd);
        if canon == self.0.workspace {
            return Vec::new();
        }
        if let Some(hit) = self
            .0
            .same_repo_cache
            .lock()
            .expect("granted roots poisoned")
            .get(&canon)
        {
            return hit.clone();
        }
        let roots = match (git_common_dir(&canon), git_common_dir(&self.0.workspace)) {
            (Some(dir_cwd), Some(dir_workspace)) if dir_cwd == dir_workspace => {
                vec![canon.clone(), dir_cwd]
            }
            _ => Vec::new(),
        };
        self.0
            .same_repo_cache
            .lock()
            .expect("granted roots poisoned")
            .insert(canon, roots.clone());
        roots
    }
}

/// The repository's git common dir for `dir` (`None` outside any git tree).
/// Linked worktrees report the main checkout's `.git`, which is exactly the
/// identity the same-repo check needs.
fn git_common_dir(dir: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("rev-parse")
        .arg("--git-common-dir")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if printed.is_empty() {
        return None;
    }
    // The printed path may be relative to `dir` (`.git` for the main
    // checkout) — resolve it against the query directory, then canonicalize
    // so both sides of the comparison are real paths.
    let resolved = Path::new(&printed);
    let absolute = if resolved.is_absolute() {
        resolved.to_path_buf()
    } else {
        dir.join(resolved)
    };
    Some(canonicalize_best_effort(&absolute))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real repo with a linked worktree: the worktree admits itself and
    /// the shared common dir; an unrelated directory admits nothing.
    #[test]
    fn linked_worktree_of_the_workspace_admits_itself() {
        let repo = tempfile::tempdir().unwrap();
        let base = repo.path().join("base");
        let wt = repo.path().join("wt");
        std::fs::create_dir(&base).unwrap();
        run_git(&base, &["init", "--initial-branch=main"]);
        // A commit to branch off of, plus the linked worktree on a branch.
        std::fs::write(base.join("f"), "x").unwrap();
        run_git(&base, &["add", "."]);
        run_git(
            &base,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "init",
            ],
        );
        run_git(
            &base,
            &["worktree", "add", wt.to_str().unwrap(), "-b", "wt-branch"],
        );

        let granted = GrantedRoots::new(&base);
        let roots = granted.roots_for(&wt);
        let canon_wt = canonicalize_best_effort(&wt);
        assert!(
            roots.contains(&canon_wt),
            "the worktree is a root: {roots:?}"
        );
        assert!(
            roots.iter().any(|r| r.ends_with(".git")),
            "the git common dir is a root: {roots:?}"
        );

        // The workspace itself admits nothing extra.
        assert!(granted.roots_for(&base).is_empty());
        // A directory outside any repo of the workspace admits nothing.
        let elsewhere = tempfile::tempdir().unwrap();
        assert!(granted.roots_for(elsewhere.path()).is_empty());
    }

    #[test]
    fn approved_roots_persist_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let granted = GrantedRoots::new(dir.path());
        granted.approve(dir.path());
        granted.approve(dir.path());
        let roots = granted.roots_for(dir.path());
        assert_eq!(roots.len(), 1, "deduplicated: {roots:?}");
        // Session-wide: visible from any cwd.
        let other = tempfile::tempdir().unwrap();
        assert_eq!(granted.roots_for(other.path()).len(), 1);
    }

    #[test]
    fn the_filesystem_root_is_never_approvable() {
        let dir = tempfile::tempdir().unwrap();
        let granted = GrantedRoots::new(dir.path());
        granted.approve("/");
        assert!(granted.roots_for(dir.path()).is_empty());
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}
