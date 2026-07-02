//! `bash` tool — a persistent in-process bash shell backed by [brush].
//!
//! The shell (`brush_core::Shell`) lives for the lifetime of the `BashTool`
//! (one per `Thread`), so `cd` / `export` / function definitions persist across
//! calls — the agent no longer has to re-pin the cwd on every command. Standard
//! builtins (`cd`, `pwd`, `echo`, `export`, `printf`, …) run in-process; only
//! external commands fork.
//!
//! Output is captured via real pipes (`std::io::pipe`) rather than brush's
//! in-memory `OpenFile::Stream`, because brush maps a `Stream` fd to
//! `Stdio::null()` for external children (`brush-core/src/openfiles.rs`:
//! `OpenFile::Stream(_) => Self::null()`) — piping is the only way an external
//! command's stdout/stderr reach us. A pair of `spawn_blocking` readers drain
//! the pipes line-by-line into a capped buffer AND forward each line to the
//! [`ToolOutputSink`] for live UI rendering.
//!
//! Cancellation/timeout semantics mirror the prior `sh -c` harness: each
//! external command runs in its own process group (`NewProcessGroup`), and on
//! timeout/cancel the groups are SIGTERM'd → grace → SIGKILL'd. brush does NOT
//! `kill_on_drop` its children (`brush-core/src/sys/tokio_process.rs`), so we
//! do the reaping ourselves via `Shell::jobs_mut()` after the `run_string`
//! future is dropped.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use brush_builtins::ShellBuilderExt;
use brush_core::openfiles::{self, OpenFile, OpenFiles};
use brush_core::results::ExecutionExitCode;
use brush_core::{ExecutionResult, Shell, SourceInfo};
use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentTool, ToolOutputSink};
use crate::tools::{bridge_tokio, schema};

/// Wall-clock limit before a hung command is killed.
const BASH_DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Hard cap on retained stdout/stderr so a runaway command cannot OOM the app.
const BASH_OUTPUT_MAX_BYTES: usize = 256 * 1024;
/// Grace window given to a SIGTERM'd process group before escalating to SIGKILL.
const CANCELLATION_GRACE_MS: u64 = 50;
/// Upper bound on draining the stdout/stderr pipes after the group is killed:
/// grandchildren that inherited the pipes can otherwise keep them open.
const IO_DRAIN_TIMEOUT_MS: u64 = 2_000;

pub struct BashTool {
    /// Base cwd used to seed the shell on first use; per-call overrides persist.
    cwd: PathBuf,
    /// Lazily-initialized persistent brush shell. One per `Thread` (the
    /// `ToolRegistry` is rebuilt per `Thread`).
    shell: Arc<tokio::sync::Mutex<Option<Shell>>>,
}

impl BashTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            shell: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct BashInput {
    /// Bash command to run in the persistent session (`cd` / `export` persist).
    command: String,
    /// Working directory override (persists across subsequent calls).
    #[serde(default)]
    cwd: Option<String>,
    /// Kill the command after this many seconds (defaults to 120).
    #[serde(default)]
    timeout_secs: Option<u64>,
}

impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "执行 bash 命令并返回 stdout。运行在持久 brush 会话中：cd / export / 函数定义跨调用保留。\
         默认 120s 超时（可用 timeout_secs 覆盖）；超时或取消时整个进程组被终止。stdout/stderr \
         实时回流 UI。"
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
        // Non-streaming entry: a throwaway sink whose chunks go unread.
        let (sink, _rx) = ToolOutputSink::channel("".into());
        self.run_streaming(input, cancel, sink, cx)
    }
    fn run_streaming(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        sink: ToolOutputSink,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<BashInput>(input) else {
            return cx.background_spawn(async { Err("input 解析失败".to_string()) });
        };
        let shell = self.shell.clone();
        let base_cwd = self.cwd.clone();
        let cwd_override = parsed.cwd.clone();
        let timeout = Duration::from_secs(parsed.timeout_secs.unwrap_or(BASH_DEFAULT_TIMEOUT_SECS));
        let command = parsed.command.clone();
        bridge_tokio(cx, async move {
            run_bash(
                shell,
                &command,
                &base_cwd,
                cwd_override.as_deref(),
                timeout,
                cancel,
                sink,
            )
            .await
        })
    }
}

