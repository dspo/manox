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

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use globset::{Glob, GlobSetBuilder};
use gpui::{App, AppContext as _, Task};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{SearcherBuilder, Sink, SinkMatch};
use ignore::{WalkBuilder, overrides::OverrideBuilder};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

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
        "读取指定文件的完整内容。输出格式：首行 `[PATH#TAG]`（TAG 是 4-hex 快照标签），\
         随后是 `N:TEXT` 行号格式（1-indexed）。后续 edit_file 必须复用此 TAG 与行号。"
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
        cx.background_spawn(async move {
            let raw = std::fs::read_to_string(&parsed.path)
                .map_err(|e| format!("read_file 失败: {e}"))?;
            let text = crate::hashline::normalize_to_lf(&raw);
            let snap = crate::hashline::global()
                .lock()
                .expect("hashline store poisoned")
                .record(std::path::Path::new(&parsed.path), &text);
            Ok(crate::hashline::format_numbered(
                &parsed.path,
                &text,
                &snap.tag,
            ))
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
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
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
    /// Hashline patch text. Each file section starts with `[PATH#TAG]` where TAG
    /// is the 4-hex snapshot tag from your latest `read_file`. Operations:
    /// `SWAP N.=M:` replace lines N..=M (inclusive) with the `+TEXT` body rows;
    /// `DEL N.=M` delete lines N..=M (no body); `INS.PRE N:` / `INS.POST N:` /
    /// `INS.HEAD:` / `INS.TAIL:` insert body rows; `SWAP.BLK N:` / `DEL.BLK N` /
    /// `INS.BLK.POST N:` operate on the bracket-block beginning at line N. Body
    /// rows are `+TEXT` (`+` alone = blank line; `+-x`/`++x` escapes a literal
    /// leading `-`/`+`). Line numbers reference the ORIGINAL file from read_file
    /// and do not shift across hunks. Ranges cover only changed lines; pure
    /// additions use `INS`, never a widened `SWAP`. On a stale-TAG rejection,
    /// re-`read_file` before retrying.
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
        cx.background_spawn(async move {
            let patches = crate::hashline::parse_patch(&parsed.patch).map_err(|e| e.to_string())?;
            let mut results: Vec<String> = Vec::new();
            for fp in patches {
                let path = fp.path.clone();
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
                    .record(std::path::Path::new(&path), &new_text);
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
            .map(PathBuf::from)
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

// ─── bash ─────────────────────────────────────────────────────────────────

pub struct BashTool {
    cwd: Arc<PathBuf>,
}

/// Wall-clock limit before a hung command is killed. Prevents a
/// non-terminating process (e.g. a network call with no response) from
/// stalling the turn indefinitely.
const BASH_DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Hard cap on retained stdout/stderr so a runaway command cannot OOM the app.
const BASH_OUTPUT_MAX_BYTES: usize = 256 * 1024;
/// Grace window given to a SIGTERM'd process group before escalating to SIGKILL.
const CANCELLATION_GRACE_MS: u64 = 50;
/// Upper bound on draining the stdout/stderr pipes after the child is killed:
/// grandchildren that inherited the pipes can otherwise keep them open.
const IO_DRAIN_TIMEOUT_MS: u64 = 2_000;

#[derive(Deserialize, JsonSchema)]
struct BashInput {
    /// Shell command to run (via `sh -c`).
    command: String,
    /// Working directory (defaults to cwd).
    #[serde(default)]
    cwd: Option<String>,
    /// Kill the command after this many seconds (defaults to 120).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

/// Why the wait ended after the natural-exit path: wall-clock timeout or user
/// cancellation. (Natural exit returns early and never constructs this.)
enum WaitEnd {
    TimedOut,
    Cancelled,
}

impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "执行 shell 命令并返回 stdout。命令经 `sh -c`，工作目录默认 cwd。\
         默认 120s 超时（可用 timeout_secs 覆盖）；超时或取消时整个进程组被终止。"
    }
    fn requires_approval(&self) -> bool {
        true
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<BashInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<BashInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        let cwd = parsed
            .cwd
            .map(PathBuf::from)
            .unwrap_or_else(|| self.cwd.as_ref().clone());
        bridge_tokio(cx, async move {
            run_bash(&parsed.command, &cwd, parsed.timeout_secs, cancel).await
        })
    }
}

/// Execute `sh -c <command>` in its own process group, enforce a wall-clock
/// timeout and cancellation, and return capped stdout/stderr. On timeout or
/// cancellation the entire process group is reaped (SIGTERM → SIGKILL) so
/// orphaned grandchildren cannot keep the pipes open.
async fn run_bash(
    command: &str,
    cwd: &Path,
    timeout_secs: Option<u64>,
    cancel: CancellationToken,
) -> Result<String, anyhow::Error> {
    let timeout = Duration::from_secs(timeout_secs.unwrap_or(BASH_DEFAULT_TIMEOUT_SECS));

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        // stdin null prevents commands that read stdin from hanging the turn.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    // Become a new session leader: the child's process-group id equals its pid,
    // so `kill(-pid)` later reaches the whole tree (curl, pipelines, etc.).
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("bash 启动失败: {e}"))?;

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");
    let out_task = tokio::spawn(async move { read_capped(stdout, BASH_OUTPUT_MAX_BYTES).await });
    let err_task = tokio::spawn(async move { read_capped(stderr, BASH_OUTPUT_MAX_BYTES).await });

    let end = tokio::select! {
        s = child.wait() => match s {
            Ok(status) => {
                let (stdout_str, stdout_trunc) = drain(out_task).await;
                let (stderr_str, stderr_trunc) = drain(err_task).await;
                return finish_exited(status, stdout_str, stdout_trunc, stderr_str, stderr_trunc);
            }
            Err(e) => return Err(anyhow::anyhow!("bash wait 失败: {e}")),
        },
        _ = tokio::time::sleep(timeout) => WaitEnd::TimedOut,
        _ = cancel.cancelled() => WaitEnd::Cancelled,
    };

    // Two-stage teardown: SIGTERM the group, give it a brief grace period, then
    // SIGKILL. Mirrors codex's cancellation escalation.
    if let Some(pid) = child.id() {
        kill_process_group(pid, libc::SIGTERM);
    }
    let status = match tokio::time::timeout(
        Duration::from_millis(CANCELLATION_GRACE_MS),
        child.wait(),
    )
    .await
    {
        Ok(s) => s?,
        Err(_) => {
            if let Some(pid) = child.id() {
                kill_process_group(pid, libc::SIGKILL);
            }
            child.wait().await?
        }
    };
    let _ = status;

    let (stdout_str, stdout_trunc) = drain(out_task).await;
    let (stderr_str, stderr_trunc) = drain(err_task).await;
    let stdout_str = with_truncation_note(&stdout_str, stdout_trunc);
    let stderr_str = with_truncation_note(&stderr_str, stderr_trunc);
    match end {
        WaitEnd::TimedOut => Err(anyhow::anyhow!(
            "bash 命令超时（{}s）已被终止\nstdout:\n{stdout_str}\nstderr:\n{stderr_str}",
            timeout.as_secs()
        )),
        WaitEnd::Cancelled => Err(anyhow::anyhow!(
            "bash 命令已被取消（用户中止）\nstdout:\n{stdout_str}\nstderr:\n{stderr_str}"
        )),
    }
}

/// Format the natural-exit outcome: success returns stdout (or stderr when
/// stdout is empty); non-zero returns an error carrying both streams.
fn finish_exited(
    status: std::process::ExitStatus,
    stdout_str: String,
    stdout_trunc: bool,
    stderr_str: String,
    stderr_trunc: bool,
) -> Result<String, anyhow::Error> {
    let stdout_str = with_truncation_note(&stdout_str, stdout_trunc);
    let stderr_str = with_truncation_note(&stderr_str, stderr_trunc);
    if status.success() {
        let combined = if stdout_str.is_empty() {
            stderr_str
        } else {
            stdout_str
        };
        Ok(combined)
    } else {
        let code = status.code().unwrap_or(-1);
        Err(anyhow::anyhow!(
            "bash 退出码 {code}\nstdout:\n{stdout_str}\nstderr:\n{stderr_str}"
        ))
    }
}

/// Drain a capped read task within the IO drain deadline. Returns an empty
/// string when the deadline is missed (grandchildren may hold the pipe open).
async fn drain(task: tokio::task::JoinHandle<(String, bool)>) -> (String, bool) {
    match tokio::time::timeout(Duration::from_millis(IO_DRAIN_TIMEOUT_MS), task).await {
        Ok(Ok(v)) => v,
        _ => (String::new(), false),
    }
}

/// Read up to `max` bytes from `r`; the boolean signals whether more was dropped.
async fn read_capped<R>(mut r: R, max: usize) -> (String, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut chunk = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if buf.len() + n <= max {
            buf.extend_from_slice(&chunk[..n]);
        } else {
            let take = max - buf.len();
            buf.extend_from_slice(&chunk[..take]);
            truncated = true;
            break;
        }
    }
    (String::from_utf8_lossy(&buf).into_owned(), truncated)
}

/// Append a truncation notice when the captured stream exceeded the byte cap.
fn with_truncation_note(s: &str, truncated: bool) -> String {
    if truncated {
        format!(
            "{s}\n... [output truncated: cap {} bytes]",
            BASH_OUTPUT_MAX_BYTES
        )
    } else {
        s.to_string()
    }
}

/// Signal an entire process group. The child was made a session leader via
/// `setsid`, so its pgid equals its pid; `kill(-pid, sig)` reaches the group.
#[cfg(unix)]
fn kill_process_group(pid: u32, sig: i32) {
    // Best-effort: the group may already be gone by the time we escalate.
    unsafe {
        let _ = libc::kill(-(pid as libc::pid_t), sig);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32, _sig: i32) {}

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
    reg.register(std::sync::Arc::new(ListDirectoryTool { cwd: cwd.clone() }) as AnyAgentTool);
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

    // ── bash harness ──
    // These exercise `run_bash` directly (no gpui App) so the process-group
    // spawn, timeout escalation, and cancellation paths are covered.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_success_returns_stdout() {
        let out = run_bash("printf hello", &PathBuf::from("."), None, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_nonzero_exit_is_error_with_code() {
        let err = run_bash("exit 7", &PathBuf::from("."), None, CancellationToken::new())
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("退出码 7"), "got: {msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_timeout_kills_command() {
        let start = std::time::Instant::now();
        let err = run_bash(
            "sleep 30",
            &PathBuf::from("."),
            Some(1),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        // The 1s timeout must reap the process, not wait out the full 30s sleep.
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "elapsed {:?}",
            start.elapsed()
        );
        let msg = err.to_string();
        assert!(msg.contains("超时"), "got: {msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_cancel_aborts_command() {
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            cancel2.cancel();
        });
        let err = run_bash("sleep 30", &PathBuf::from("."), Some(60), cancel)
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("取消"), "got: {msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_timeout_reaps_orphaned_grandchild() {
        // `sleep 30 &` detaches a grandchild from the foreground group only if
        // the shell doesn't put it in its own group; with setsid the whole tree
        // shares one group, so the timeout's group-kill must reach it. We can't
        // observe the grandchild directly, but the turn must not block past the
        // timeout waiting on a pipe the grandchild keeps open.
        let start = std::time::Instant::now();
        let _ = run_bash(
            "sleep 30 & wait",
            &PathBuf::from("."),
            Some(1),
            CancellationToken::new(),
        )
        .await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "elapsed {:?}",
            start.elapsed()
        );
    }
}
