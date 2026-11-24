// Path utilities — path resolution and validation.
//
// Resolves paths relative to the working directory, handles `~` expansion,
// and validates that paths stay within allowed boundaries.

use std::path::{Path, PathBuf};

use serde_json::Value as JsonValue;

use crate::tool::ToolContext;

/// Schema description shared by every FS/Bash tool's `cwd` property.
pub const CWD_SCHEMA_DOC: &str = "Optional working directory for this call as \
`[dir, ...subdirs]`. Omit it to run in the conversation's directory — the same as \
`[\"\"]`. Elements join in order (`PathBuf::join` semantics): an absolute element \
resets the base, a relative first element resolves against the conversation's \
directory. The argument locates this call only — it never changes the \
conversation's directory. Prefer a single element with the target expressed in \
`path`.";

/// The instructional text for a present-but-malformed `cwd` argument — it
/// doubles as the model-facing how-to, and names omission as the default so
/// a model that guessed the shape wrong learns the cheap way out. Every
/// malformed shape funnels here: the tool's own parse, and the pipeline
/// rewriting the generic jsonschema message the schema's shape constraints
/// produce.
pub const CWD_HOWTO_DOC: &str = "invalid cwd: expected [dir, ...subdirs] — omit `cwd` to run \
in the conversation's directory. Elements join in order (an absolute element resets the \
base; a relative first element resolves against the conversation's directory). The \
argument locates this call only and never changes the conversation's directory. \
Example: [\"crates/manox-harness\"]";

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

/// The parsed `cwd` argument: `[dir, ...subdirs]`.
struct CwdArg {
    /// First element: `""` means the conversation's directory; a relative
    /// element resolves against it; an absolute element replaces it.
    dir: String,
    /// Remaining elements: joined onto the first in order.
    subdirs: Vec<String>,
}

impl CwdArg {
    /// What an omitted `cwd` means: the conversation's directory, nothing
    /// joined.
    fn inherited() -> Self {
        CwdArg {
            dir: String::new(),
            subdirs: Vec::new(),
        }
    }
}

/// Parse the `cwd` argument. Omission (see [`cwd_is_omitted`]) means the
/// conversation's directory. A present value must be a non-empty JSON array
/// of strings; anything else (non-array, empty array, non-string element) is
/// rejected with the instructional text.
fn parse_cwd_arg(value: Option<&JsonValue>) -> Result<CwdArg, String> {
    if cwd_is_omitted(value) {
        return Ok(CwdArg::inherited());
    }
    let Some(items) = value.and_then(|v| v.as_array()) else {
        return Err(CWD_HOWTO_DOC.to_string());
    };
    let mut elements = items.iter();
    let dir = elements
        .next()
        .and_then(|v| v.as_str())
        .ok_or_else(|| CWD_HOWTO_DOC.to_string())?;
    let subdirs = elements
        .map(|v| v.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| CWD_HOWTO_DOC.to_string())?;
    Ok(CwdArg {
        dir: dir.to_string(),
        subdirs,
    })
}