/// Why the wait ended: natural exit, wall-clock timeout, or user cancellation.
enum Outcome {
    Ran(Result<ExecutionResult, brush_core::Error>),
    TimedOut,
    Cancelled,
}

/// Execute `command` in the persistent brush shell, enforce a wall-clock timeout
/// and cancellation, stream live output to `sink`, and return capped
/// stdout/stderr. On timeout/cancellation the spawned process groups are reaped
/// (SIGTERM → SIGKILL) so orphaned grandchildren cannot keep the pipes open.
#[allow(clippy::too_many_arguments)]
async fn run_bash(
    shell: Arc<tokio::sync::Mutex<Option<Shell>>>,
    command: &str,
    base_cwd: &Path,
    cwd_override: Option<&str>,
    timeout: Duration,
    cancel: CancellationToken,
    sink: ToolOutputSink,
) -> Result<String, anyhow::Error> {
    // Two pipe pairs: external children and builtins both write here, the
    // spawn_blocking readers drain the read ends.
    let (out_r, out_w) = std::io::pipe()?;
    let (err_r, err_w) = std::io::pipe()?;
    let out_buf = Arc::new(std::sync::Mutex::new(CaptureBuffer::new(BASH_OUTPUT_MAX_BYTES)));
    let err_buf = Arc::new(std::sync::Mutex::new(CaptureBuffer::new(BASH_OUTPUT_MAX_BYTES)));
    // Clone the sink and buffers for each reader task; the originals stay here
    // so we can pull the final captured text once the readers finish.
    let out_sink = sink.clone();
    let err_sink = sink;
    let out_buf_task = out_buf.clone();
    let err_buf_task = err_buf.clone();
    let mut out_task =
        tokio::task::spawn_blocking(move || read_pipe(out_r, out_buf_task, out_sink));
    let mut err_task =
        tokio::task::spawn_blocking(move || read_pipe(err_r, err_buf_task, err_sink));

    // Hold the lock only across the race. `params` (and thus the brush-side pipe
    // write ends) drops at the end of this block, so once the child group is
    // killed the readers reach EOF.
    let ran: Outcome = {
        let mut guard = shell.lock().await;
        if guard.is_none() {
            let mut s = Shell::builder()
                .default_builtins(brush_builtins::BuiltinSet::BashMode)
                .build()
                .await
                .map_err(brush_err)?;
            s.set_working_dir(base_cwd).map_err(brush_err)?;
            *guard = Some(s);
        }
        if let Some(cwd) = cwd_override {
            guard
                .as_mut()
                .expect("shell initialized")
                .set_working_dir(cwd)
                .map_err(brush_err)?;
        }
        let sh = guard.as_mut().expect("shell initialized");
        let mut params = sh.default_exec_params();
        // Each external command leads its own process group so the cancel/timeout
        // path can reap the whole tree with `kill(-pgid)`, matching the prior
        // `setsid` harness. brush defaults to `SameProcessGroup` when job control
        // is off, which would make the group-id kill miss.
        params.process_group_policy = brush_core::ProcessGroupPolicy::NewProcessGroup;
        params.set_fd(OpenFiles::STDIN_FD, openfiles::null().map_err(brush_err)?);
        params.set_fd(OpenFiles::STDOUT_FD, OpenFile::from(out_w));
        params.set_fd(OpenFiles::STDERR_FD, OpenFile::from(err_w));
        let source = SourceInfo::default();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Outcome::Cancelled,
            _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
            r = sh.run_string(command, &source, &params) => Outcome::Ran(r),
        }
    };

    // brush does not kill_on_drop, so a cancelled/timed-out run leaves the
    // spawned groups orphaned. Reap them now that the `run_string` borrow is
    // released.
    if matches!(ran, Outcome::Cancelled | Outcome::TimedOut) {
        let mut guard = shell.lock().await;
        let sh = guard.as_mut().expect("shell initialized");
        kill_jobs(sh, libc::SIGTERM);
        tokio::time::sleep(Duration::from_millis(CANCELLATION_GRACE_MS)).await;
        kill_jobs(sh, libc::SIGKILL);
        let _ = sh.jobs_mut().poll();
        sh.jobs_mut().jobs.clear();
    }

    // Drain readers within the IO deadline, then extract the captured text.
    drain(&mut out_task).await;
    drain(&mut err_task).await;
    let (out_str, out_trunc) = out_buf
        .lock()
        .expect("capture buffer lock poisoned")
        .take();
    let (err_str, err_trunc) = err_buf
        .lock()
        .expect("capture buffer lock poisoned")
        .take();

    Ok(format_result(ran, out_str, out_trunc, err_str, err_trunc)?)
}

