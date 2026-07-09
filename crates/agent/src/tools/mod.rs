//! Built-in tool registry + shared helpers for the per-tool modules.
//!
//! Per-tool implementations live in sibling files (`read_file.rs`, `write_file.rs`,
//! `edit_file.rs`, `list_directory.rs`, `grep.rs`, `glob.rs`, `bash.rs`, `agent.rs`,
//! `ask_user.rs`, `monitor.rs`, `self_info.rs`, `skill.rs`). This module holds
//! the path/truncation helpers they share, plus the default registry assembly.
//!
//! `requires_approval` gates the approval overlay: write_file / edit_file /
//! ask_user always require it; `bash` requires it on `unsandboxed: true`
//! escalation or when no OS sandbox backend is available (see [`bash`]).

pub mod agent;
pub mod ask_user;
pub mod bash;
pub mod edit_file;
pub mod file_lock;
pub mod glob;
pub mod grep;
pub mod list_directory;
pub mod monitor;
pub mod read_file;
pub mod self_info;
pub mod skill;
pub mod worktree;
pub mod write_file;

use gpui::{App, AppContext as _, Task, WeakEntity};
use schemars::JsonSchema;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::thread::Thread;
use crate::tool::{AnyAgentTool, ToolRegistry};

// ─── shared helpers ───────────────────────────────────────────────────────

/// Convert a schemars schema to a `serde_json::Value`.
pub(crate) fn schema<T: JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("schema serialization")
}

/// Bridge a tokio task back to a gpui background `Task` via `async_channel`.
pub(crate) fn bridge_tokio<F, R>(cx: &mut App, fut: F) -> Task<Result<String, String>>
where
    F: std::future::Future<Output = Result<R, anyhow::Error>> + Send + 'static,
    R: std::fmt::Display + Send + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    crate::runtime::handle().spawn(async move {
        let result = fut.await.map(|v| v.to_string()).map_err(|e| e.to_string());
        let _ = tx.send(result).await;
    });
    cx.background_spawn(async move {
        rx.recv()
            .await
            .map_err(|_| "tool cancelled".to_string())
            .and_then(|r| r)
    })
}

// ─── path resolution ──────────────────────────────────────────────────────

/// Resolve a tool input path against the thread cwd.
///
/// Absolute paths are returned as-is; relative paths are joined onto `cwd`.
/// The result feeds both FS ops and hashline snapshot keys, so `read_file` and
/// `edit_file` stay consistent regardless of how the model spelled the path.
/// `cwd` is absolute (set by `Thread::set_project`), so a relative input always
/// yields an absolute path — never the process cwd. `~` is NOT expanded (callers
/// spell absolute home paths or `set_project` to a resolved dir); the path is
/// also not canonicalized, so `.`/`..` segments stay literal to keep snapshot
/// keys a stable string.
pub(crate) fn resolve_path<P: AsRef<Path>>(input: P, cwd: &Path) -> PathBuf {
    debug_assert!(cwd.is_absolute(), "resolve_path cwd must be absolute");
    let p = input.as_ref();
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Resolve a write target and enforce the sandbox write confinement: the path
/// must fall under the project root or temp dir, and must not be under a
/// protected path (`.git`). Cross-platform pure-Rust check, independent of
/// whether the seatbelt backend itself is available — FS tools don't go
/// through bash, so seatbelt never covers them, and this is the hard layer.
///
/// The model escalates a write outside the writable set by routing through
/// `bash` with `unsandboxed: true` (which triggers user approval); the error
/// message says so.
pub(crate) fn resolve_path_for_write<P: AsRef<Path>>(
    input: P,
    cwd: &Path,
    policy: &crate::sandbox::SandboxPolicy,
) -> Result<PathBuf, String> {
    let path = resolve_path(input, cwd);
    if policy.is_protected(&path) {
        return Err(format!(
            "Write blocked by sandbox (`.git`): {}. To write to a protected path, set `unsandboxed: true` in the bash tool and pass user approval.",
            path.display()
        ));
    }
    if !policy.is_writable(&path) {
        return Err(format!(
            "Write outside project root and temp dir: {}. To write outside the project root, set `unsandboxed: true` in the bash tool and pass user approval.",
            path.display()
        ));
    }
    Ok(path)
}

// ─── truncation ───────────────────────────────────────────────────────────

/// Tool output that may have exceeded a byte cap, plus enough metadata to
/// render a uniform `⚠`-prefixed advisory.
///
/// Bash reaches this state via `CaptureBuffer::take` (streaming capture that
/// already knows the dropped tail size); other tools reach it via
/// [`truncate_output`], which cuts on a UTF-8 boundary. Both produce a
/// `TruncatedText` so the rendered notice — `⚠ Output too long (N bytes total,
/// showing first M)` plus a per-tool narrow hint plus `do not speculate about
/// the truncated content` — is identical across tools, matching the
/// `Command output too long. The first N bytes:` format.
pub(crate) struct TruncatedText<'a> {
    text: &'a str,
    truncated: bool,
    cap: usize,
    dropped: usize,
}

