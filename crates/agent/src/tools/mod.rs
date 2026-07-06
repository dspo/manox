//! Six file-system tools plus a brush-backed `bash` tool (see [`bash`]).
//!
//! - read_file / write_file / edit_file / list_directory: std::fs via gpui `background_spawn`.
//! - bash: in-process brush shell, spawned on the runtime handle and bridged back to a gpui `Task` via `async_channel`.
//! - grep: ripgrep library (grep-searcher + grep-regex + ignore), in-process via `background_spawn`.
//! - glob: the `ignore` crate (gitignore-aware walk) + `globset` (pattern match).
//!
//! Each tool generates its `input_schema` from a typed Input via `schemars`.
//! `requires_approval` gates the approval overlay: write_file / edit_file /
//! ask_user always require it; `bash` requires it on `unsandboxed: true`
//! escalation or when no OS sandbox backend is available (see [`bash`]).

pub mod agent;
pub mod ask_user;
pub mod bash;
pub mod self_info;
pub mod skill;

use globset::{Glob, GlobSetBuilder};
use gpui::{App, AppContext as _, Task, WeakEntity};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

use crate::thread::Thread;
use crate::tool::{AgentTool, AnyAgentTool, ToolRegistry};

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
fn resolve_path_for_write<P: AsRef<Path>>(
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
/// the truncated content` — is identical across tools, mirroring zed's
/// `Command output too long. The first N bytes:` and codex's
/// `Warning: truncated output (original token count: N)`.
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

// ─── read_file ────────────────────────────────────────────────────────────

pub struct ReadFileTool {
    cwd: Arc<PathBuf>,
}

#[derive(Deserialize, JsonSchema)]
struct ReadFileInput {
    /// Absolute or relative file path to read (relative to cwd).
    path: String,
}

impl AgentTool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read the full contents of the specified file. Output format: first line `[<abs-path>#<TAG>]`, \
         e.g. `[/Users/me/proj/src/lib.rs#A557]` where TAG is a 4-hex snapshot tag; \
         followed by `N:TEXT` line-numbered rows (1-indexed)."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<ReadFileInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<ReadFileInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let path = resolve_path(&parsed.path, &self.cwd);
        cx.background_spawn(async move {
            let raw =
                std::fs::read_to_string(&path).map_err(|e| format!("read_file failed: {e}"))?;
            let text = crate::hashline::normalize_to_lf(&raw);
            let snap = crate::hashline::global()
                .lock()
                .expect("hashline store poisoned")
                .record(&path, &text);
            Ok(crate::hashline::format_numbered(
                &path.display().to_string(),
                &text,
                &snap.tag,
            ))
        })
    }
}

// ─── write_file ───────────────────────────────────────────────────────────

pub struct WriteFileTool {
    cwd: Arc<PathBuf>,
    sandbox: crate::sandbox::SandboxPolicy,
}

#[derive(Deserialize, JsonSchema)]
struct WriteFileInput {
    /// File path to write.
    path: String,
    /// Full content to write.
    content: String,
}

impl AgentTool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write content to the specified file (overwrite). Use to create or rewrite a file."
    }
    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<WriteFileInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<WriteFileInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let path = match resolve_path_for_write(&parsed.path, &self.cwd, &self.sandbox) {
            Ok(p) => p,
            Err(e) => return cx.background_spawn(async move { Err(e) }),
        };
        let content_len = parsed.content.len();
        cx.background_spawn(async move {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, &parsed.content)
                .map(|_| format!("Wrote {} ({content_len} bytes)", path.display()))
                .map_err(|e| format!("write_file failed: {e}"))
        })
    }
}

// ─── edit_file ────────────────────────────────────────────────────────────

pub struct EditFileTool {
    cwd: Arc<PathBuf>,
    sandbox: crate::sandbox::SandboxPolicy,
}

