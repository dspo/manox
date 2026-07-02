//! Seven built-in tools.
//!
//! - read_file / write_file / edit_file / list_directory: std::fs via gpui `background_spawn`.
//! - bash: tokio::process, spawned on the runtime handle and bridged back to a gpui `Task` via `async_channel`.
//! - grep: ripgrep library (grep-searcher + grep-regex + ignore), in-process via `background_spawn`.
//! - glob: the `ignore` crate (gitignore-aware walk) + `globset` (pattern match).
//!
//! Each tool generates its `input_schema` from a typed Input via `schemars`. Tools
//! requiring approval (write_file / edit_file / bash) override `requires_approval`
//! to return true.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext as _, Task};
use globset::{Glob, GlobSetBuilder};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tool::{AgentTool, AnyAgentTool, ToolRegistry};

/// Convert a schemars schema to a `serde_json::Value`.
fn schema<T: JsonSchema>() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(T)).expect("schema serialization")
}

/// Bridge a tokio task back to a gpui background `Task` via `async_channel`.
fn bridge_tokio<F, R>(cx: &mut App, fut: F) -> Task<Result<String, String>>
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

// ─── read_file ────────────────────────────────────────────────────────────

pub struct ReadFileTool;

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
        "读取指定文件的完整内容。用于查看源码、配置等。"
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<ReadFileInput>()
    }
    fn run(&self, input: serde_json::Value, cx: &mut App) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<ReadFileInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        cx.background_spawn(async move {
            std::fs::read_to_string(&parsed.path).map_err(|e| format!("read_file 失败: {e}"))
        })
    }
}

// ─── write_file ───────────────────────────────────────────────────────────

pub struct WriteFileTool;

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
    fn run(&self, input: serde_json::Value, cx: &mut App) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<WriteFileInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        cx.background_spawn(async move {
            if let Some(parent) = std::path::Path::new(&parsed.path).parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&parsed.path, &parsed.content)
                .map(|_| format!("已写入 {}（{} 字节）", parsed.path, parsed.content.len()))
                .map_err(|e| format!("write_file 失败: {e}"))
        })
    }
}

// ─── edit_file ────────────────────────────────────────────────────────────

pub struct EditFileTool;

#[derive(Deserialize, JsonSchema)]
struct EditFileInput {
    /// File path to edit.
    path: String,
    /// Old text to replace; must occur uniquely in the file.
    old_string: String,
    /// New text to substitute in.
    new_string: String,
}

impl AgentTool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "把文件中 old_string 替换为 new_string（old_string 必须唯一匹配）。"
    }
    fn requires_approval(&self) -> bool {
        true
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<EditFileInput>()
    }
    fn run(&self, input: serde_json::Value, cx: &mut App) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<EditFileInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        cx.background_spawn(async move {
            let content = std::fs::read_to_string(&parsed.path)
                .map_err(|e| format!("edit_file 读取失败: {e}"))?;
            let count = content.matches(&parsed.old_string).count();
            if count == 0 {
                return Err("edit_file 失败: 未找到 old_string".to_string());
            }
            if count > 1 {
                return Err(format!(
                    "edit_file 失败: old_string 匹配 {count} 处，需唯一"
                ));
            }
            let new_content = content.replacen(&parsed.old_string, &parsed.new_string, 1);
            std::fs::write(&parsed.path, new_content.as_bytes())
                .map(|_| format!("已编辑 {}", parsed.path))
                .map_err(|e| format!("edit_file 写入失败: {e}"))
        })
    }
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
    fn run(&self, input: serde_json::Value, cx: &mut App) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<ListDirectoryInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        let base = parsed
            .path
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd.as_ref().clone());
        cx.background_spawn(async move {
            let entries = std::fs::read_dir(&base).map_err(|e| format!("list_directory 失败: {e}"))?;
            let mut lines: Vec<String> = Vec::new();
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let tag = if entry
                    .file_type()
                    .map(|t| t.is_dir())
                    .unwrap_or(false)
                {
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

// ─── bash ─────────────────────────────────────────────────────────────────

pub struct BashTool {
    cwd: Arc<PathBuf>,
}

#[derive(Deserialize, JsonSchema)]
struct BashInput {
    /// Shell command to run (via `sh -c`).
    command: String,
    /// Working directory (defaults to cwd).
    #[serde(default)]
    cwd: Option<String>,
}

impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "执行 shell 命令并返回 stdout。命令经 `sh -c`，工作目录默认 cwd。"
    }
    fn requires_approval(&self) -> bool {
        true
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<BashInput>()
    }
    fn run(&self, input: serde_json::Value, cx: &mut App) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<BashInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        let cwd = parsed
            .cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd.as_ref().clone());
        bridge_tokio(cx, async move {
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&parsed.command)
                .current_dir(&cwd)
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("bash 启动失败: {e}"))?;
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            if !output.status.success() {
                let code = output.status.code().unwrap_or(-1);
                return Err(anyhow::anyhow!(
                    "bash 退出码 {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                ));
            }
            let combined = if stdout.is_empty() { stderr } else { stdout };
            Ok(combined)
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
    fn run(&self, input: serde_json::Value, cx: &mut App) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<GrepInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        let base = parsed
            .path
            .map(PathBuf::from)
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
        if searcher.search_path(&matcher, file_path, &mut sink).is_err() {
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
    fn run(&self, input: serde_json::Value, cx: &mut App) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<GlobInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        let cwd = self.cwd.as_ref().clone();
        cx.background_spawn(async move {
            let root = parsed.path.map(PathBuf::from).unwrap_or_else(|| cwd.clone());
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

/// Build a `ToolRegistry` with the 7 built-in tools; `cwd` is the relative root for each tool.
pub fn default_registry(cwd: PathBuf) -> ToolRegistry {
    let cwd = Arc::new(cwd);
    let mut reg = ToolRegistry::new();
    reg.register(std::sync::Arc::new(ReadFileTool) as AnyAgentTool);
    reg.register(std::sync::Arc::new(WriteFileTool) as AnyAgentTool);
    reg.register(std::sync::Arc::new(EditFileTool) as AnyAgentTool);
    reg.register(std::sync::Arc::new(ListDirectoryTool {
        cwd: cwd.clone(),
    }) as AnyAgentTool);
    reg.register(std::sync::Arc::new(BashTool { cwd: cwd.clone() }) as AnyAgentTool);
    reg.register(std::sync::Arc::new(GrepTool { cwd: cwd.clone() }) as AnyAgentTool);
    reg.register(std::sync::Arc::new(GlobTool { cwd }) as AnyAgentTool);
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_registry_has_seven_tools() {
        let reg = default_registry(PathBuf::from("."));
        let tools = reg.to_request_tools();
        assert_eq!(tools.len(), 7);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"edit_file"));
        assert!(names.contains(&"list_directory"));
        assert!(names.contains(&"bash"));
        assert!(names.contains(&"grep"));
        assert!(names.contains(&"glob"));
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
}