impl<'a> TruncatedText<'a> {
    /// Wrap already-truncated text where the caller knows the byte cap and the
    /// dropped tail size. `dropped == 0` means no truncation occurred.
    pub(crate) fn new(text: &'a str, cap: usize, dropped: usize) -> Self {
        Self {
            text,
            truncated: dropped > 0,
            cap,
            dropped,
        }
    }

    /// Render with the uniform advisory. `hint` is folded into one line before
    /// the "do not guess" directive, e.g. "retry with a narrower command
    /// (`| head`, `LIMIT`, tighten the pattern)".
    pub(crate) fn render(&self, hint: &str) -> String {
        if !self.truncated {
            return self.text.to_string();
        }
        let total = self.cap + self.dropped;
        format!(
            "⚠ Output too long ({total} bytes total, showing first {cap}). {hint} — do not speculate about the truncated content.\n\n{text}",
            cap = self.cap,
            text = self.text,
        )
    }
}

/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary, returning a
/// `TruncatedText` that knows how many bytes were dropped. For tools without a
/// streaming capture buffer. Currently exercised by its own tests; kept for the
/// next non-streaming tool that needs it.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn truncate_output(s: &str, max: usize) -> TruncatedText<'_> {
    if s.len() <= max {
        return TruncatedText::new(s, max, 0);
    }
    // Walk char boundaries; include each char whose end offset fits in `max`.
    let mut end = 0;
    for (i, c) in s.char_indices() {
        let next = i + c.len_utf8();
        if next > max {
            break;
        }
        end = next;
    }
    TruncatedText::new(&s[..end], max, s.len() - end)
}

// ─── Default registry ─────────────────────────────────────────────────────

/// The built-in tools as `AnyAgentTool` (no `agent` tool). Shared by the main
/// registry and sub-agent registries (which filter by name). No `Thread`
/// reference: every base tool is stateless w.r.t. its owner — runtime identity
/// flows in through the per-call `ToolContext` snapshot, so the dependency
/// direction stays `Thread → tools`.
///
/// Derives the write-confinement policy from `cwd` via `for_project`. For a
/// worktree-active thread, use [`base_tools_with_policy`] with a
/// worktree-aware policy so the bound repo's `.git` and network open up.
#[allow(dead_code)] // convenience constructor; the live paths use _with_policy
pub(crate) fn base_tools(cwd: Arc<PathBuf>) -> Vec<AnyAgentTool> {
    let sandbox = crate::sandbox::SandboxPolicy::for_project(cwd.as_ref());
    base_tools_with_policy(cwd, sandbox)
}

/// Same as [`base_tools`] but with an explicit sandbox policy. The worktree
/// path passes a `with_worktree` / `for_worktree` policy so git ops and network
/// are admitted; the default path delegates to [`base_tools`].
pub(crate) fn base_tools_with_policy(
    cwd: Arc<PathBuf>,
    sandbox: crate::sandbox::SandboxPolicy,
) -> Vec<AnyAgentTool> {
    vec![
        Arc::new(read_file::ReadFileTool { cwd: cwd.clone() }) as AnyAgentTool,
        Arc::new(write_file::WriteFileTool {
            cwd: cwd.clone(),
            sandbox: sandbox.clone(),
        }),
        Arc::new(edit_file::EditFileTool {
            cwd: cwd.clone(),
            sandbox: sandbox.clone(),
        }),
        Arc::new(list_directory::ListDirectoryTool { cwd: cwd.clone() }),
        Arc::new(bash::BashTool::new(cwd.as_ref().clone(), sandbox.clone())),
        Arc::new(grep::GrepTool { cwd: cwd.clone() }),
        Arc::new(glob::GlobTool { cwd: cwd.clone() }),
        Arc::new(ask_user::AskUserQuestionTool),
        Arc::new(skill::SkillTool),
    ]
}

