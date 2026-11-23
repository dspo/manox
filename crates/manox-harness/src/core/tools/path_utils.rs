// Path utilities — path resolution and validation.
//
// Resolves paths relative to the working directory, handles `~` expansion,
// and validates that paths stay within allowed boundaries.

use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

use crate::tool::ToolContext;

/// Schema description shared by every FS/Bash tool's `cwd` property.
pub const CWD_SCHEMA_DOC: &str = "Optional working directory for this call as \
`[anchor, ...route]`. Omit it to run in the conversation's current directory — the same as \
`[\"\"]`. A non-empty first element re-anchors the conversation's default directory (a \
relative anchor resolves against the current one, so pass an absolute path to start from a \
known root). Remaining elements locate this call only, without re-anchoring. Prefer a single \
element with the target expressed in `path`.";

/// The instructional text for a present-but-malformed `cwd` argument — it
/// doubles as the model-facing how-to, and names omission as the default so
/// a model that guessed the shape wrong learns the cheap way out. Every
/// malformed shape funnels here: the tool's own parse, and the pipeline
/// rewriting the generic jsonschema message the schema's shape constraints
/// produce.
pub const CWD_HOWTO_DOC: &str = "invalid cwd: expected [anchor, ...route] — omit `cwd` to run \
in the conversation's current directory. The first element is the conversation's new anchor \
directory (\"\" keeps the current one; a relative anchor resolves against the current anchor, \
so use an absolute path to start from a known root), later elements locate this call without \
re-anchoring. Example: [\"~/projects/dspo/manox\", \"crates/manox-harness\"]";

/// Whether `cwd` was left out of the call: absent, or JSON `null` (what an
/// optional property becomes when a provider serializes its absence).
/// Omission resolves to the conversation's current directory, exactly like
/// the empty anchor `[""]`.
pub fn cwd_is_omitted(value: Option<&JsonValue>) -> bool {
    value.is_none_or(JsonValue::is_null)
}

/// Whether `value` is a present, well-formed `cwd` argument. The pipeline's
/// schema-failure path uses this to tell a cwd-caused validation failure
/// (present but malformed — rewritten to [`CWD_HOWTO_DOC`]) from a failure
/// in some other property (left as-is).
pub fn cwd_is_shaped(value: Option<&JsonValue>) -> bool {
    match value.and_then(|v| v.as_array()) {
        Some(items) => !items.is_empty() && items.iter().all(|v| v.is_string()),
        None => false,
    }
}

/// The parsed `cwd` argument: `[anchor, ...route]`.
struct CwdArg {
    /// First element: `""` keeps the current anchor; non-empty re-anchors the
    /// conversation's default directory.
    anchor: String,
    /// Remaining elements: joined onto the anchor to locate this call only.
    route: Vec<String>,
}

impl CwdArg {
    /// What an omitted `cwd` means: the conversation's current directory,
    /// with nothing re-anchored and nothing routed.
    fn inherited() -> Self {
        CwdArg {
            anchor: String::new(),
            route: Vec::new(),
        }
    }
}

/// Parse the `cwd` argument. Omission (see [`cwd_is_omitted`]) means the
/// conversation's current directory. A present value must be a non-empty
/// JSON array of strings; anything else (non-array, empty array, non-string
/// element) is rejected with the instructional text.
fn parse_cwd_arg(value: Option<&JsonValue>) -> Result<CwdArg, String> {
    if cwd_is_omitted(value) {
        return Ok(CwdArg::inherited());
    }
    let Some(items) = value.and_then(|v| v.as_array()) else {
        return Err(CWD_HOWTO_DOC.to_string());
    };
    let mut elements = items.iter();
    let anchor = elements
        .next()
        .and_then(|v| v.as_str())
        .ok_or_else(|| CWD_HOWTO_DOC.to_string())?;
    let route = elements
        .map(|v| v.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| CWD_HOWTO_DOC.to_string())?;
    Ok(CwdArg {
        anchor: anchor.to_string(),
        route,
    })
}

/// Resolve the effective working directory for one tool call without
/// advancing the sticky cwd — the read-only half of
/// [`resolve_effective_cwd`], for surfaces that must predict the call's
/// directory without disturbing it (the approval fence runs before the
/// tool's own resolution).
pub fn peek_effective_cwd(
    ctx: &dyn ToolContext,
    value: Option<&JsonValue>,
) -> Result<PathBuf, String> {
    Ok(resolve_dirs(ctx, value)?.1)
}