#[derive(Deserialize, JsonSchema)]
struct EditFileInput {
    /// Hashline patch text. Each file section starts with a header
    /// `[<abs-path>#<tag>]` — paste the exact absolute path and 4-hex tag
    /// returned by your latest `read_file` for that file; do NOT write the
    /// literal word `PATH`. Example: `[/Users/me/proj/CLAUDE.md#A557]`.
    /// Operations: `SWAP N.=M:` replace lines N..=M (inclusive) with the
    /// `+TEXT` body rows; `DEL N.=M` delete lines N..=M (no body);
    /// `INS.PRE N:` / `INS.POST N:` / `INS.HEAD:` / `INS.TAIL:` insert body
    /// rows; `SWAP.BLK N:` / `DEL.BLK N` / `INS.BLK.POST N:` operate on the
    /// bracket-block beginning at line N. Body rows are `+TEXT` (`+` alone =
    /// blank line; `+-x`/`++x` escapes a literal leading `-`/`+`); a `-`-prefixed
    /// markdown list item is NOT a body row — rewrite it with a `+` prefix.
    /// Line numbers reference the ORIGINAL file from read_file and do not shift
    /// across hunks. Ranges cover only changed lines; pure additions use
    /// `INS`, never a widened `SWAP`. On a stale-TAG rejection, re-`read_file`
    /// before retrying.
    patch: String,
}

impl AgentTool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Edit an existing file via a hashline patch (line-anchored + TAG validation). See the input.patch field docs."
    }
    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<EditFileInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<EditFileInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let cwd = self.cwd.clone();
        let sandbox = self.sandbox.clone();
        cx.background_spawn(async move {
            let patches = crate::hashline::parse_patch(&parsed.patch).map_err(|e| e.to_string())?;
            let mut results: Vec<String> = Vec::new();
            for fp in patches {
                let path = resolve_path_for_write(&fp.path, &cwd, &sandbox)?;
                let path_display = path.display().to_string();
                let raw = std::fs::read_to_string(&path)
                    .map_err(|e| format!("edit_file read failed {path_display}: {e}"))?;
                let had_bom = crate::hashline::has_bom(&raw);
                let is_crlf = crate::hashline::detect_crlf(&raw);
                let had_trailing_nl = raw.ends_with('\n');
                let current = crate::hashline::normalize_to_lf(&raw);
                let current_tag = crate::hashline::compute_tag(&current);

                let new_text = if current_tag == fp.tag {
                    crate::hashline::apply(&current, &fp.ops)
                        .map_err(|e| format!("edit_file apply failed {path_display}: {e}"))?
                        .text
                } else {
                    let store = crate::hashline::global()
                        .lock()
                        .expect("hashline store poisoned");
                    crate::hashline::try_recover(&current, &fp.tag, &fp.ops, &store, &path)
                        .map_err(|e| format!("edit_file {path_display}: {e}"))?
                };

                // Restore original line endings, trailing newline, and BOM so
                // the write is a minimal content delta, not a full-rewrite that
                // flattens formatting or drops the file's terminating newline.
                let persisted = persist(&new_text, is_crlf, had_bom, had_trailing_nl);
                std::fs::write(&path, persisted.as_bytes())
                    .map_err(|e| format!("edit_file write failed {path_display}: {e}"))?;

                let new_snap = crate::hashline::global()
                    .lock()
                    .expect("hashline store poisoned")
                    .record(&path, &new_text);
                let diff = unified_diff(&current, &new_text);
                results.push(format!("[{}#{}]\n{}", path_display, new_snap.tag, diff));
            }
            Ok(results.join("\n---\n"))
        })
    }
}

/// Render a minimal unified diff for the edit result preview.
fn unified_diff(old: &str, new: &str) -> String {
    use similar::TextDiff;
    let diff = TextDiff::from_lines(old, new);
    let rendered = diff.unified_diff().to_string();
    if rendered.is_empty() {
        "(no changes)".to_string()
    } else {
        rendered
    }
}

/// Restore the file's original line-ending style, trailing newline, and optional
/// BOM on write. `apply`/`recover` model files as content lines without
/// terminators, so the trailing newline the source file carried is restored
/// here rather than dropped.
fn persist(text: &str, crlf: bool, bom: bool, trailing_nl: bool) -> String {
    let mut out = String::with_capacity(text.len() + 3);
    if bom {
        out.push('\u{feff}');
    }
    if crlf {
        let mut iter = text.split('\n').peekable();
        while let Some(line) = iter.next() {
            out.push_str(line);
            if iter.peek().is_some() {
                out.push_str("\r\n");
            }
        }
    } else {
        out.push_str(text);
    }
    // Re-attach a trailing terminator if the original file had one and the
    // edited content is non-empty (an emptied file stays empty).
    if trailing_nl && !text.is_empty() {
        if crlf {
            out.push_str("\r\n");
        } else {
            out.push('\n');
        }
    }
    out
}

