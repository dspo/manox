// Path utilities — path resolution and validation.
//
// Resolves paths relative to the working directory, handles `~` expansion,
// and validates that paths stay within allowed boundaries.

use std::path::{Path, PathBuf};

use crate::tool::ToolContext;

/// Resolve the effective working directory for one tool call without
/// advancing the sticky cwd — the read-only half of
/// [`resolve_effective_cwd`], for surfaces that must predict the call's
/// directory without disturbing it (the approval fence runs before the
/// tool's own resolution).
pub fn peek_effective_cwd(
    ctx: &dyn ToolContext,
    explicit: Option<&str>,
) -> Result<PathBuf, String> {
    let tool_state = ctx.tool_state();
    let sticky = tool_state
        .sticky_cwd
        .lock()
        .expect("sticky cwd poisoned")
        .clone();
    let base = sticky.unwrap_or_else(|| ctx.cwd().to_path_buf());
    let effective = match explicit.map(expand_tilde) {
        Some(cwd) => {
            let path = Path::new(&cwd);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                base.join(path)
            }
        }
        None => base,
    };
    if !effective.is_dir() {
        return Err(format!(
            "working directory does not exist: {}",
            effective.display()
        ));
    }
    Ok(effective)
}

/// Resolve the effective working directory for one tool call and advance the
/// session's sticky cwd to it.
///
/// Resolution chain: an explicit `cwd` argument → the sticky cwd (the
/// directory the last tool call ran in) → the tool context's baseline (the
/// session cwd). An explicit relative `cwd` resolves against the sticky cwd
/// (or the session cwd before any tool call moved it). `~` expands to the
/// home directory. A resolved directory that does not exist is an error —
/// every consumer (path joins, shell spawns) needs a real directory, and a
/// missing target (a removed worktree) must not poison the sticky cwd.
pub fn resolve_effective_cwd(
    ctx: &dyn ToolContext,
    explicit: Option<&str>,
) -> Result<PathBuf, String> {
    let effective = peek_effective_cwd(ctx, explicit)?;
    *ctx.tool_state()
        .sticky_cwd
        .lock()
        .expect("sticky cwd poisoned") = Some(effective.clone());
    Ok(effective)
}