/// Resolve `(anchor_dir, call_dir)` for one `cwd` argument. `anchor_dir` is
/// `None` when the anchor element is `""` — the conversation's default
/// directory stays where it is. An omitted `cwd` parses as exactly that
/// shape, so omission and `[""]` cannot diverge.
///
/// The anchor resolves against the sticky cwd (the directory the last
/// non-empty anchor set), falling back to the tool context's baseline (the
/// session cwd); `~` expands to the home directory and a relative anchor
/// resolves against that base. Route elements join onto the anchor in order
/// with `PathBuf::join` semantics — an absolute element resets the base. The
/// anchor and the joined call directory must both exist: every consumer
/// (path joins, shell spawns) needs a real directory, and a missing target
/// (a removed worktree) must not poison the sticky cwd.
fn resolve_dirs(
    ctx: &dyn ToolContext,
    value: Option<&JsonValue>,
) -> Result<(Option<PathBuf>, PathBuf), String> {
    let arg = parse_cwd_arg(value)?;
    let tool_state = ctx.tool_state();
    let sticky = tool_state
        .sticky_cwd
        .lock()
        .expect("sticky cwd poisoned")
        .clone();
    let base = sticky.unwrap_or_else(|| ctx.cwd().to_path_buf());
    let anchor_dir = if arg.anchor.is_empty() {
        None
    } else {
        let expanded = expand_tilde(&arg.anchor);
        let path = Path::new(&expanded);
        let dir = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base.join(path)
        };
        if !dir.is_dir() {
            return Err(format!(
                "working directory does not exist: {}",
                dir.display()
            ));
        }
        Some(dir)
    };
    let mut call_dir = anchor_dir.clone().unwrap_or_else(|| base.clone());
    for element in &arg.route {
        let expanded = expand_tilde(element);
        let path = Path::new(&expanded);
        call_dir = if path.is_absolute() {
            path.to_path_buf()
        } else {
            call_dir.join(path)
        };
    }
    if !call_dir.is_dir() {
        return Err(format!(
            "working directory does not exist: {}",
            call_dir.display()
        ));
    }
    Ok((anchor_dir, call_dir))
}

