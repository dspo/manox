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
use gpui::WeakEntity;
use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::thread::Thread;
use crate::tool::{AgentTool, ToolOutputSink};
use crate::tools::{bridge_tokio, schema};

/// Wall-clock limit before a hung command is killed.
const BASH_DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Hard cap on retained stdout/stderr so a runaway command cannot OOM the app
/// and does not flood the model's context. Above this the capture keeps a
/// running byte total (for the truncation notice) but drops the overflowing
/// text. 64 KiB mirrors the order of magnitude used by other native agents
/// (zed 16 KiB, pi/oh-my-pi 50 KiB) — enough for typical `git`/`cargo` output,
/// small enough that the model can act on it.
const BASH_OUTPUT_MAX_BYTES: usize = 64 * 1024;
/// Narrow-the-command hint folded into the truncation advisory. Lives here
/// (not in `system_prompt.md`) because it is tool-specific guidance.
const BASH_TRUNCATION_HINT: &str = "retry with a narrower command (specify columns, `| head`, `LIMIT`, tighten the pattern)";
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
    /// Owning thread, read to check YOLO mode (forces the unsandboxed branch
    /// so bash runs outside seatbelt when YOLO is on). `None` in tests.
    thread: Option<WeakEntity<Thread>>,
}

impl BashTool {
    pub fn new(cwd: PathBuf, thread: WeakEntity<Thread>) -> Self {
        Self {
            cwd,
            shell: Arc::new(tokio::sync::Mutex::new(None)),
            thread: Some(thread),
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
    /// Run outside the OS sandbox (macOS seatbelt). Default false: the command
    /// is confined to project root + temp dir writes, `.git` read-only, network
    /// denied, and runs without approval. Set true only when the command
    /// genuinely needs the outside (network, writes outside the project) — it
    /// then runs in a persistent shell with no confinement, gated by user
    /// approval.
    #[serde(default)]
    unsandboxed: Option<bool>,
}

impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }
    fn description(&self) -> &str {
        "Run a bash command and return stdout. Runs inside an OS sandbox by default (macOS seatbelt: writes \
         confined to the project root and temp dir, `.git` read-only, network disabled); each call is a \
         one-shot `bash -c` — `cd`/`export` don't persist across calls, so chain steps with `&&` or use \
         the `cwd` parameter. For out-of-sandbox capabilities (network, writing outside the project root) \
         set `unsandboxed: true`, which after user approval runs in a persistent shell session (cd/export \
         persist across calls). Default timeout is 120s (override with timeout_secs); on timeout or cancel \
         the whole process group is terminated. stdout/stderr stream back to the UI live."
    }
    fn requires_approval(&self, input: &serde_json::Value) -> bool {
        let unsandboxed = input
            .get("unsandboxed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if unsandboxed {
            return true;
        }
        // Sandboxed bash skips approval only where an OS sandbox actually
        // enforces confinement. On platforms without a backend, the default
        // path falls back to an unconstrained brush shell — gate it on
        // approval until a real backend (Linux bwrap / Windows) lands.
        !crate::sandbox::is_available()
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
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let shell = self.shell.clone();
        let base_cwd = self.cwd.clone();
        let cwd_override = parsed.cwd.clone();
        let timeout = Duration::from_secs(parsed.timeout_secs.unwrap_or(BASH_DEFAULT_TIMEOUT_SECS));
        let command = parsed.command.clone();
        // YOLO mode forces the unsandboxed branch (DangerFullAccess): when the
        // owning thread has YOLO on, ignore the per-call `unsandboxed` flag and
        // always run via the persistent shell without seatbelt confinement.
        let yolo = self
            .thread
            .as_ref()
            .and_then(|t| t.upgrade())
            .map(|t| t.read_with(cx, |t, _| t.yolo()))
            .unwrap_or(false);
        let unsandboxed = parsed.unsandboxed.unwrap_or(false) || yolo;
        bridge_tokio(cx, async move {
            if unsandboxed {
                // Approved escalation / YOLO: brush's persistent shell, no confinement.
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
            } else if crate::sandbox::is_available() {
                // Sandboxed default: seatbelt-wrapped subprocess, no approval.
                #[cfg(target_os = "macos")]
                {
                    run_sandboxed_bash(
                        &command,
                        &base_cwd,
                        cwd_override.as_deref(),
                        timeout,
                        cancel,
                        sink,
                    )
                    .await
                }
                #[cfg(not(target_os = "macos"))]
                {
                    unreachable!("is_available() true only on macos")
                }
            } else {
                // No OS sandbox on this platform: fall back to brush + warn.
                tracing::warn!(
                    "sandbox unavailable on this platform, running unsandboxed via brush"
                );
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
            }
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
    let out_buf = Arc::new(std::sync::Mutex::new(CaptureBuffer::new(
        BASH_OUTPUT_MAX_BYTES,
    )));
    let err_buf = Arc::new(std::sync::Mutex::new(CaptureBuffer::new(
        BASH_OUTPUT_MAX_BYTES,
    )));
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
            // Resolve relative overrides against the thread cwd explicitly,
            // rather than relying on brush's internal resolution — the model
            // may pass a relative path meaning "relative to thread cwd", and
            // `resolve_path` makes that absolute before brush sees it.
            let resolved = super::resolve_path(cwd, base_cwd);
            guard
                .as_mut()
                .expect("shell initialized")
                .set_working_dir(&resolved)
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
    let (out_str, _out_trunc, out_dropped) =
        out_buf.lock().expect("capture buffer lock poisoned").take();
    let (err_str, _err_trunc, err_dropped) =
        err_buf.lock().expect("capture buffer lock poisoned").take();

    format_result(ran, out_str, out_dropped, err_str, err_dropped)
}

/// Format the outcome: success returns stdout (or stderr when stdout is empty);
/// non-zero exit / brush error / timeout / cancel returns an error carrying both
/// streams.
fn format_result(
    ran: Outcome,
    stdout: String,
    stdout_dropped: usize,
    stderr: String,
    stderr_dropped: usize,
) -> Result<String, anyhow::Error> {
    // `truncated == dropped > 0` is the `CaptureBuffer` invariant, so the bool
    // is derivable and not passed separately.
    let stdout = crate::tools::TruncatedText::new(&stdout, BASH_OUTPUT_MAX_BYTES, stdout_dropped)
        .render(BASH_TRUNCATION_HINT);
    let stderr = crate::tools::TruncatedText::new(&stderr, BASH_OUTPUT_MAX_BYTES, stderr_dropped)
        .render(BASH_TRUNCATION_HINT);
    match ran {
        Outcome::Ran(Ok(er)) => {
            if er.exit_code.is_success() {
                let combined = if stdout.is_empty() { stderr } else { stdout };
                Ok(combined)
            } else {
                let code = exit_code_num(er.exit_code);
                Err(anyhow::anyhow!(
                    "bash exit code {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                ))
            }
        }
        Outcome::Ran(Err(e)) => Err(anyhow::anyhow!(
            "bash execution failed: {}\nstdout:\n{stdout}\nstderr:\n{stderr}",
            brush_err(e)
        )),
        Outcome::TimedOut => Err(anyhow::anyhow!(
            "bash timed out (process group killed)\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )),
        Outcome::Cancelled => Err(anyhow::anyhow!(
            "bash cancelled\nstdout:\n{stdout}\nstderr:\n{stderr}"
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

/// Byte-capped buffer: appends until `max`, then drops the overflowing text
/// while still tallying the total bytes seen. The total lets the truncation
/// notice tell the model how much it is missing, so it knows to narrow the
/// command rather than guess at the truncated tail.
struct CaptureBuffer {
    buf: Vec<u8>,
    max: usize,
    truncated: bool,
    /// Bytes seen after the cap was hit; reported in the truncation notice.
    /// Only accrued once `truncated` is set, so this is the dropped tail size.
    dropped: usize,
}

impl CaptureBuffer {
    fn new(max: usize) -> Self {
        Self {
            buf: Vec::with_capacity(8 * 1024),
            max,
            truncated: false,
            dropped: 0,
        }
    }

    fn push(&mut self, data: &[u8]) {
        if self.truncated {
            self.dropped = self.dropped.saturating_add(data.len());
            return;
        }
        let remaining = self.max.saturating_sub(self.buf.len());
        if data.len() <= remaining {
            self.buf.extend_from_slice(data);
        } else {
            self.buf.extend_from_slice(&data[..remaining]);
            self.dropped = self.dropped.saturating_add(data.len() - remaining);
            self.truncated = true;
        }
    }

    /// Returns `(text, truncated, dropped_bytes)`.
    fn take(&mut self) -> (String, bool, usize) {
        let data = std::mem::take(&mut self.buf);
        let dropped = std::mem::take(&mut self.dropped);
        (
            String::from_utf8_lossy(&data).into_owned(),
            self.truncated,
            dropped,
        )
    }
}

/// Why a sandboxed wait ended: child exited, spawn failed, wall-clock
/// timeout, or user cancellation.
#[cfg(target_os = "macos")]
enum SandboxOutcome {
    Exited(std::process::ExitStatus),
    SpawnFailed(std::io::Error),
    TimedOut,
    Cancelled,
}

/// Format a sandboxed run. Mirrors [`format_result`] but over `ExitStatus`
/// instead of brush's `ExecutionResult`; the truncation rendering is shared
/// via [`crate::tools::TruncatedText`].
#[cfg(target_os = "macos")]
fn format_sandboxed_result(
    ran: SandboxOutcome,
    stdout: String,
    stdout_dropped: usize,
    stderr: String,
    stderr_dropped: usize,
) -> Result<String, anyhow::Error> {
    let stdout = crate::tools::TruncatedText::new(&stdout, BASH_OUTPUT_MAX_BYTES, stdout_dropped)
        .render(BASH_TRUNCATION_HINT);
    let stderr = crate::tools::TruncatedText::new(&stderr, BASH_OUTPUT_MAX_BYTES, stderr_dropped)
        .render(BASH_TRUNCATION_HINT);
    match ran {
        SandboxOutcome::Exited(status) => {
            if status.success() {
                let combined = if stdout.is_empty() { stderr } else { stdout };
                Ok(combined)
            } else {
                let code = status.code().unwrap_or(-1);
                Err(anyhow::anyhow!(
                    "bash exit code {code}\nstdout:\n{stdout}\nstderr:\n{stderr}"
                ))
            }
        }
        SandboxOutcome::SpawnFailed(e) => Err(anyhow::anyhow!(
            "bash spawn failed: {e}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )),
        SandboxOutcome::TimedOut => Err(anyhow::anyhow!(
            "bash timed out (process group killed)\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )),
        SandboxOutcome::Cancelled => Err(anyhow::anyhow!(
            "bash cancelled\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )),
    }
}

/// Async chunk reader: appends bytes into a [`CaptureBuffer`] and forwards
/// complete lines to the live `sink`, mirroring the sync [`read_pipe`] but for
/// a tokio pipe. Reads in fixed 8 KiB chunks (not `read_until`) so a
/// newline-less binary stream can't grow an unbounded line buffer or emit a
/// single huge chunk to the sink. EOFs when the child's stdout/stderr close.
#[cfg(target_os = "macos")]
async fn read_pipe_async<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    buf: Arc<std::sync::Mutex<CaptureBuffer>>,
    sink: ToolOutputSink,
) {
    use tokio::io::AsyncReadExt;
    let mut carry: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
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

/// Execute `command` in a seatbelt-sandboxed `sandbox-exec` subprocess: write
/// to project root + temp dir only, `.git` read-only, network denied. Mirrors
/// [`run_bash`]'s capture/timeout/cancel structure but over a one-shot
/// `tokio::process::Command` rather than brush's persistent shell.
#[cfg(target_os = "macos")]
async fn run_sandboxed_bash(
    command: &str,
    base_cwd: &Path,
    cwd_override: Option<&str>,
    timeout: Duration,
    cancel: CancellationToken,
    sink: ToolOutputSink,
) -> Result<String, anyhow::Error> {
    use std::process::Stdio;
    use tokio::process::Child;

    let cwd = cwd_override
        .map(|c| super::resolve_path(c, base_cwd))
        .unwrap_or_else(|| base_cwd.to_path_buf());
    let policy = crate::sandbox::SandboxPolicy::for_project(base_cwd);
    let mut cmd = policy.wrap_command(command, &cwd);
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    let mut child: Child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return format_sandboxed_result(
                SandboxOutcome::SpawnFailed(e),
                String::new(),
                0,
                String::new(),
                0,
            );
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let out_buf = Arc::new(std::sync::Mutex::new(CaptureBuffer::new(
        BASH_OUTPUT_MAX_BYTES,
    )));
    let err_buf = Arc::new(std::sync::Mutex::new(CaptureBuffer::new(
        BASH_OUTPUT_MAX_BYTES,
    )));
    let out_sink = sink.clone();
    let err_sink = sink;
    let out_buf_task = out_buf.clone();
    let err_buf_task = err_buf.clone();

    let mut out_task = tokio::task::spawn(read_pipe_async(stdout, out_buf_task, out_sink));
    let mut err_task = tokio::task::spawn(read_pipe_async(stderr, err_buf_task, err_sink));

    // Race the wait against the wall-clock timeout and user cancellation. The
    // `child.wait()` future is inline in the select! (not pinned outside), so
    // when cancel/timeout wins it is dropped — releasing the `&mut child`
    // borrow so we can `start_kill` + reap on the next line.
    let ran = tokio::select! {
        status = child.wait() => match status {
            Ok(s) => SandboxOutcome::Exited(s),
            Err(e) => SandboxOutcome::SpawnFailed(e),
        },
        _ = cancel.cancelled() => SandboxOutcome::Cancelled,
        _ = tokio::time::sleep(timeout) => SandboxOutcome::TimedOut,
    };
    if matches!(ran, SandboxOutcome::Cancelled | SandboxOutcome::TimedOut) {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }

    // Drain readers within the IO deadline; grandchildren holding the pipes
    // open would otherwise stall the join. The `join!` inside the timeout polls
    // each handle at most once; on a missed deadline we abort instead of polling
    // again — a `JoinHandle` polled after `Complete` panics, which was the
    // sandboxed-bash crash when the readers finished before the deadline.
    if tokio::time::timeout(Duration::from_millis(IO_DRAIN_TIMEOUT_MS), async {
        let _ = tokio::join!(&mut out_task, &mut err_task);
    })
    .await
    .is_err()
    {
        out_task.abort();
        err_task.abort();
    }

    let (out_str, out_dropped) = {
        let mut b = out_buf.lock().expect("capture buffer lock poisoned");
        let (t, _, d) = b.take();
        (t, d)
    };
    let (err_str, err_dropped) = {
        let mut b = err_buf.lock().expect("capture buffer lock poisoned");
        let (t, _, d) = b.take();
        (t, d)
    };

    format_sandboxed_result(ran, out_str, out_dropped, err_str, err_dropped)
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

    #[cfg(target_os = "macos")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sandboxed_bash_fast_exit_does_not_panic() {
        // Regression: `run_sandboxed_bash` used to `join!` the reader handles
        // inside the drain timeout and then `drain()` them again — polling a
        // completed JoinHandle panics ("JoinHandle polled after completion").
        // A command that exits immediately makes the readers finish before the
        // drain deadline, which was the exact crash repro. The fixed path
        // aborts on a missed deadline instead of re-polling, so a fast exit
        // must complete without panicking.
        let out = run_sandboxed_bash(
            "true",
            &PathBuf::from("."),
            None,
            Duration::from_secs(10),
            CancellationToken::new(),
            null_sink(),
        )
        .await
        .expect("fast exit must not panic and must report Ok");
        assert!(out.is_empty(), "expected empty output, got: {out}");
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
        assert!(msg.contains("exit code 7"), "got: {msg}");
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
        assert!(msg.contains("timed out"), "got: {msg}");
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
        assert!(msg.contains("cancelled"), "got: {msg}");
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

    #[test]
    fn capture_buffer_caps_and_tracks_dropped_total() {
        let mut buf = CaptureBuffer::new(64 * 1024);
        // First 64 KiB fills the cap exactly without truncation.
        buf.push(&vec![b'x'; 64 * 1024]);
        let (_, truncated, dropped) = buf.take();
        assert!(!truncated, "exactly at cap is not truncated");
        assert_eq!(dropped, 0);

        // Now overflow: cap + 10 KiB more. The tail is dropped but tallied.
        let mut buf = CaptureBuffer::new(64 * 1024);
        buf.push(&vec![b'x'; 64 * 1024]);
        buf.push(&vec![b'y'; 10 * 1024]);
        let (text, truncated, dropped) = buf.take();
        assert!(truncated);
        assert_eq!(dropped, 10 * 1024);
        assert!(!text.contains('y'), "dropped tail must not appear in text");
    }

    #[test]
    fn truncation_notice_is_prefixed_advisory_and_reports_total() {
        // 64 KiB cap + 12 KiB dropped → 76 KiB total reported.
        let note =
            crate::tools::TruncatedText::new("leftover line", BASH_OUTPUT_MAX_BYTES, 12 * 1024)
                .render(BASH_TRUNCATION_HINT);
        assert!(
            note.starts_with('⚠'),
            "notice must be prefixed so the model sees it first: {note}"
        );
        assert!(
            note.contains("narrower command"),
            "must advise narrowing: {note}"
        );
        assert!(
            note.contains(&format!("{} bytes total", BASH_OUTPUT_MAX_BYTES + 12 * 1024)),
            "must report total bytes: {note}"
        );
        assert!(
            note.contains("leftover line"),
            "truncated text must still be present: {note}"
        );
        // The notice is prefixed, so the truncated text comes after the warning.
        let warning_idx = note.find('⚠').unwrap();
        let text_idx = note.find("leftover line").unwrap();
        assert!(warning_idx < text_idx);
    }

    #[test]
    fn truncation_notice_absent_when_not_truncated() {
        let s = crate::tools::TruncatedText::new("plain output", BASH_OUTPUT_MAX_BYTES, 0)
            .render(BASH_TRUNCATION_HINT);
        assert_eq!(s, "plain output");
    }
}