/// Resolve a potentially relative path against the working directory.
///
/// Handles:
/// - Absolute paths (returned as-is after canonicalization)
/// - `~` home directory expansion
/// - Relative paths (resolved against `cwd`)
pub fn resolve_path(path_str: &str, cwd: &Path) -> PathBuf {
    let expanded = expand_tilde(path_str);
    let path = Path::new(&expanded);

    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Expand `~` to the user's home directory.
fn expand_tilde(path_str: &str) -> String {
    if path_str.starts_with("~/") {
        if let Some(home) = dirs_home() {
            return home.to_string_lossy().to_string() + &path_str[1..];
        }
    } else if path_str == "~"
        && let Some(home) = dirs_home()
    {
        return home.to_string_lossy().to_string();
    }
    path_str.to_string()
}

/// Get the user's home directory.
fn dirs_home() -> Option<PathBuf> {
    std::env::var("HOME").ok().map(PathBuf::from).or({
        #[cfg(target_os = "windows")]
        {
            std::env::var("USERPROFILE").ok().map(PathBuf::from)
        }
        #[cfg(not(target_os = "windows"))]
        {
            None
        }
    })
}

/// Check whether a path is within an allowed directory.
///
/// Returns true if the path is under `allowed_root` (or equal to it).
pub fn is_within(path: &Path, allowed_root: &Path) -> bool {
    // Canonicalize both paths for comparison.
    let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let canonical_root = allowed_root
        .canonicalize()
        .unwrap_or_else(|_| allowed_root.to_path_buf());

    canonical_path.starts_with(&canonical_root)
}

/// Resolve a path and validate it's within the working directory.
///
/// Returns an error if the resolved path escapes the working directory
/// (e.g., via `../` traversal).
pub fn resolve_safe(path_str: &str, cwd: &Path) -> Result<PathBuf, String> {
    let resolved = resolve_path(path_str, cwd);

    // Canonicalize to resolve symlinks and `..`.
    let canonical = resolved.canonicalize().unwrap_or_else(|_| resolved.clone());

    let cwd_canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());

    if !canonical.starts_with(&cwd_canonical) {
        return Err(format!(
            "Path escapes working directory: {} → {}",
            path_str,
            canonical.display()
        ));
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{LocalToolContext, ToolState};

    fn ctx_at(dir: &Path) -> (LocalToolContext, std::sync::Arc<ToolState>) {
        let state = std::sync::Arc::new(ToolState::new());
        let env = std::sync::Arc::new(crate::env::TokioExecutionEnv::new(dir.to_path_buf()));
        (
            LocalToolContext::new(env, dir.to_path_buf(), std::sync::Arc::clone(&state)),
            state,
        )
    }

    fn sticky(state: &ToolState) -> Option<PathBuf> {
        state.sticky_cwd.lock().unwrap().clone()
    }

    #[test]
    fn test_resolve_absolute_path() {
        let resolved = resolve_path("/usr/bin", Path::new("/tmp"));
        assert_eq!(resolved, PathBuf::from("/usr/bin"));
    }

    #[test]
    fn test_resolve_relative_path() {
        let resolved = resolve_path("src/main.rs", Path::new("/project"));
        assert_eq!(resolved, PathBuf::from("/project/src/main.rs"));
    }

    #[test]
    fn test_is_within() {
        assert!(is_within(
            Path::new("/project/src/main.rs"),
            Path::new("/project")
        ));
    }

    #[test]
    fn test_is_not_within() {
        assert!(!is_within(Path::new("/etc/passwd"), Path::new("/project")));
    }

    fn setup_dirs() -> (tempfile::TempDir, tempfile::TempDir) {
        let base = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir_in(base.path()).unwrap();
        (base, work)
    }

    #[test]
    fn effective_cwd_starts_at_session_cwd_without_sticky() {
        let (base, _work) = setup_dirs();
        let (ctx, state) = ctx_at(base.path());
        let effective = resolve_effective_cwd(&ctx, None).unwrap();
        assert_eq!(effective, base.path());
        // The first resolution advances the sticky to the session cwd.
        assert_eq!(sticky(&state), Some(base.path().to_path_buf()));
    }

    #[test]
    fn effective_cwd_explicit_absolute_overrides_and_advances_sticky() {
        let (base, work) = setup_dirs();
        let (ctx, state) = ctx_at(base.path());
        let effective = resolve_effective_cwd(&ctx, Some(work.path().to_str().unwrap())).unwrap();
        assert_eq!(effective, work.path());
        assert_eq!(sticky(&state), Some(work.path().to_path_buf()));
        // The next call without an argument inherits the advanced sticky.
        let inherited = resolve_effective_cwd(&ctx, None).unwrap();
        assert_eq!(inherited, work.path());
    }

    #[test]
    fn effective_cwd_explicit_relative_resolves_against_sticky() {
        let (base, work) = setup_dirs();
        let sub = work.path().join("nested");
        std::fs::create_dir(&sub).unwrap();
        let (ctx, state) = ctx_at(base.path());
        resolve_effective_cwd(&ctx, Some(work.path().to_str().unwrap())).unwrap();
        let effective = resolve_effective_cwd(&ctx, Some("nested")).unwrap();
        assert_eq!(effective, sub);
        assert_eq!(sticky(&state), Some(sub));
    }

    #[test]
    fn effective_cwd_rejects_missing_directory_and_keeps_sticky() {
        let (base, work) = setup_dirs();
        let (ctx, state) = ctx_at(base.path());
        let gone = work.path().join("gone");
        let err = resolve_effective_cwd(&ctx, Some(gone.to_str().unwrap())).unwrap_err();
        assert!(err.contains("working directory does not exist"), "{err}");
        // A failed resolution must not advance the sticky.
        assert_eq!(sticky(&state), None);
        let effective = resolve_effective_cwd(&ctx, None).unwrap();
        assert_eq!(effective, base.path());
    }
}
