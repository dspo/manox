//! Six file-system tools plus a brush-backed `bash` tool (see [`bash`]).
//!
//! - read_file / write_file / edit_file / list_directory: std::fs via gpui `background_spawn`.
//! - bash: in-process brush shell, spawned on the runtime handle and bridged back to a gpui `Task` via `async_channel`.
//! - grep: ripgrep library (grep-searcher + grep-regex + ignore), in-process via `background_spawn`.
//! - glob: the `ignore` crate (gitignore-aware walk) + `globset` (pattern match).
//!
//! Each tool generates its `input_schema` from a typed Input via `schemars`. Tools
//! requiring approval (write_file / edit_file / bash) override `requires_approval`
//! to return true.

pub mod agent;
pub mod ask_user;
pub mod bash;

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
fn resolve_path<P: AsRef<Path>>(input: P, cwd: &Path) -> PathBuf {
    debug_assert!(cwd.is_absolute(), "resolve_path cwd must be absolute");
    let p = input.as_ref();
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
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
        "读取指定文件的完整内容。输出格式：首行 `[<绝对路径>#<TAG>]`，\
         例如 `[/Users/me/proj/src/lib.rs#A557]`，其中 TAG 是 4-hex 快照标签。\
         随后是 `N:TEXT` 行号格式（1-indexed）。后续 edit_file 必须复用此 TAG 与行号，\
         且 patch 头部要写 read_file 返回的同一个绝对路径——不要用字面 `PATH` 占位符。"
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<ReadFileInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<ReadFileInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        let path = resolve_path(&parsed.path, &self.cwd);
        cx.background_spawn(async move {
            let raw = std::fs::read_to_string(&path).map_err(|e| format!("read_file 失败: {e}"))?;
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
        "把内容写入指定文件（覆盖）。用于创建或重写文件。"
    }
    fn requires_approval(&self) -> bool {
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
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        let path = resolve_path(&parsed.path, &self.cwd);
        let content_len = parsed.content.len();
        cx.background_spawn(async move {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, &parsed.content)
                .map(|_| format!("已写入 {}（{content_len} 字节）", path.display()))
                .map_err(|e| format!("write_file 失败: {e}"))
        })
    }
}

// ─── edit_file ────────────────────────────────────────────────────────────

pub struct EditFileTool {
    cwd: Arc<PathBuf>,
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
        "用 hashline patch 编辑已存在文件（行号锚定 + TAG 校验）。见 input.patch 字段说明。"
    }
    fn requires_approval(&self) -> bool {
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
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        let cwd = self.cwd.clone();
        cx.background_spawn(async move {
            let patches = crate::hashline::parse_patch(&parsed.patch).map_err(|e| e.to_string())?;
            let mut results: Vec<String> = Vec::new();
            for fp in patches {
                let path = resolve_path(&fp.path, &cwd);
                let path_display = path.display().to_string();
                let raw = std::fs::read_to_string(&path)
                    .map_err(|e| format!("edit_file 读取失败 {path_display}: {e}"))?;
                let had_bom = crate::hashline::has_bom(&raw);
                let is_crlf = crate::hashline::detect_crlf(&raw);
                let had_trailing_nl = raw.ends_with('\n');
                let current = crate::hashline::normalize_to_lf(&raw);
                let current_tag = crate::hashline::compute_tag(&current);

                let new_text = if current_tag == fp.tag {
                    crate::hashline::apply(&current, &fp.ops)
                        .map_err(|e| format!("edit_file 应用失败 {path_display}: {e}"))?
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
                    .map_err(|e| format!("edit_file 写入失败 {path_display}: {e}"))?;

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
        "(无变更)".to_string()
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
        "列出目录下的直接子条目（文件与目录）。"
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<ListDirectoryInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<ListDirectoryInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        let base = parsed
            .path
            .map(|p| resolve_path(&p, &self.cwd))
            .unwrap_or_else(|| self.cwd.as_ref().clone());
        cx.background_spawn(async move {
            let entries =
                std::fs::read_dir(&base).map_err(|e| format!("list_directory 失败: {e}"))?;
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
        "用 ripgrep 搜索文件内容，返回匹配行（带行号）。"
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<GrepInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<GrepInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
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
        .map_err(|e| format!("grep 正则无效: {e}"))?;

    let mut walker = WalkBuilder::new(root);
    walker.standard_filters(true);
    walker.hidden(false);

    if let Some(g) = glob {
        let overrides = OverrideBuilder::new(root)
            .add(g)
            .map_err(|e| format!("grep glob 无效: {e}"))?
            .build()
            .map_err(|e| format!("grep glob 构建失败: {e}"))?;
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
        Ok("无匹配".to_string())
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
        "按 glob 模式查找文件路径（相对 cwd）。默认尊重 .gitignore 并跳过隐藏文件；返回相对路径，上限 100。可传 no_ignore/include_hidden/include_dirs/limit 放宽。注意：limit 无硬顶，调大可能撑爆上下文，优先收窄 pattern。"
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<GlobInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<GlobInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        let cwd = self.cwd.as_ref().clone();
        cx.background_spawn(async move {
            let root = parsed
                .path
                .map(|p| resolve_path(&p, &cwd))
                .unwrap_or_else(|| cwd.clone());
            let matcher = GlobSetBuilder::new()
                .add(Glob::new(&parsed.pattern).map_err(|e| format!("glob 模式无效: {e}"))?)
                .build()
                .map_err(|e| format!("glob 编译失败: {e}"))?;
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
                Ok("无匹配文件".to_string())
            } else if truncated {
                Ok(format!(
                    "{}\n... (truncated to {}, more matches exist — raise `limit` or narrow `pattern`)",
                    out.join("\n"),
                    limit
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
pub(crate) fn base_tools(cwd: Arc<PathBuf>) -> Vec<AnyAgentTool> {
    vec![
        Arc::new(ReadFileTool { cwd: cwd.clone() }) as AnyAgentTool,
        Arc::new(WriteFileTool { cwd: cwd.clone() }),
        Arc::new(EditFileTool { cwd: cwd.clone() }),
        Arc::new(ListDirectoryTool { cwd: cwd.clone() }),
        Arc::new(bash::BashTool::new(cwd.as_ref().clone())),
        Arc::new(GrepTool { cwd: cwd.clone() }),
        Arc::new(GlobTool { cwd: cwd.clone() }),
        Arc::new(ask_user::AskUserQuestionTool),
    ]
}

/// Build a `ToolRegistry` with the built-in tools plus the `agent` sub-agent
/// tool. `parent` is the owning `Thread` so the `agent` tool can route
/// bubbled-up authorizations and read the parent's model.
pub fn default_registry(cwd: PathBuf, parent: WeakEntity<Thread>) -> ToolRegistry {
    let cwd = Arc::new(cwd);
    let mut reg = ToolRegistry::new();
    for tool in base_tools(cwd.clone()) {
        reg.register(tool);
    }
    reg.register(Arc::new(agent::SpawnAgentTool::new(cwd, 0, parent)) as AnyAgentTool);
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_tools_has_eight_tools() {
        let tools = base_tools(Arc::new(PathBuf::from(".")));
        assert_eq!(tools.len(), 8);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"list_directory"));
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"glob"));
        assert!(names.contains(&"AskUserQuestion"));
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
}