/// Format the outcome: success returns stdout (or stderr when stdout is empty);
/// non-zero exit / brush error / timeout / cancel returns an error carrying both
/// streams.
fn format_result(
    ran: Outcome,
    stdout: String,
    stdout_trunc: bool,
    stderr: String,
    stderr_trunc: bool,
) -> Result<String, anyhow::Error> {
    let stdout = with_truncation_note(&stdout, stdout_trunc);
    let stderr = with_truncation_note(&stderr, stderr_trunc);
    match ran {
        Outcome::Ran(Ok(er)) => {
            if er.exit_code.is_success() {
                let combined = if stdout.is_empty() { stderr } else { stdout };
                Ok(combined)
            } else {
                let code = exit_code_num(er.exit_code);
                Err(anyhow::anyhow!(
                    "bash 退出码 {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                ))
            }
        }
        Outcome::Ran(Err(e)) => Err(anyhow::anyhow!(
            "bash 执行失败: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            brush_err(e)
        )),
        Outcome::TimedOut => Err(anyhow::anyhow!(
            "bash 超时（已终止进程组）\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )),
        Outcome::Cancelled => Err(anyhow::anyhow!(
            "bash 已取消\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )),
    }
}

/// Numeric exit code for the model-facing message. brush maps well-known codes
/// to named variants; `Custom(u8)` carries anything else.
fn exit_code_num(code: ExecutionExitCode) -> u8 {
    match code {
        ExecutionExitCode::Success => 0,
        ExecutionExitCode::GeneralError => 1,
        ExecutionExitCode::InvalidUsage => 2,
        ExecutionExitCode::Unimplemented => 99,
        ExecutionExitCode::CannotExecute => 126,
        ExecutionExitCode::NotFound => 127,
        ExecutionExitCode::Interrupted => 130,
        ExecutionExitCode::BrokenPipe => 141,
        ExecutionExitCode::Custom(c) => c,
    }
}

/// Drain a capped read task within the IO drain deadline; abort it if the
/// deadline is missed (a grandchild holding the pipe open would otherwise pin
/// the blocking thread).
async fn drain(task: &mut tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(Duration::from_millis(IO_DRAIN_TIMEOUT_MS), &mut *task)
        .await
        .is_err()
    {
        task.abort();
    }
}

/// Blocking reader: appends bytes into a [`CaptureBuffer`] and forwards complete
/// lines to the live `sink`. Runs on a `spawn_blocking` thread so the async
/// runtime is never stalled by a slow pipe.
fn read_pipe(
    mut r: std::io::PipeReader,
    buf: Arc<std::sync::Mutex<CaptureBuffer>>,
    sink: ToolOutputSink,
) {
    let mut carry: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let data = &chunk[..n];
                if let Ok(mut b) = buf.lock() {
                    b.push(data);
                }
                carry.extend_from_slice(data);
                while let Some(i) = carry.iter().position(|&b| b == b'\n') {
                    let rest = carry.split_off(i + 1);
                    let line = std::mem::take(&mut carry);
                    sink.try_emit(&String::from_utf8_lossy(&line));
                    carry = rest;
                }
            }
        }
    }
    if !carry.is_empty() {
        sink.try_emit(&String::from_utf8_lossy(&carry));
    }
}

/// Byte-capped buffer: appends until `max`, then drops the rest and marks
/// truncated. Matches the prior harness's cap-at-max policy.
struct CaptureBuffer {
    buf: Vec<u8>,
    max: usize,
    truncated: bool,
}

impl CaptureBuffer {
    fn new(max: usize) -> Self {
        Self {
            buf: Vec::with_capacity(8 * 1024),
            max,
            truncated: false,
        }
    }

    fn push(&mut self, data: &[u8]) {
        if self.truncated {
            return;
        }
        let remaining = self.max.saturating_sub(self.buf.len());
        if data.len() <= remaining {
            self.buf.extend_from_slice(data);
        } else {
            self.buf.extend_from_slice(&data[..remaining]);
            self.truncated = true;
        }
    }

    fn take(&mut self) -> (String, bool) {
        let data = std::mem::take(&mut self.buf);
        (String::from_utf8_lossy(&data).into_owned(), self.truncated)
    }
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

/// Signal every tracked job's process group. With `NewProcessGroup` each
/// external command leads its own group (pgid == child pid), so `kill(-pgid)`
/// reaches the whole tree — equivalent to the prior `setsid` + `kill(-pid)`.
#[cfg(unix)]
fn kill_jobs(shell: &Shell, sig: i32) {
    for job in &shell.jobs().jobs {
        if let Some(pgid) = job.process_group_id() {
            // Best-effort: the group may already be gone by the time we escalate.
            unsafe {
                let _ = libc::kill(-pgid, sig);
            }
        }
    }
}

#[cfg(not(unix))]
fn kill_jobs(_shell: &Shell, _sig: i32) {}

/// Adapt a brush error into an anyhow error for the model-facing string.
fn brush_err(e: brush_core::Error) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fresh_shell() -> Arc<tokio::sync::Mutex<Option<Shell>>> {
        Arc::new(tokio::sync::Mutex::new(None))
    }

    fn null_sink() -> ToolOutputSink {
        let (sink, _rx) = ToolOutputSink::channel("".into());
        sink
    }

    // These exercise `run_bash` directly (no gpui App) so the brush execution,
    // timeout escalation, and cancellation paths are covered.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_success_returns_stdout() {
        let out = run_bash(
            fresh_shell(),
            "printf hello",
            &PathBuf::from("."),
            None,
            Duration::from_secs(10),
            CancellationToken::new(),
            null_sink(),
        )
        .await
        .unwrap();
        assert_eq!(out, "hello");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_nonzero_exit_is_error_with_code() {
        let err = run_bash(
            fresh_shell(),
            "exit 7",
            &PathBuf::from("."),
            None,
            Duration::from_secs(10),
            CancellationToken::new(),
            null_sink(),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("退出码 7"), "got: {msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_timeout_kills_command() {
        let start = std::time::Instant::now();
        let err = run_bash(
            fresh_shell(),
            "sleep 30",
            &PathBuf::from("."),
            None,
            Duration::from_secs(1),
            CancellationToken::new(),
            null_sink(),
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
        let err = run_bash(
            fresh_shell(),
            "sleep 30",
            &PathBuf::from("."),
            None,
            Duration::from_secs(60),
            cancel,
            null_sink(),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("取消"), "got: {msg}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_timeout_reaps_orphaned_grandchild() {
        // `sleep 30 & wait` backgrounds a job the foreground `wait` is blocked on.
        // The timeout's group-kill must reach it; the turn must not block past
        // the timeout waiting on a pipe the grandchild keeps open.
        let start = std::time::Instant::now();
        let _ = run_bash(
            fresh_shell(),
            "sleep 30 & wait",
            &PathBuf::from("."),
            None,
            Duration::from_secs(1),
            CancellationToken::new(),
            null_sink(),
        )
        .await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "elapsed {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bash_persistent_session_preserves_cwd() {
        // `cd` in one call must persist to the next: the defining property of
        // the brush-backed persistent shell.
        let shell = fresh_shell();
        let _ = run_bash(
            shell.clone(),
            "cd /tmp",
            &PathBuf::from("."),
            None,
            Duration::from_secs(10),
            CancellationToken::new(),
            null_sink(),
        )
        .await
        .unwrap();
        let out = run_bash(
            shell,
            "pwd",
            &PathBuf::from("."),
            None,
            Duration::from_secs(10),
            CancellationToken::new(),
            null_sink(),
        )
        .await
        .unwrap();
        assert_eq!(out.trim_end(), "/tmp");
    }
}