// ─── list_directory ───────────────────────────────────────────────────────

pub struct ListDirectoryTool {
    cwd: Arc<PathBuf>,
}

#[derive(Deserialize, JsonSchema)]
struct ListDirectoryInput {
    /// Directory path to list (defaults to cwd).
    #[serde(default)]
    path: Option<String>,
}

impl AgentTool for ListDirectoryTool {
    fn name(&self) -> &str {
        "list_directory"
    }
    fn description(&self) -> &str {
        "List the direct children (files and directories) of a directory."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<ListDirectoryInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<ListDirectoryInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let base = parsed
            .path
            .map(|p| resolve_path(&p, &self.cwd))
            .unwrap_or_else(|| self.cwd.as_ref().clone());
        cx.background_spawn(async move {
            let entries =
                std::fs::read_dir(&base).map_err(|e| format!("list_directory failed: {e}"))?;
            let mut lines: Vec<String> = Vec::new();
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let tag = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    "/"
                } else {
                    ""
                };
                lines.push(format!("{name}{tag}"));
            }
            lines.sort();
            Ok(lines.join("\n"))
        })
    }
}

// ─── grep ─────────────────────────────────────────────────────────────────

pub struct GrepTool {
    cwd: Arc<PathBuf>,
}

#[derive(Deserialize, JsonSchema)]
struct GrepInput {
    /// Regex pattern.
    pattern: String,
    /// Search root directory (defaults to cwd).
    #[serde(default)]
    path: Option<String>,
    /// Optional filename glob filter (e.g. `*.rs`).
    #[serde(default)]
    glob: Option<String>,
}

impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents by regex, returning matching lines (with line numbers)."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<GrepInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<GrepInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let base = parsed
            .path
            .map(|p| resolve_path(&p, &self.cwd))
            .unwrap_or_else(|| self.cwd.as_ref().clone());
        cx.background_spawn(async move { run_grep(&parsed.pattern, &base, parsed.glob.as_deref()) })
    }
}

// ─── grep sink & runner ───────────────────────────────────────────────────

/// Accumulates search results as "file:line:content" lines.
struct GrepSink {
    path: String,
    results: Vec<String>,
}

impl Sink for GrepSink {
    type Error = std::io::Error;

    fn matched(
        &mut self,
        _searcher: &grep_searcher::Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        let line = mat.line_number().unwrap_or(0);
        let content = String::from_utf8_lossy(mat.bytes()).trim_end().to_string();
        self.results
            .push(format!("{}:{}:{}", self.path, line, content));
        Ok(true)
    }
}

fn run_grep(pattern: &str, root: &PathBuf, glob: Option<&str>) -> Result<String, String> {
    let matcher = RegexMatcherBuilder::new()
        .build(pattern)
        .map_err(|e| format!("grep invalid regex: {e}"))?;

    let mut walker = WalkBuilder::new(root);
    walker.standard_filters(true);
    walker.hidden(false);

    if let Some(g) = glob {
        let overrides = OverrideBuilder::new(root)
            .add(g)
            .map_err(|e| format!("grep invalid glob: {e}"))?
            .build()
            .map_err(|e| format!("grep glob build failed: {e}"))?;
        walker.overrides(overrides);
    }

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(grep_searcher::BinaryDetection::quit(b'\x00'))
        .build();

    let mut all_results: Vec<String> = Vec::new();
    for result in walker.build() {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        let is_file = entry.file_type().map(|ft| ft.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }
        let file_path = entry.path();
        let mut sink = GrepSink {
            path: file_path.display().to_string(),
            results: Vec::new(),
        };
        if searcher
            .search_path(&matcher, file_path, &mut sink)
            .is_err()
        {
            continue;
        }
        all_results.append(&mut sink.results);
    }

    if all_results.is_empty() {
        Ok("No matches".to_string())
    } else {
        Ok(all_results.join("\n"))
    }
}

// ─── glob ─────────────────────────────────────────────────────────────────

pub struct GlobTool {
    cwd: Arc<PathBuf>,
}