/// Build the main-thread `ToolRegistry`: the built-in tools plus the
/// `agent` sub-agent tool, `self_info`, `monitor`, the `enter_worktree` /
/// `exit_worktree` harness tools, and MCP tools. `parent` is the owning
/// `Thread` so the `agent` and worktree tools can route bubbled-up
/// authorizations and mutate thread cwd.
///
/// Sub-agents do not use this — they build their own filtered registry from
/// [`base_tools`] (plus their own `agent` tool), so `agent` / `self_info` /
/// `monitor` / worktree / MCP stay main-thread-only.
pub fn main_registry(cwd: PathBuf, parent: WeakEntity<Thread>) -> ToolRegistry {
    let sandbox = crate::sandbox::SandboxPolicy::for_project(&cwd);
    main_registry_with_policy(cwd, sandbox, parent)
}

/// Same as [`main_registry`] but with an explicit sandbox policy. The worktree
/// entry/exit path rebuilds the registry with a `with_worktree` policy so the
/// whole tool set re-derives path confinement against the active worktree.
pub fn main_registry_with_policy(
    cwd: PathBuf,
    sandbox: crate::sandbox::SandboxPolicy,
    parent: WeakEntity<Thread>,
) -> ToolRegistry {
    let cwd = Arc::new(cwd);
    let mut reg = ToolRegistry::new();
    for tool in base_tools_with_policy(cwd.clone(), sandbox) {
        reg.register(tool);
    }
    reg.register(Arc::new(agent::SpawnAgentTool::new(cwd, 0, parent.clone())) as AnyAgentTool);
    reg.register(self_info::new());
    // `monitor` is main-thread-only (not in `base_tools`, so sub-agents do not
    // get it): streaming a long-running command is a top-level orchestration
    // concern, and like `agent` it should not nest into sub-agent contexts.
    reg.register(Arc::new(monitor::MonitorTool) as AnyAgentTool);
    // Worktree harness tools: main-thread-only, they mutate the owning
    // Thread's cwd and rebuild its tool registry on enter/exit.
    reg.register(Arc::new(worktree::EnterWorktreeTool::new(parent.clone())) as AnyAgentTool);
    reg.register(Arc::new(worktree::ExitWorktreeTool::new(parent.clone())) as AnyAgentTool);

    // Team coordination tools (shared by the leader and every worker member).
    // Registering here is a one-time tool-spec fingerprint change — stable
    // across turns thereafter, so the provider prefix cache settles once.
    for tool in crate::team::tools::shared_tools() {
        reg.register(tool);
    }
    // Leader-only team-management tools: form / grow / disband a peer team.
    // Never registered on worker members (only the leader manages teams).
    for tool in crate::team::tools::leader_tools(parent.clone()) {
        reg.register(tool);
    }

    // Append MCP tools discovered at startup from `mcp.toml` plus each
    // installed plugin's `.mcp.json`. The registry is process-global; `try_global`
    // is `None` only before `agent::init` (e.g. unit tests that build a registry
    // directly).
    if let Some(mcp) = crate::mcp::registry::try_global() {
        for tool in mcp.tools() {
            reg.register(tool.clone());
        }
    }
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_tools_has_nine_tools() {
        let tools = base_tools(Arc::new(PathBuf::from(".")));
        assert_eq!(tools.len(), 9);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"list_directory"));
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"glob"));
        assert!(names.contains(&"AskUserQuestion"));
        assert!(names.contains(&"skill"));
        // The sub-agent tool is registered by `main_registry`, not `base_tools`.
        assert!(!names.contains(&"agent"));
    }

    #[test]
    fn input_schemas_are_objects() {
        // Exercises the public `input_schema()` surface (the `schema::<T>()`
        // helper runs under the hood inside each tool); the Input structs are
        // private to their submodules, so drive this through the trait method.
        let tools = base_tools(Arc::new(PathBuf::from(".")));
        for t in &tools {
            assert_eq!(t.input_schema()["type"], "object", "{} schema", t.name());
        }
    }

    #[test]
    fn resolve_path_passes_absolute_through() {
        let cwd = Path::new("/Users/someone/proj");
        assert_eq!(
            resolve_path("/Users/someone/proj/CLAUDE.md", cwd),
            PathBuf::from("/Users/someone/proj/CLAUDE.md")
        );
        assert_eq!(
            resolve_path("/tmp/elsewhere.txt", cwd),
            PathBuf::from("/tmp/elsewhere.txt")
        );
    }

    #[test]
    fn resolve_path_joins_relative_onto_cwd() {
        let cwd = Path::new("/Users/someone/proj");
        assert_eq!(
            resolve_path("CLAUDE.md", cwd),
            PathBuf::from("/Users/someone/proj/CLAUDE.md")
        );
        assert_eq!(
            resolve_path("./src/lib.rs", cwd),
            PathBuf::from("/Users/someone/proj/src/lib.rs")
        );
        assert_eq!(
            resolve_path("src/lib.rs", cwd),
            PathBuf::from("/Users/someone/proj/src/lib.rs")
        );
    }

    #[test]
    fn resolve_path_handles_dot_dot() {
        let cwd = Path::new("/Users/someone/proj");
        assert_eq!(
            resolve_path("../sibling/file.txt", cwd),
            PathBuf::from("/Users/someone/proj/../sibling/file.txt")
        );
    }

    #[test]
    fn truncate_output_caps_at_char_boundary() {
        let s = "abcdef世";
        let t = truncate_output(s, 7);
        assert!(t.truncated);
        assert_eq!(t.text, "abcdef");
        assert_eq!(t.dropped, "世".len());
    }

    #[test]
    fn truncate_output_no_truncation_under_cap() {
        let t = truncate_output("short", 100);
        assert!(!t.truncated);
        assert_eq!(t.render("hint"), "short");
    }

    #[test]
    fn render_prefixed_advisory_reports_total() {
        let t = TruncatedText::new("body", 100, 50);
        let r = t.render("narrow the pattern");
        assert!(r.starts_with('⚠'), "prefixed: {r}");
        assert!(r.contains("150 bytes total"), "total reported: {r}");
        assert!(r.contains("showing first 100"), "cap reported: {r}");
        assert!(r.contains("narrow the pattern"), "hint folded in: {r}");
        assert!(r.contains("do not speculate"), "no-guess directive: {r}");
        assert!(r.contains("body"), "text preserved: {r}");
    }

    #[test]
    fn write_confinement_rejects_outside_project() {
        let cwd = Path::new("/tmp/manox-write-confinement");
        let policy = crate::sandbox::SandboxPolicy::for_project(cwd);
        let r = resolve_path_for_write("/etc/manox-probe", cwd, &policy);
        assert!(r.is_err(), "must reject write outside project: {r:?}");
        let e = r.unwrap_err();
        assert!(e.contains("outside project root"), "error: {e}");
    }

    #[test]
    fn write_confinement_rejects_dot_git() {
        let cwd = Path::new("/tmp/manox-write-confinement");
        let policy = crate::sandbox::SandboxPolicy::for_project(cwd);
        let r = resolve_path_for_write(".git/config", cwd, &policy);
        assert!(r.is_err(), "must reject write to .git: {r:?}");
        let e = r.unwrap_err();
        assert!(e.contains("sandbox"), "error: {e}");
    }

    #[test]
    fn write_confinement_allows_project_and_tmp() {
        let cwd = Path::new("/tmp/manox-write-confinement");
        let policy = crate::sandbox::SandboxPolicy::for_project(cwd);
        assert!(resolve_path_for_write("src/lib.rs", cwd, &policy).is_ok());
        let tmp = std::env::temp_dir().join("manox-write-confinement-probe");
        assert!(resolve_path_for_write(&tmp, cwd, &policy).is_ok());
    }

    #[test]
    fn read_not_confined() {
        let cwd = Path::new("/tmp/manox-write-confinement");
        let p = resolve_path("/etc/passwd", cwd);
        assert_eq!(p, PathBuf::from("/etc/passwd"));
    }

    #[test]
    fn read_only_flags_match_plan_mode_allowlist() {
        let tools = base_tools(Arc::new(PathBuf::from(".")));
        let by_name = |n: &str| tools.iter().find(|t| t.name() == n).unwrap();
        for n in [
            "read_file",
            "list_directory",
            "grep",
            "glob",
            "AskUserQuestion",
            "skill",
        ] {
            assert!(by_name(n).is_read_only(), "{n} should be read-only");
        }
        for n in ["write_file", "edit_file", "bash"] {
            assert!(!by_name(n).is_read_only(), "{n} should NOT be read-only");
        }
    }
}