/// Resolve the effective working directory for one tool call and advance the
/// session's sticky cwd to the call's anchor directory.
///
/// `cwd` is the array `[anchor, ...route]`: a non-empty anchor re-anchors the
/// sticky cwd (to the anchor directory itself — route elements only locate
/// this call); an empty anchor keeps it. The returned directory is where this
/// call runs (anchor joined with the route). See [`resolve_dirs`].
pub fn resolve_effective_cwd(
    ctx: &dyn ToolContext,
    value: Option<&JsonValue>,
) -> Result<PathBuf, String> {
    let (anchor_dir, call_dir) = resolve_dirs(ctx, value)?;
    if let Some(anchor_dir) = anchor_dir {
        *ctx.tool_state()
            .sticky_cwd
            .lock()
            .expect("sticky cwd poisoned") = Some(anchor_dir);
    }
    Ok(call_dir)
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

    fn cwd_arg(items: &[&str]) -> Option<serde_json::Value> {
        Some(serde_json::json!(items))
    }

    #[test]
    fn omitted_cwd_runs_in_the_conversation_directory() {
        let (base, work) = setup_dirs();
        let (ctx, state) = ctx_at(base.path());
        // Absent and JSON `null` are the same omission.
        assert_eq!(resolve_effective_cwd(&ctx, None).unwrap(), base.path());
        assert_eq!(
            resolve_effective_cwd(&ctx, Some(&serde_json::Value::Null)).unwrap(),
            base.path()
        );
        assert_eq!(sticky(&state), None, "omission never re-anchors");
        // Omission inherits the directory an earlier anchor established.
        resolve_effective_cwd(&ctx, cwd_arg(&[work.path().to_str().unwrap()]).as_ref()).unwrap();
        assert_eq!(resolve_effective_cwd(&ctx, None).unwrap(), work.path());
        // The read-only peek agrees without advancing anything.
        assert_eq!(
            peek_effective_cwd(&ctx, Some(&serde_json::Value::Null)).unwrap(),
            work.path()
        );
    }

    #[test]
    fn malformed_or_empty_cwd_is_rejected() {
        let (base, _work) = setup_dirs();
        let (ctx, state) = ctx_at(base.path());
        // Non-array.
        let err = resolve_effective_cwd(&ctx, Some(&serde_json::json!("/abs"))).unwrap_err();
        assert!(err.contains("invalid cwd"), "{err}");
        // Empty array.
        let err = resolve_effective_cwd(&ctx, Some(&serde_json::json!([]))).unwrap_err();
        assert!(err.contains("invalid cwd"), "{err}");
        // Non-string element.
        let err = resolve_effective_cwd(&ctx, Some(&serde_json::json!(["", 3]))).unwrap_err();
        assert!(err.contains("invalid cwd"), "{err}");
        assert_eq!(sticky(&state), None);
    }

    #[test]
    fn empty_anchor_keeps_the_base_and_never_advances_sticky() {
        let (base, _work) = setup_dirs();
        let (ctx, state) = ctx_at(base.path());
        let effective = resolve_effective_cwd(&ctx, cwd_arg(&[""]).as_ref()).unwrap();
        assert_eq!(effective, base.path());
        assert_eq!(sticky(&state), None, "an empty anchor never re-anchors");
        // Peek agrees and stays read-only.
        let peeked = peek_effective_cwd(&ctx, cwd_arg(&[""]).as_ref()).unwrap();
        assert_eq!(peeked, base.path());
    }

    #[test]
    fn single_element_anchor_reanchors_and_advances_sticky() {
        let (base, work) = setup_dirs();
        let (ctx, state) = ctx_at(base.path());
        let arg = cwd_arg(&[work.path().to_str().unwrap()]);
        let effective = resolve_effective_cwd(&ctx, arg.as_ref()).unwrap();
        assert_eq!(effective, work.path());
        assert_eq!(sticky(&state), Some(work.path().to_path_buf()));
        // The next call with an empty anchor inherits the advanced sticky.
        let inherited = resolve_effective_cwd(&ctx, cwd_arg(&[""]).as_ref()).unwrap();
        assert_eq!(inherited, work.path());
    }

    #[test]
    fn relative_anchor_resolves_against_sticky() {
        let (base, work) = setup_dirs();
        let sub = work.path().join("nested");
        std::fs::create_dir(&sub).unwrap();
        let (ctx, state) = ctx_at(base.path());
        resolve_effective_cwd(&ctx, cwd_arg(&[work.path().to_str().unwrap()]).as_ref()).unwrap();
        let effective = resolve_effective_cwd(&ctx, cwd_arg(&["nested"]).as_ref()).unwrap();
        assert_eq!(effective, sub);
        assert_eq!(sticky(&state), Some(sub));
    }

    #[test]
    fn route_locates_the_call_without_moving_sticky() {
        let (base, work) = setup_dirs();
        let nested = work.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let (ctx, state) = ctx_at(base.path());
        let arg = cwd_arg(&[work.path().to_str().unwrap(), "nested"]);
        let effective = resolve_effective_cwd(&ctx, arg.as_ref()).unwrap();
        assert_eq!(effective, nested, "the call runs in anchor + route");
        assert_eq!(
            sticky(&state),
            Some(work.path().to_path_buf()),
            "the sticky advances to the anchor, not the route directory"
        );
    }

    #[test]
    fn empty_anchor_with_absolute_route_keeps_the_anchor() {
        let (base, work) = setup_dirs();
        let (ctx, state) = ctx_at(base.path());
        let arg = cwd_arg(&["", work.path().to_str().unwrap()]);
        let effective = resolve_effective_cwd(&ctx, arg.as_ref()).unwrap();
        assert_eq!(effective, work.path());
        assert_eq!(sticky(&state), None, "an empty anchor never re-anchors");
    }

    #[test]
    fn absolute_route_element_resets_the_base() {
        let (base, work) = setup_dirs();
        let (ctx, state) = ctx_at(base.path());
        let arg = cwd_arg(&[base.path().to_str().unwrap(), work.path().to_str().unwrap()]);
        let effective = resolve_effective_cwd(&ctx, arg.as_ref()).unwrap();
        assert_eq!(effective, work.path(), "the absolute route resets the base");
        assert_eq!(sticky(&state), Some(base.path().to_path_buf()));
    }

    #[test]
    fn nonempty_anchor_advances_even_with_an_absolute_route() {
        let (base, work) = setup_dirs();
        let anchor = base.path().join("anchor");
        std::fs::create_dir(&anchor).unwrap();
        let (ctx, state) = ctx_at(base.path());
        let arg = cwd_arg(&["anchor", work.path().to_str().unwrap()]);
        let effective = resolve_effective_cwd(&ctx, arg.as_ref()).unwrap();
        assert_eq!(effective, work.path());
        assert_eq!(sticky(&state), Some(anchor), "the anchor still re-anchors");
    }

    #[test]
    fn missing_directory_is_rejected_and_keeps_sticky() {
        let (base, work) = setup_dirs();
        let (ctx, state) = ctx_at(base.path());
        // A missing anchor.
        let gone = work.path().join("gone");
        let err =
            resolve_effective_cwd(&ctx, cwd_arg(&[gone.to_str().unwrap()]).as_ref()).unwrap_err();
        assert!(err.contains("working directory does not exist"), "{err}");
        // A missing route segment.
        let err = resolve_effective_cwd(&ctx, cwd_arg(&["", "gone"]).as_ref()).unwrap_err();
        assert!(err.contains("working directory does not exist"), "{err}");
        // A failed resolution must not advance the sticky.
        assert_eq!(sticky(&state), None);
        let effective = resolve_effective_cwd(&ctx, cwd_arg(&[""]).as_ref()).unwrap();
        assert_eq!(effective, base.path());
    }
}