#[derive(Deserialize, JsonSchema)]
struct GlobInput {
    /// Glob pattern (e.g. `**/*.rs`), relative to `path`/cwd.
    /// Supports `{a,b}` alternation, `[ab]` classes, `**` recursion.
    /// `*.rs` matches top-level only; use `**/*.rs` for recursion.
    pattern: String,
    /// Search root, defaults to cwd.
    #[serde(default)]
    path: Option<String>,
    /// When true, ignore all `.gitignore`/`.ignore`/global gitignore rules.
    #[serde(default)]
    no_ignore: bool,
    /// When true, do not skip hidden (dot) files and directories.
    #[serde(default)]
    include_hidden: bool,
    /// When true, also yield directory entries that match the pattern.
    #[serde(default)]
    include_dirs: bool,
    /// Max results to return. Default 100. No hard ceiling — raising this
    /// risks blowing up your own context window with a huge repo; narrow
    /// `pattern` instead of raising `limit` when possible.
    #[serde(default)]
    limit: Option<usize>,
}

impl AgentTool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Find file paths matching a glob pattern (relative to cwd). Honors .gitignore and skips hidden files by default; returns relative paths, capped at 100. Pass no_ignore/include_hidden/include_dirs/limit to relax. Note: limit has no hard ceiling — raising it can blow up the context, so prefer narrowing the pattern first."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<GlobInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<GlobInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let cwd = self.cwd.as_ref().clone();
        cx.background_spawn(async move {
            let root = parsed
                .path
                .map(|p| resolve_path(&p, &cwd))
                .unwrap_or_else(|| cwd.clone());
            let matcher = GlobSetBuilder::new()
                .add(Glob::new(&parsed.pattern).map_err(|e| format!("glob invalid pattern: {e}"))?)
                .build()
                .map_err(|e| format!("glob build failed: {e}"))?;
            let limit = parsed.limit.unwrap_or(100);
            // Default flags enforce the strongest filtering; each toggle relaxes one axis.
            let mut builder = WalkBuilder::new(&root);
            builder
                .hidden(!parsed.include_hidden)
                .ignore(!parsed.no_ignore)
                .git_ignore(!parsed.no_ignore)
                .git_global(!parsed.no_ignore)
                .git_exclude(!parsed.no_ignore)
                .parents(!parsed.no_ignore);
            let mut out: Vec<String> = Vec::new();
            let mut truncated = false;
            for entry in builder.build().by_ref() {
                let Ok(e) = entry else { continue };
                let path = e.path();
                let is_dir = path.is_dir();
                if is_dir {
                    if !parsed.include_dirs {
                        continue;
                    }
                } else if !path.is_file() {
                    // Skip symlinks-to-nonfiles and special nodes.
                    continue;
                }
                let rel = path.strip_prefix(&root).unwrap_or(path);
                if matcher.is_match(rel) {
                    out.push(rel.display().to_string());
                    if out.len() >= limit {
                        truncated = true;
                        break;
                    }
                }
            }
            out.sort();
            if out.is_empty() {
                Ok("No matching files".to_string())
            } else if truncated {
                // Count-based truncation (not byte), so it does not go through
                // `TruncatedText`; the wording is unified to the `⚠` advisory style.
                Ok(format!(
                    "⚠ Too many matches (showing first {limit}, more omitted). Narrow `pattern` or raise `limit` and retry — do not speculate about omitted matches.\n\n{}",
                    out.join("\n")
                ))
            } else {
                Ok(out.join("\n"))
            }
        })
    }
}

// ─── Default registry ─────────────────────────────────────────────────────

/// The built-in tools as `AnyAgentTool` (no `agent` tool). Shared by the main
/// registry and sub-agent registries (which filter by name).
pub(crate) fn base_tools(cwd: Arc<PathBuf>, thread: WeakEntity<Thread>) -> Vec<AnyAgentTool> {
    // Derive the write-confinement policy once and share it across every write
    // tool (WriteFileTool / EditFileTool / BashTool) so the Rust-side FS check
    // and the bash seatbelt / cwd pre-check classify paths identically —
    // independent per-tool derivations drift (issue 3).
    let sandbox = crate::sandbox::SandboxPolicy::for_project(cwd.as_ref());
    vec![
        Arc::new(ReadFileTool { cwd: cwd.clone() }) as AnyAgentTool,
        Arc::new(WriteFileTool {
            cwd: cwd.clone(),
            sandbox: sandbox.clone(),
        }),
        Arc::new(EditFileTool {
            cwd: cwd.clone(),
            sandbox: sandbox.clone(),
        }),
        Arc::new(ListDirectoryTool { cwd: cwd.clone() }),
        Arc::new(bash::BashTool::new(
            cwd.as_ref().clone(),
            thread,
            sandbox.clone(),
        )),
        Arc::new(GrepTool { cwd: cwd.clone() }),
        Arc::new(GlobTool { cwd: cwd.clone() }),
        Arc::new(ask_user::AskUserQuestionTool),
        Arc::new(skill::SkillTool),
    ]
}