/// Resolve the working directory one tool call runs in. Pure: the result
/// derives from the call's arguments and the conversation's directory alone
/// — nothing persists across calls, so concurrent calls cannot interfere and
/// the model can always predict where its call runs. The approval fence and
/// the tools share this one resolution.
///
/// `cwd` is the array `[dir, ...subdirs]`: elements join in order with
/// `PathBuf::join` semantics — an absolute element resets the base, a
/// relative first element resolves against the conversation's directory,
/// `~` expands to the home directory. The joined directory must exist:
/// every consumer (path joins, shell spawns) needs a real directory.
pub fn resolve_effective_cwd(
    ctx: &dyn ToolContext,
    value: Option<&JsonValue>,
) -> Result<PathBuf, String> {
    let arg = parse_cwd_arg(value)?;
    let base = ctx.cwd().to_path_buf();
    let head = if arg.dir.is_empty() {
        base
    } else {
        let expanded = expand_tilde(&arg.dir);
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
        dir
    };
    let mut call_dir = head;
    for element in &arg.subdirs {
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

/// Nearest enclosing git repository root of `target`: the first ancestor
/// carrying a `.git` entry. The entry may be a directory (a normal repo) or
/// a file (a linked worktree) — both count. Lexical walk, no git binary.
fn git_repo_root(target: &Path) -> Option<PathBuf> {
    target
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The FS tools the repo-path nudge governs — every tool whose `path`
/// argument is resolved against the call's working directory.
fn is_fs_path_tool(name: &str) -> bool {
    matches!(name, "Read" | "Write" | "Edit" | "Grep" | "Glob" | "Ls")
}

/// Model-facing nudge for an absolute `path` argument that resolves inside a
/// git repository or worktree: teaches the repo-root-anchored `cwd` form as
/// the cheaper carrier. Two-tier — the first qualifying call in a session
/// carries the full how-to, every later one a one-line reminder with the
/// concrete rewrite. A non-empty `cwd` anchor means the model already
/// carries locations the intended way and is never nudged.
pub fn repo_path_nudge(tool_name: &str, args: &JsonValue, ctx: &dyn ToolContext) -> String {
    if !is_fs_path_tool(tool_name) {
        return String::new();
    }
    if let Some(items) = args.get("cwd").and_then(|v| v.as_array())
        && items
            .first()
            .and_then(|v| v.as_str())
            .is_some_and(|anchor| !anchor.is_empty())
    {
        return String::new();
    }
    let Some(path_str) = args.get("path").and_then(|v| v.as_str()) else {
        return String::new();
    };
    let expanded = expand_tilde(path_str);
    let target = PathBuf::from(&expanded);
    if !target.is_absolute() {
        return String::new();
    }
    let Some(root) = git_repo_root(&target) else {
        return String::new();
    };
    let Ok(rel) = target.strip_prefix(&root) else {
        return String::new();
    };
    // The repo root itself has no shorter form — anchoring adds nothing.
    if rel.as_os_str().is_empty() {
        return String::new();
    }
    let root_str = root.to_string_lossy();
    let rel_str = rel.to_string_lossy();
    if root_str.contains('"') || rel_str.contains('"') {
        return String::new(); // the rewrite example would not be quotable
    }
    let nf = &ctx.tool_state().nudge_flags;
    if crate::core::tool::nudge::mark(nf, crate::core::tool::nudge::FS_REPO_PATH) {
        format!(
            "[harness] prefer a repo-relative path: {{\"cwd\": [\"{root_str}\"], \"path\": \"{rel_str}\"}}."
        )
    } else {
        format!(
            "[harness] tip: \"{path_str}\" lives inside the git repository at \"{root_str}\" — \
             prefer paths relative to the repository root: \
             {{\"cwd\": [\"{root_str}\"], \"path\": \"{rel_str}\"}}."
        )
    }
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
        let (ctx, _state) = ctx_at(base.path());
        // Absent and JSON `null` are the same omission.
        assert_eq!(resolve_effective_cwd(&ctx, None).unwrap(), base.path());
        assert_eq!(
            resolve_effective_cwd(&ctx, Some(&serde_json::Value::Null)).unwrap(),
            base.path()
        );
        // A prior call's directory never persists: resolution is a pure
        // function of the arguments and the conversation's directory.
        resolve_effective_cwd(&ctx, cwd_arg(&[work.path().to_str().unwrap()]).as_ref()).unwrap();
        assert_eq!(resolve_effective_cwd(&ctx, None).unwrap(), base.path());
    }

    #[test]
    fn malformed_or_empty_cwd_is_rejected() {
        let (base, _work) = setup_dirs();
        let (ctx, _state) = ctx_at(base.path());
        // Non-array.
        let err = resolve_effective_cwd(&ctx, Some(&serde_json::json!("/abs"))).unwrap_err();
        assert!(err.contains("invalid cwd"), "{err}");
        // Empty array.
        let err = resolve_effective_cwd(&ctx, Some(&serde_json::json!([]))).unwrap_err();
        assert!(err.contains("invalid cwd"), "{err}");
        // Non-string element.
        let err = resolve_effective_cwd(&ctx, Some(&serde_json::json!(["", 3]))).unwrap_err();
        assert!(err.contains("invalid cwd"), "{err}");
    }

    #[test]
    fn empty_dir_element_means_the_conversation_directory() {
        let (base, _work) = setup_dirs();
        let (ctx, _state) = ctx_at(base.path());
        let effective = resolve_effective_cwd(&ctx, cwd_arg(&[""]).as_ref()).unwrap();
        assert_eq!(effective, base.path());
    }

    #[test]
    fn single_element_locates_this_call_only() {
        let (base, work) = setup_dirs();
        let (ctx, _state) = ctx_at(base.path());
        let arg = cwd_arg(&[work.path().to_str().unwrap()]);
        let effective = resolve_effective_cwd(&ctx, arg.as_ref()).unwrap();
        assert_eq!(effective, work.path());
        // The next call is back at the conversation's directory.
        let next = resolve_effective_cwd(&ctx, cwd_arg(&[""]).as_ref()).unwrap();
        assert_eq!(next, base.path());
    }

    #[test]
    fn relative_first_element_resolves_against_the_conversation_directory() {
        let (base, _work) = setup_dirs();
        let sub = base.path().join("nested");
        std::fs::create_dir(&sub).unwrap();
        let (ctx, _state) = ctx_at(base.path());
        let effective = resolve_effective_cwd(&ctx, cwd_arg(&["nested"]).as_ref()).unwrap();
        assert_eq!(effective, sub);
    }

    #[test]
    fn subdirs_join_onto_the_first_element() {
        let (base, work) = setup_dirs();
        let nested = work.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let (ctx, _state) = ctx_at(base.path());
        let arg = cwd_arg(&[work.path().to_str().unwrap(), "nested"]);
        let effective = resolve_effective_cwd(&ctx, arg.as_ref()).unwrap();
        assert_eq!(effective, nested);
    }

    #[test]
    fn empty_head_with_absolute_second_element_still_locates() {
        let (base, work) = setup_dirs();
        let (ctx, _state) = ctx_at(base.path());
        let arg = cwd_arg(&["", work.path().to_str().unwrap()]);
        let effective = resolve_effective_cwd(&ctx, arg.as_ref()).unwrap();
        assert_eq!(effective, work.path());
    }

    #[test]
    fn absolute_element_resets_the_base() {
        let (base, work) = setup_dirs();
        let (ctx, _state) = ctx_at(base.path());
        let arg = cwd_arg(&[base.path().to_str().unwrap(), work.path().to_str().unwrap()]);
        let effective = resolve_effective_cwd(&ctx, arg.as_ref()).unwrap();
        assert_eq!(effective, work.path());
    }

    #[test]
    fn missing_directory_is_rejected() {
        let (base, work) = setup_dirs();
        let (ctx, _state) = ctx_at(base.path());
        // A missing first element.
        let gone = work.path().join("gone");
        let err =
            resolve_effective_cwd(&ctx, cwd_arg(&[gone.to_str().unwrap()]).as_ref()).unwrap_err();
        assert!(err.contains("working directory does not exist"), "{err}");
        // A missing joined segment.
        let err = resolve_effective_cwd(&ctx, cwd_arg(&["", "gone"]).as_ref()).unwrap_err();
        assert!(err.contains("working directory does not exist"), "{err}");
    }

    #[test]
    fn repo_path_nudge_full_then_short() {
        let (base, _work) = setup_dirs();
        let repo = base.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let deep = repo.join("crates").join("app").join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("lib.rs"), "x").unwrap();
        let target = deep.join("lib.rs");
        let abs = target.to_str().unwrap();
        let (ctx, _state) = ctx_at(base.path());
        let args = serde_json::json!({ "path": abs });
        let first = repo_path_nudge("Read", &args, &ctx);
        assert!(first.contains("lives inside the git repository"), "{first}");
        assert!(
            first.contains("prefer paths relative to the repository root"),
            "{first}"
        );
        let second = repo_path_nudge("Read", &args, &ctx);
        assert!(second.contains("prefer a repo-relative path"), "{second}");
        assert!(!second.contains("lives inside"), "{second}");
    }

    #[test]
    fn repo_path_nudge_negative_cases() {
        let (base, _work) = setup_dirs();
        let plain = base.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        std::fs::write(plain.join("f.txt"), "x").unwrap();
        let (ctx, _state) = ctx_at(base.path());
        // Not a repo.
        let args = serde_json::json!({ "path": plain.join("f.txt").to_str().unwrap() });
        assert_eq!(repo_path_nudge("Read", &args, &ctx), "");
        // Relative path.
        let rel = serde_json::json!({ "path": "f.txt" });
        assert_eq!(repo_path_nudge("Read", &rel, &ctx), "");
        // Non-FS tool.
        assert_eq!(repo_path_nudge("Bash", &args, &ctx), "");
        // Non-empty anchor: the model already carries the directory itself.
        let repo = base.path().join("repo2");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join("f.txt"), "x").unwrap();
        let anchored = serde_json::json!({
            "path": repo.join("f.txt").to_str().unwrap(),
            "cwd": [repo.to_str().unwrap()],
        });
        assert_eq!(repo_path_nudge("Read", &anchored, &ctx), "");
        // The repo root itself has no shorter form.
        let root_only = serde_json::json!({ "path": repo.to_str().unwrap() });
        assert_eq!(repo_path_nudge("Read", &root_only, &ctx), "");
    }
}