/// Build a `ToolRegistry` with the built-in tools plus the `agent` sub-agent
/// tool. `parent` is the owning `Thread` so the `agent` tool can route
/// bubbled-up authorizations and read the parent's model.
pub fn default_registry(cwd: PathBuf, parent: WeakEntity<Thread>) -> ToolRegistry {
    let cwd = Arc::new(cwd);
    let mut reg = ToolRegistry::new();
    for tool in base_tools(cwd.clone(), parent.clone()) {
        reg.register(tool);
    }
    reg.register(Arc::new(agent::SpawnAgentTool::new(cwd, 0, parent.clone())) as AnyAgentTool);
    reg.register(self_info::new(parent));

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
        let tools = base_tools(
            Arc::new(PathBuf::from(".")),
            WeakEntity::<Thread>::new_invalid(),
        );
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
        // The sub-agent tool is registered by `default_registry`, not `base_tools`.
        assert!(!names.contains(&"agent"));
    }

    #[test]
    fn input_schemas_are_objects() {
        for v in [
            schema::<ReadFileInput>(),
            schema::<WriteFileInput>(),
            schema::<EditFileInput>(),
        ] {
            assert_eq!(v["type"], "object");
        }
    }

    #[test]
    fn persist_restores_trailing_newline_and_crlf() {
        // apply yields content without a terminator; persist re-attaches the
        // file's original trailing newline (LF or CRLF) and BOM.
        assert_eq!(persist("a\nb", false, false, true), "a\nb\n");
        assert_eq!(persist("a\nb", true, false, true), "a\r\nb\r\n");
        // No trailing newline originally → none added.
        assert_eq!(persist("a\nb", false, false, false), "a\nb");
        // Emptied content stays empty even if the original had a newline.
        assert_eq!(persist("", false, false, true), "");
        // BOM is re-prepended.
        assert_eq!(persist("x", false, true, false), "\u{feff}x");
    }

    #[test]
    fn resolve_path_passes_absolute_through() {
        let cwd = Path::new("/Users/someone/proj");
        // Unix absolute, with trailing file name.
        assert_eq!(
            resolve_path("/Users/someone/proj/CLAUDE.md", cwd),
            PathBuf::from("/Users/someone/proj/CLAUDE.md")
        );
        // Absolute path is not rewritten to live under cwd even if it overlaps.
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
        // `.` and nested relative paths resolve under cwd, never the process cwd.
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
        // `..` stays literal (no canonicalization) so hashline snapshot keys
        // remain a stable string across calls — the cwd is already absolute.
        assert_eq!(
            resolve_path("../sibling/file.txt", cwd),
            PathBuf::from("/Users/someone/proj/../sibling/file.txt")
        );
    }

    #[test]
    fn truncate_output_caps_at_char_boundary() {
        // 6-byte ASCII + 3-byte '世' (U+4E16). Cap at 7 bytes must keep the
        // ASCII prefix and drop '世' whole (no mid-codepoint split).
        let s = "abcdef世";
        let t = truncate_output(s, 7);
        assert!(t.truncated);
        assert_eq!(t.text, "abcdef");
        assert_eq!(t.dropped, "世".len()); // 3 bytes dropped
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
        // /etc is outside the project root + temp dir.
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
        // Reads use resolve_path, not resolve_path_for_write, so they are not
        // confined — matching zed/codex which allow reading anywhere.
        let cwd = Path::new("/tmp/manox-write-confinement");
        let p = resolve_path("/etc/passwd", cwd);
        assert_eq!(p, PathBuf::from("/etc/passwd"));
    }

    #[test]
    fn read_only_flags_match_plan_mode_allowlist() {
        // The plan-mode tool filter relies on `is_read_only()` being accurate:
        // read-only tools stay available, write/exec/spawn tools are hidden.
        let tools = base_tools(
            Arc::new(PathBuf::from(".")),
            WeakEntity::<Thread>::new_invalid(),
        );
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
