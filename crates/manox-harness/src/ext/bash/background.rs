// Background task registry implementation and the poll/stop tools built on
// it. A background task is a `sh -c` process whose output accumulates in a
// ring buffer; `poll` returns only the increment since the last poll, so the
// model can follow a long-running command without re-reading history.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::core::BackgroundTaskRegistry;
use crate::core::tool::{AgentTool, AgentToolResult, ToolError};
use serde_json::Value as JsonValue;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

/// Per-line stdout callback for line-event observers (the background bash
/// producer feeds the host task center its card lines this way).
pub type LineEventCallback = Box<dyn Fn(&crate::core::TaskId, String) + Send + Sync + 'static>;

/// Hard cap on the accumulated output buffer per task; older bytes are
/// dropped from the front once the cap is hit.
const MAX_BUFFER_BYTES: usize = 256 * 1024;
/// How long a finished task stays in the registry before GC sweeps it.
const GC_AFTER_EXIT: Duration = Duration::from_secs(300);

struct TaskEntry {
    /// All bytes seen so far (front-dropped past `MAX_BUFFER_BYTES`).
    buffer: Mutex<Vec<u8>>,
    /// Logical byte offset the last `poll` read up to, relative to the
    /// stream start (front-dropped bytes are accounted via `total_drained`).
    read_cursor: Mutex<u64>,
    /// Bytes dropped from the front of the ring, so the buffer's first byte
    /// corresponds to logical offset `total_drained`.
    total_drained: AtomicU64,
    /// Total bytes ever produced (even after the ring drops old bytes).
    total_bytes: AtomicU64,
    /// `None` while running, `Some(Some(code))` on clean exit, `Some(None)`
    /// when signaled.
    exit_code: Mutex<Option<Option<i32>>>,
    /// Completion notification: the drain task sends the exit code the moment
    /// it is recorded, so a watcher can await the exit instead of polling.
    exit: watch::Sender<Option<Option<i32>>>,
    child: Mutex<Option<tokio::process::Child>>,
    /// The child pid, which is also its process-group id (`process_group(0)`);
    /// kept separate so `kill` can signal the group even after the drain task
    /// took the child handle.
    pid: Mutex<Option<i32>>,
    last_activity: Mutex<Instant>,
    /// Model-facing reads currently blocked on this task's exit.
    collectors: AtomicUsize,
    /// Whether the exit landed while a model-facing read was blocked on it:
    /// that read's result already carries the terminal state, so the
    /// completion summary stands down.
    exit_collected: AtomicBool,
    /// Per-line stdout taps. Installed at spawn for line-event observers,
    /// or attached later when a foreground command is promoted to a
    /// background task and its output starts feeding the host card. The
    /// drain task holds the only writer; `add_line_tap` is the only
    /// installer.
    line_taps: Mutex<Vec<LineEventCallback>>,
    /// While a foreground call owns the task, every chunk is also forwarded
    /// here — under the buffer lock, so the forwarded stream and the ring
    /// agree at the instant the call releases the task.
    foreground: Mutex<Option<mpsc::UnboundedSender<OutputChunk>>>,
}

/// Which pipe an output chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// One raw read from a task's pipes, as a foreground call receives it.
#[derive(Debug)]
pub struct OutputChunk {
    pub stream: OutputStream,
    pub bytes: Vec<u8>,
}

impl TaskEntry {
    fn new(child: tokio::process::Child, pid: i32) -> Self {
        TaskEntry {
            buffer: Mutex::new(Vec::new()),
            read_cursor: Mutex::new(0),
            total_drained: AtomicU64::new(0),
            total_bytes: AtomicU64::new(0),
            exit_code: Mutex::new(None),
            exit: watch::channel(None).0,
            child: Mutex::new(Some(child)),
            pid: Mutex::new(Some(pid)),
            last_activity: Mutex::new(Instant::now()),
            collectors: AtomicUsize::new(0),
            exit_collected: AtomicBool::new(false),
            line_taps: Mutex::new(Vec::new()),
            foreground: Mutex::new(None),
        }
    }

    /// Emit one framed line to every installed tap.
    fn tap_line(&self, id: &crate::core::TaskId, line: String) {
        let taps = self.line_taps.lock().expect("taps lock poisoned");
        for tap in taps.iter() {
            tap(id, line.clone());
        }
    }

    /// Record the exit and wake every waiter. Whether a model-facing read
    /// was blocked at this instant is fixed here, before any waiter resumes.
    fn record_exit(&self, code: Option<i32>) {
        *self.exit_code.lock().expect("exit lock poisoned") = Some(code);
        self.exit_collected
            .store(self.collectors.load(Ordering::SeqCst) > 0, Ordering::SeqCst);
        let _ = self.exit.send(Some(code));
        self.touch();
    }

    fn push(&self, stream: OutputStream, data: &[u8]) {
        let mut buf = self.buffer.lock().expect("buffer lock poisoned");
        if let Some(tx) = self
            .foreground
            .lock()
            .expect("foreground lock poisoned")
            .as_ref()
        {
            let _ = tx.send(OutputChunk {
                stream,
                bytes: data.to_vec(),
            });
        }
        buf.extend_from_slice(data);
        let overflow = buf.len().saturating_sub(MAX_BUFFER_BYTES);
        if overflow > 0 {
            buf.drain(..overflow);
            self.total_drained
                .fetch_add(overflow as u64, Ordering::Relaxed);
        }
        self.total_bytes
            .fetch_add(data.len() as u64, Ordering::Relaxed);
        self.touch();
    }

    fn touch(&self) {
        *self.last_activity.lock().expect("activity lock poisoned") = Instant::now();
    }
}

/// Command wrapper for sandboxed background tasks: builds the seatbelt
/// command for a task line + cwd (the registry then adds process-group and
/// pipes).
pub type SandboxCommandBuilder = Arc<dyn Fn(&str, &Path) -> tokio::process::Command + Send + Sync>;

/// Receiving half of a foreground call's output: every chunk the task
/// produces until the call releases it ([`BackgroundRegistry::release_foreground`]).
pub type ForegroundOutput = mpsc::UnboundedReceiver<OutputChunk>;

/// Default `BackgroundTaskRegistry`: `sh -c` processes with ring-buffered
/// output, per-task read cursors, and best-effort process-group kill.
///
/// A host may inject a sandbox wrapper (`with_sandbox`): then
/// `spawn_sandboxed` / `spawn_with_line_events` wrap the command in the
/// seatbelt while the trait `spawn` stays unconfined — escalation is the
/// caller's choice. A host may also inject the unconfined builder
/// (`with_unconfined`) so an escalated task runs in the same shell and
/// environment as the host's unconfined foreground backend.
pub struct BackgroundRegistry {
    tasks: Mutex<std::collections::HashMap<String, Arc<TaskEntry>>>,
    /// Optional seatbelt wrapper for sandboxed background tasks.
    sandbox: Option<SandboxCommandBuilder>,
    /// Optional builder for unconfined tasks; absent, a bare `sh -c`.
    unconfined: Option<SandboxCommandBuilder>,
}

impl BackgroundRegistry {
    pub fn new() -> Self {
        BackgroundRegistry {
            tasks: Mutex::new(std::collections::HashMap::new()),
            sandbox: None,
            unconfined: None,
        }
    }

    /// Inject the unconfined command builder (escalated tasks).
    pub fn with_unconfined(mut self, build: SandboxCommandBuilder) -> Self {
        self.unconfined = Some(build);
        self
    }

    fn unconfined_command(&self, command: &str, cwd: &Path) -> tokio::process::Command {
        match &self.unconfined {
            Some(build) => build(command, cwd),
            None => {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(command).current_dir(cwd);
                c
            }
        }
    }

    /// Inject the sandbox wrapper: background tasks spawned through the
    /// sandboxed paths run inside it (writes + network confined per the
    /// wrapped policy).
    pub fn with_sandbox(mut self, wrap: SandboxCommandBuilder) -> Self {
        self.sandbox = Some(wrap);
        self
    }

    /// Build the command for a sandboxed background task: the injected
    /// wrapper when present, else a bare `sh -c`.
    fn sandboxed_command(&self, command: &str, cwd: &Path) -> tokio::process::Command {
        match &self.sandbox {
            Some(wrap) => wrap(command, cwd),
            None => {
                let mut c = tokio::process::Command::new("sh");
                c.arg("-c").arg(command).current_dir(cwd);
                c
            }
        }
    }

    /// Sweep tasks that exited long enough ago to be irrelevant.
    fn gc(&self) {
        let now = Instant::now();
        let mut tasks = self.tasks.lock().expect("tasks lock poisoned");
        tasks.retain(|_, e| {
            e.exit_code.lock().expect("exit lock poisoned").is_none()
                || now.duration_since(*e.last_activity.lock().expect("activity lock poisoned"))
                    < GC_AFTER_EXIT
        });
    }

    /// Spawn a background command with a per-line stdout callback.
    ///
    /// Same process management as `spawn()` (process group, ring buffer, wait
    /// reaping), but each stdout line also triggers `on_output`. The ring
    /// buffer still accumulates so the task remains observable via `poll()` /
    /// `status()`. Runs through the sandbox wrapper when configured.
    pub fn spawn_with_line_events(
        &self,
        command: &str,
        cwd: &Path,
        on_output: LineEventCallback,
    ) -> Result<crate::core::TaskId, crate::core::TaskError> {
        self.spawn_tracked(command, cwd, true, Some(on_output))
    }

    /// Line-event variant of the escalated (unconfined) `spawn` path.
    pub fn spawn_escalated_with_line_events(
        &self,
        command: &str,
        cwd: &Path,
        on_output: LineEventCallback,
    ) -> Result<crate::core::TaskId, crate::core::TaskError> {
        self.spawn_tracked(command, cwd, false, Some(on_output))
    }

    /// Attach a per-line stdout tap to an already-spawned task. Only lines
    /// produced after the install reach the tap (the ring buffer and read
    /// cursor are unaffected), so a caller that promotes a foreground
    /// command to a background task seeds the card with the retained tail
    /// and taps from there.
    pub fn add_line_tap(
        &self,
        id: &crate::core::TaskId,
        tap: LineEventCallback,
    ) -> Result<(), crate::core::TaskError> {
        let entry = self
            .tasks
            .lock()
            .expect("tasks lock poisoned")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| crate::core::TaskError::NotFound(id.0.clone()))?;
        entry
            .line_taps
            .lock()
            .expect("taps lock poisoned")
            .push(tap);
        Ok(())
    }

    /// Shared spawn core: build the child (sandboxed or bare), register the
    /// task, install the optional initial line tap, and drain its output
    /// into the ring buffer.
    fn spawn_tracked(
        &self,
        command: &str,
        cwd: &Path,
        sandboxed: bool,
        initial_tap: Option<LineEventCallback>,
    ) -> Result<crate::core::TaskId, crate::core::TaskError> {
        self.spawn_entry(command, cwd, sandboxed, initial_tap, None)
    }

    fn spawn_entry(
        &self,
        command: &str,
        cwd: &Path,
        sandboxed: bool,
        initial_tap: Option<LineEventCallback>,
        foreground: Option<mpsc::UnboundedSender<OutputChunk>>,
    ) -> Result<crate::core::TaskId, crate::core::TaskError> {
        self.gc();
        let mut child = if sandboxed {
            self.sandboxed_command(command, cwd)
        } else {
            self.unconfined_command(command, cwd)
        };
        child
            .process_group(0)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = child
            .spawn()
            .map_err(|e| crate::core::TaskError::Spawn(format!("{e}")))?;
        let pid = child.id().map(|p| p as i32).unwrap_or(-1);
        let id = crate::core::TaskId(format!("bg_{}", crate::ext::next_task_ordinal()));
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let entry = Arc::new(TaskEntry::new(child, pid));
        *entry.foreground.lock().expect("foreground lock poisoned") = foreground;
        if let Some(tap) = initial_tap {
            entry
                .line_taps
                .lock()
                .expect("taps lock poisoned")
                .push(tap);
        }
        self.tasks
            .lock()
            .expect("tasks lock poisoned")
            .insert(id.0.clone(), Arc::clone(&entry));
        tokio::spawn(drain_task(stdout, stderr, entry, id.clone()));
        Ok(id)
    }

    /// Synchronous core of `kill`: signal the task's process group without
    /// awaiting. Usable from non-async contexts (a `Drop` teardown cannot
    /// await); the drain task's `wait()` still reaps the child.
    ///
    /// An exited task's group id may have been recycled by the OS; do not
    /// signal it.
    pub fn kill_sync(&self, id: &crate::core::TaskId) -> Result<(), crate::core::TaskError> {
        let entry = self
            .tasks
            .lock()
            .expect("tasks lock poisoned")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| crate::core::TaskError::NotFound(id.0.clone()))?;
        entry.touch();
        if entry
            .exit_code
            .lock()
            .expect("exit lock poisoned")
            .is_some()
        {
            return Ok(());
        }
        let pid = *entry.pid.lock().expect("pid lock poisoned");
        if let Some(pid) = pid {
            // Negative pid signals the whole process group.
            #[cfg(unix)]
            unsafe {
                let _ = libc::kill(-pid, libc::SIGKILL);
            }
        }
        Ok(())
    }
}

impl Default for BackgroundRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Non-consuming snapshot of a task: unlike `poll`, `status` does not
/// advance the read cursor, so an orchestrator can watch for completion and
/// build a summary without stealing output the model still expects to fetch
/// via `BashOutput`.
#[derive(Debug, Clone)]
pub struct TaskStatusInfo {
    pub is_running: bool,
    pub exit_code: Option<Option<i32>>,
    /// Tail of the accumulated output, truncated to `max_tail_bytes`.
    pub output_tail: String,
}

impl BackgroundRegistry {
    pub fn status(
        &self,
        id: &crate::core::TaskId,
        max_tail_bytes: usize,
    ) -> Result<TaskStatusInfo, crate::core::TaskError> {
        let entry = self
            .tasks
            .lock()
            .expect("tasks lock poisoned")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| crate::core::TaskError::NotFound(id.0.clone()))?;
        entry.touch();
        let buf = entry.buffer.lock().expect("buffer lock poisoned");
        let start = buf.len().saturating_sub(max_tail_bytes);
        let output_tail = String::from_utf8_lossy(&buf[start..]).to_string();
        let exit_code = *entry.exit_code.lock().expect("exit lock poisoned");
        Ok(TaskStatusInfo {
            is_running: exit_code.is_none(),
            exit_code,
            output_tail,
        })
    }

    /// Resolve when the task's exit code has been recorded. Unlike `poll`,
    /// this does not advance the read cursor; a task that already exited
    /// resolves immediately (the watch carries the current value).
    pub async fn wait_exit(&self, id: &crate::core::TaskId) -> Result<(), crate::core::TaskError> {
        let entry = self.entry(id)?;
        entry.touch();
        await_exit(&entry).await
    }

    /// `wait_exit` on behalf of the model: an exit that lands while this
    /// call is blocked counts as collected by it.
    pub async fn collect_exit(
        &self,
        id: &crate::core::TaskId,
    ) -> Result<(), crate::core::TaskError> {
        let entry = self.entry(id)?;
        entry.touch();
        let _collecting = CollectorGuard::new(&entry);
        await_exit(&entry).await
    }

    /// Whether the task's exit landed while a model-facing read was blocked
    /// on it (`collect_exit`). False while the task runs.
    pub fn exit_collected(&self, id: &crate::core::TaskId) -> bool {
        self.entry(id)
            .is_ok_and(|entry| entry.exit_collected.load(Ordering::SeqCst))
    }

    fn entry(&self, id: &crate::core::TaskId) -> Result<Arc<TaskEntry>, crate::core::TaskError> {
        self.tasks
            .lock()
            .expect("tasks lock poisoned")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| crate::core::TaskError::NotFound(id.0.clone()))
    }
}

async fn await_exit(entry: &TaskEntry) -> Result<(), crate::core::TaskError> {
    let mut rx = entry.exit.subscribe();
    if rx.borrow().is_some() {
        return Ok(());
    }
    loop {
        rx.changed()
            .await
            .map_err(|_| crate::core::TaskError::Other("task dropped before exit".into()))?;
        if rx.borrow().is_some() {
            return Ok(());
        }
    }
}

/// Counts one model-facing read as blocked on a task's exit for as long as
/// it lives, so an expired or cancelled read stops counting the moment it
/// unwinds.
struct CollectorGuard<'a>(&'a TaskEntry);

impl<'a> CollectorGuard<'a> {
    fn new(entry: &'a TaskEntry) -> Self {
        entry.collectors.fetch_add(1, Ordering::SeqCst);
        CollectorGuard(entry)
    }
}

impl Drop for CollectorGuard<'_> {
    fn drop(&mut self) {
        self.0.collectors.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Drain a task's pipes into its ring buffer, emitting each stdout line to
/// the entry's installed taps, then record the exit code.
///
/// stdout is read in raw chunks so the ring buffer keeps the exact byte
/// stream — `poll()` accounting stays identical. Lines are split out of the
/// chunk stream separately for the taps: a partial line carries across
/// chunk boundaries, a trailing line without a final newline is emitted at
/// EOF, and a trailing `\r` from CRLF input is stripped from the emitted
/// line text only (the raw bytes in the ring buffer are untouched). stderr
/// is never tapped — observers follow the command's own narration.
async fn drain_task(
    mut stdout: tokio::process::ChildStdout,
    mut stderr: tokio::process::ChildStderr,
    entry: Arc<TaskEntry>,
    id: crate::core::TaskId,
) {
    use tokio::io::AsyncReadExt;
    let mut out_chunk = [0u8; 8192];
    let mut err_chunk = [0u8; 8192];
    let mut out_done = false;
    let mut err_done = false;
    let mut carry: Vec<u8> = Vec::new();
    let tap = |id: &crate::core::TaskId, line: String| entry.tap_line(id, line);
    while !(out_done && err_done) {
        tokio::select! {
            n = stdout.read(&mut out_chunk), if !out_done => match n {
                Ok(0) | Err(_) => {
                    // EOF: emit a trailing partial line (no final newline).
                    if !carry.is_empty() {
                        let tail = String::from_utf8_lossy(&carry).into_owned();
                        carry.clear();
                        tap(&id, tail);
                    }
                    out_done = true;
                }
                Ok(n) => {
                    let data = &out_chunk[..n];
                    entry.push(OutputStream::Stdout, data);
                    emit_lines(&id, data, &mut carry, &tap);
                }
            },
            n = stderr.read(&mut err_chunk), if !err_done => match n {
                Ok(0) | Err(_) => err_done = true,
                Ok(n) => entry.push(OutputStream::Stderr, &err_chunk[..n]),
            },
        }
    }
    // Take the child handle so the mutex guard drops before the await; a
    // guard held across `.wait()` would make the drain future !Send.
    let mut child = entry.child.lock().expect("child lock poisoned").take();
    let code = match child.as_mut() {
        Some(c) => c.wait().await.ok().map(|s| s.code()),
        None => None,
    }
    .flatten();
    // Wake any waiter the moment the exit is recorded; a watcher awaiting
    // `wait_exit` resumes without polling.
    entry.record_exit(code);
}

#[async_trait::async_trait]
impl BackgroundTaskRegistry for BackgroundRegistry {
    fn spawn(
        &self,
        command: &str,
        cwd: &Path,
    ) -> Result<crate::core::TaskId, crate::core::TaskError> {
        // The trait path stays bare: an escalated background task runs
        // without confinement. Sandboxed backgrounds use `spawn_sandboxed`.
        self.spawn_impl(command, cwd, false)
    }

    async fn poll(
        &self,
        id: &crate::core::TaskId,
    ) -> Result<crate::core::PollResult, crate::core::TaskError> {
        let entry = self
            .tasks
            .lock()
            .expect("tasks lock poisoned")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| crate::core::TaskError::NotFound(id.0.clone()))?;

        entry.touch();

        let buf = entry.buffer.lock().expect("buffer lock poisoned");

        let mut cursor = entry.read_cursor.lock().expect("cursor lock poisoned");
        // The buffer's first byte sits at logical offset `total_drained`, so
        // the read start is the cursor clamped into the retained window.
        let drained = entry.total_drained.load(Ordering::Relaxed);
        let start = cursor.saturating_sub(drained).min(buf.len() as u64) as usize;
        // Read under the buffer lock: the drain pushes every byte before it
        // records the exit, so an exited task's buffer is complete here.
        let exit_code = *entry.exit_code.lock().expect("exit lock poisoned");
        let is_running = exit_code.is_none();
        // A running task may have written only part of a multi-byte
        // character; that partial suffix stays unread until it completes.
        let end = if is_running {
            buf.len() - incomplete_utf8_suffix(&buf[start..])
        } else {
            buf.len()
        };
        let new_output = String::from_utf8_lossy(&buf[start..end]).to_string();
        *cursor = drained + end as u64;
        Ok(crate::core::PollResult {
            new_output,
            is_running,
            exit_code,
            total_bytes: entry.total_bytes.load(Ordering::Relaxed),
        })
    }

    async fn kill(&self, id: &crate::core::TaskId) -> Result<(), crate::core::TaskError> {
        self.kill_sync(id)
    }

    async fn collect_exit(&self, id: &crate::core::TaskId) -> Result<(), crate::core::TaskError> {
        BackgroundRegistry::collect_exit(self, id).await
    }
}

impl BackgroundRegistry {
    /// Spawn a background command inside the sandbox wrapper (when
    /// configured), else bare — same task lifecycle as `spawn`.
    pub fn spawn_sandboxed(
        &self,
        command: &str,
        cwd: &Path,
    ) -> Result<crate::core::TaskId, crate::core::TaskError> {
        self.spawn_impl(command, cwd, true)
    }

    /// Spawn a task a foreground call owns: besides the ring, every chunk
    /// reaches the returned receiver (stdout/stderr kept apart) until the
    /// call either forgets the finished task or releases it to the
    /// background.
    pub fn spawn_foreground(
        &self,
        command: &str,
        cwd: &Path,
        sandboxed: bool,
    ) -> Result<(crate::core::TaskId, ForegroundOutput), crate::core::TaskError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = self.spawn_entry(command, cwd, sandboxed, None, Some(tx))?;
        Ok((id, rx))
    }

    /// End a foreground call's ownership: chunk forwarding stops and the read
    /// cursor moves past everything forwarded so far, so the next `poll`
    /// starts exactly where the call's output ends. The receiver then yields
    /// the chunks still queued and closes.
    pub fn release_foreground(
        &self,
        id: &crate::core::TaskId,
    ) -> Result<(), crate::core::TaskError> {
        let entry = self.entry(id)?;
        let buf = entry.buffer.lock().expect("buffer lock poisoned");
        entry
            .foreground
            .lock()
            .expect("foreground lock poisoned")
            .take();
        *entry.read_cursor.lock().expect("cursor lock poisoned") =
            entry.total_drained.load(Ordering::Relaxed) + buf.len() as u64;
        Ok(())
    }

    /// Drop a task's record. A foreground call that kept its task in the
    /// foreground to the end forgets it: the model never saw the id.
    pub fn forget(&self, id: &crate::core::TaskId) {
        self.tasks
            .lock()
            .expect("tasks lock poisoned")
            .remove(&id.0);
    }

    /// The recorded exit: `None` while running, `Some(None)` for a signal
    /// death, `Some(Some(code))` otherwise.
    pub fn exit_status(
        &self,
        id: &crate::core::TaskId,
    ) -> Result<Option<Option<i32>>, crate::core::TaskError> {
        let entry = self.entry(id)?;
        Ok(*entry.exit_code.lock().expect("exit lock poisoned"))
    }

    /// Spawn core for the no-tap paths: a plain ring-buffered task.
    fn spawn_impl(
        &self,
        command: &str,
        cwd: &Path,
        sandboxed: bool,
    ) -> Result<crate::core::TaskId, crate::core::TaskError> {
        self.spawn_tracked(command, cwd, sandboxed, None)
    }
}

/// Split `data` into lines against `\n`, joining onto any partial line
/// carried over from a previous chunk. Each complete line is emitted without
/// its newline (and without a trailing `\r` from CRLF input); the remainder
/// after the last newline stays in `carry` for the next chunk.
fn emit_lines(
    id: &crate::core::TaskId,
    data: &[u8],
    carry: &mut Vec<u8>,
    on_output: &(dyn Fn(&crate::core::TaskId, String) + Send + Sync),
) {
    let mut start = 0usize;
    for (idx, &byte) in data.iter().enumerate() {
        if byte != b'\n' {
            continue;
        }
        let mut line = std::mem::take(carry);
        line.extend_from_slice(&data[start..idx]);
        start = idx + 1;
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        on_output(id, String::from_utf8_lossy(&line).into_owned());
    }
    if start < data.len() {
        carry.extend_from_slice(&data[start..]);
    }
}

/// Render a poll result for the model.
///
/// An increment is bounded like a foreground command's result: past the
/// default budget only its tail is kept, since a long-running command's
/// signal (the failing step, the final verdict) sits at the end.
fn render_poll(poll: &crate::core::PollResult) -> String {
    let mut out = if poll.new_output.is_empty() {
        "(no new output)".to_string()
    } else {
        let kept = crate::core::tools::truncate::truncate_tail(
            &poll.new_output,
            &crate::core::tools::truncate::TruncateConfig::default(),
        );
        if kept.was_truncated {
            format!(
                "[earlier output of this read omitted: {} lines, {} bytes in total]\n{}",
                kept.original_lines, kept.original_bytes, kept.content
            )
        } else {
            kept.content
        }
    };
    if poll.is_running {
        out.push_str(&format!("\n\n[running; {} bytes total]", poll.total_bytes));
    } else {
        match poll.exit_code {
            Some(Some(code)) => out.push_str(&format!("\n\n[exited with code {code}]")),
            Some(None) => out.push_str("\n\n[terminated by signal]"),
            None => {}
        }
    }
    out
}

/// Default duration of a blocking `BashOutput` wait when `timeout_ms` is
/// absent. The tool layer has no per-tool config surface, so the default
/// and the cap are compile constants here.
const WAIT_DEFAULT_MS: u64 = 30_000;
/// Hard cap on a blocking `BashOutput` wait; larger `timeout_ms` values are
/// clamped down. An expired wait never kills the task — it only ends the
/// read.
const WAIT_MAX_MS: u64 = 600_000;

/// `BashOutput` tool — poll a background task for output since the last poll,
/// optionally blocking until it finishes.
pub struct BashOutputTool {
    registry: Arc<dyn BackgroundTaskRegistry>,
}

impl BashOutputTool {
    pub fn new(registry: Arc<dyn BackgroundTaskRegistry>) -> Self {
        BashOutputTool { registry }
    }
}

#[async_trait::async_trait]
impl AgentTool for BashOutputTool {
    fn name(&self) -> &str {
        "BashOutput"
    }

    fn description(&self) -> &str {
        "Fetch incremental output from a background task started with `run_in_background: true`. \
         Each call returns only the output produced since the last read. Set `wait: true` to \
         block until the task finishes or the wait expires — the task keeps running on expiry."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "shell_id": {
                    "type": "string",
                    "description": "The background task id returned by bash"
                },
                "wait": {
                    "type": "boolean",
                    "description": "Block until the task finishes or the wait times out. Defaults to false."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Max wait in milliseconds with wait: true. Defaults to 30000, capped at 600000. An expired wait leaves the task running."
                }
            },
            "required": ["shell_id"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        signal: CancellationToken,
        _ctx: &dyn crate::core::tool::ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let shell_id = params["shell_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("shell_id is required".into()))?;
        let id = crate::core::TaskId(shell_id.to_string());
        let mut expired_after_secs = None;
        if params["wait"].as_bool().unwrap_or(false) {
            let timeout_ms = params["timeout_ms"]
                .as_u64()
                .unwrap_or(WAIT_DEFAULT_MS)
                .clamp(1, WAIT_MAX_MS);
            let started = std::time::Instant::now();
            // A cancelled read ends without consuming anything: the task is
            // never touched and its pending output stays for the next read.
            let waited = tokio::select! {
                outcome = tokio::time::timeout(
                    Duration::from_millis(timeout_ms),
                    self.registry.collect_exit(&id),
                ) => outcome,
                () = signal.cancelled() => {
                    return Err(ToolError::ExecutionFailed(format!(
                        "wait on `{shell_id}` cancelled; the task keeps running and its output is unread"
                    )));
                }
            };
            match waited {
                Err(_) => expired_after_secs = Some(started.elapsed().as_secs()),
                Ok(Err(e)) => return Err(not_found_hint(e)),
                Ok(Ok(())) => {}
            }
        }
        let poll = self.registry.poll(&id).await.map_err(not_found_hint)?;
        let mut out = render_poll(&poll);
        if let Some(secs) = expired_after_secs {
            out.push_str(&format!(
                "\n\n[still running; waited {secs}s — call again with wait to keep waiting, TaskStop to stop]"
            ));
        }
        Ok(AgentToolResult::text(out))
    }
}

/// Length of a trailing UTF-8 sequence that is cut short (0 when the slice
/// ends on a character boundary or with bytes that can never complete).
fn incomplete_utf8_suffix(bytes: &[u8]) -> usize {
    (1..=bytes.len().min(3))
        .find(|&n| {
            matches!(
                std::str::from_utf8(&bytes[bytes.len() - n..]),
                Err(e) if e.valid_up_to() == 0 && e.error_len().is_none()
            )
        })
        .unwrap_or(0)
}

/// A read on an unknown id names what `BashOutput` can read, since
/// `TaskList` also shows subagents that have no output stream here.
fn not_found_hint(e: crate::core::TaskError) -> ToolError {
    match e {
        crate::core::TaskError::NotFound(id) => ToolError::ExecutionFailed(format!(
            "task not found: {id} — BashOutput reads background Bash tasks of this session \
             (see TaskList); a subagent's report arrives as a message when it finishes"
        )),
        e => ToolError::ExecutionFailed(format!("{e}")),
    }
}

/// `TaskStop` tool — terminate a background task's process group.
pub struct TaskStopTool {
    registry: Arc<dyn BackgroundTaskRegistry>,
}

impl TaskStopTool {
    pub fn new(registry: Arc<dyn BackgroundTaskRegistry>) -> Self {
        TaskStopTool { registry }
    }
}

#[async_trait::async_trait]
impl AgentTool for TaskStopTool {
    fn name(&self) -> &str {
        "TaskStop"
    }

    fn description(&self) -> &str {
        "Stop a background task started with `run_in_background: true`. Kills its process group."
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "The background task id to stop"
                }
            },
            "required": ["task_id"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        _signal: CancellationToken,
        _ctx: &dyn crate::core::tool::ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let task_id = params["task_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("task_id is required".into()))?;
        match self
            .registry
            .kill(&crate::core::TaskId(task_id.to_string()))
            .await
        {
            Ok(()) => Ok(AgentToolResult::text(format!(
                "Stopped background task `{task_id}`"
            ))),
            Err(e) => Err(ToolError::ExecutionFailed(format!("{e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_poll_kill_lifecycle() {
        let registry = BackgroundRegistry::new();
        // A long-running command: spawn returns immediately, and the drain
        // task reads the pipes asynchronously — wait for the first chunk
        // rather than racing it.
        let id = registry
            .spawn("echo started; sleep 30", Path::new("/tmp"))
            .unwrap();
        let mut saw_started = false;
        for _ in 0..50 {
            let poll = registry.poll(&id).await.unwrap();
            if poll.new_output.contains("started") {
                saw_started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(saw_started, "initial output observed");
        assert!(registry.poll(&id).await.unwrap().is_running);
        registry.kill(&id).await.unwrap();
        // After the kill the exit status is recorded; a final poll observes it.
        for _ in 0..50 {
            if registry.poll(&id).await.unwrap().exit_code.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let poll2 = registry.poll(&id).await.unwrap();
        assert!(!poll2.is_running);
        assert!(poll2.exit_code.is_some(), "exit code recorded: {poll2:?}");
    }

    #[tokio::test]
    async fn poll_returns_only_incremental_output() {
        let registry = BackgroundRegistry::new();
        let id = registry
            .spawn("echo one; sleep 0.2; echo two", Path::new("/tmp"))
            .unwrap();
        let mut saw_one = false;
        let mut saw_two = false;
        // Bounded so a stuck drain fails the test instead of hanging it.
        for _ in 0..100 {
            let poll = registry.poll(&id).await.unwrap();
            if poll.new_output.contains("one") {
                saw_one = true;
            }
            if poll.new_output.contains("two") {
                saw_two = true;
            }
            if !poll.is_running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(saw_one, "first command output observed");
        assert!(saw_two, "second command output observed");
    }

    #[tokio::test]
    async fn poll_survives_ring_buffer_overflow() {
        let registry = BackgroundRegistry::new();
        // Output far past the ring cap, then a tail marker: the logical read
        // cursor must keep incrementing past front-dropped bytes.
        let id = registry
            .spawn(
                "head -c 400000 /dev/zero | tr '\\0' a; echo MARKER; sleep 0.2; echo TAIL",
                Path::new("/tmp"),
            )
            .unwrap();
        let mut saw_tail = false;
        for _ in 0..200 {
            let poll = registry.poll(&id).await.unwrap();
            if poll.new_output.contains("TAIL") {
                saw_tail = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(saw_tail, "output after overflow is still observable");
    }
}

#[cfg(test)]
mod line_tap_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A tap installed after spawn sees only the lines produced from the
    /// install onward; the ring buffer and read cursor are unaffected.
    #[tokio::test]
    async fn add_line_tap_sees_only_post_install_lines() {
        let registry = BackgroundRegistry::new();
        let id = registry
            .spawn("echo before; sleep 0.3; echo after", Path::new("/tmp"))
            .unwrap();
        // Wait until the pre-install line is in the ring: line framing
        // happens in the same drain step as the push, so `before` is fully
        // framed before the tap exists.
        let mut saw_before = false;
        for _ in 0..100 {
            let poll = registry.poll(&id).await.unwrap();
            if poll.new_output.contains("before") {
                saw_before = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(saw_before, "pre-install line observed");

        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&lines);
        registry
            .add_line_tap(
                &id,
                Box::new(move |_id, line| sink.lock().unwrap().push(line)),
            )
            .unwrap();
        registry.wait_exit(&id).await.unwrap();

        let got = lines.lock().unwrap().clone();
        assert!(
            got.iter().any(|l| l.contains("after")),
            "post-install line reached the tap: {got:?}"
        );
        assert!(
            !got.iter().any(|l| l.contains("before")),
            "pre-install line must not reach a later tap: {got:?}"
        );
    }
}

#[cfg(test)]
mod wait_exit_tests {
    use super::*;

    #[tokio::test]
    async fn wait_exit_resolves_on_completion() {
        let registry = BackgroundRegistry::new();
        let id = registry.spawn("sleep 0.1", Path::new("/tmp")).unwrap();
        // Event-driven: resolves as soon as the drain records the exit.
        tokio::time::timeout(Duration::from_secs(5), registry.wait_exit(&id))
            .await
            .expect("wait_exit must resolve")
            .unwrap();
        assert!(!registry.status(&id, 0).unwrap().is_running);
    }

    #[tokio::test]
    async fn wait_exit_resolves_immediately_for_finished_task() {
        let registry = BackgroundRegistry::new();
        let id = registry.spawn("echo done", Path::new("/tmp")).unwrap();
        registry.wait_exit(&id).await.unwrap();
        // A second wait sees the watch's current value and returns at once.
        let start = std::time::Instant::now();
        registry.wait_exit(&id).await.unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}

#[cfg(test)]
mod task_stop_tests {
    use super::*;
    use crate::core::tool::{AgentTool, LocalToolContext, ToolState};
    use std::sync::Arc;

    fn ctx() -> LocalToolContext {
        LocalToolContext::new(
            Arc::new(crate::core::env::TokioExecutionEnv::new(
                std::env::temp_dir(),
            )),
            std::env::temp_dir(),
            Arc::new(ToolState::new()),
        )
    }

    #[tokio::test]
    async fn task_stop_kills_a_running_task() {
        let registry = Arc::new(BackgroundRegistry::new());
        let tool = TaskStopTool::new(Arc::clone(&registry) as Arc<dyn BackgroundTaskRegistry>);
        let id = registry.spawn("sleep 30", Path::new("/tmp")).unwrap();
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"task_id": id.0}),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        // The kill lands via SIGKILL; wait for the recorded exit.
        let mut stopped = false;
        for _ in 0..50 {
            if !registry.status(&id, 0).unwrap().is_running {
                stopped = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(stopped, "task stopped through the registry");
    }

    /// Without a fallback, an unknown id surfaces a not-found error rather
    /// than silently succeeding.
    #[tokio::test]
    async fn task_stop_unknown_id_errors() {
        let registry = Arc::new(BackgroundRegistry::new());
        let tool = TaskStopTool::new(Arc::clone(&registry) as Arc<dyn BackgroundTaskRegistry>);
        let err = tool
            .execute(
                "c1",
                serde_json::json!({"task_id": "bg_404"}),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("not found"), "got: {err}");
    }
}

#[cfg(test)]
mod bash_output_wait_tests {
    use super::*;
    use crate::core::tool::{AgentTool, LocalToolContext, ToolState};
    use std::sync::Arc;

    fn ctx() -> LocalToolContext {
        LocalToolContext::new(
            Arc::new(crate::core::env::TokioExecutionEnv::new(
                std::env::temp_dir(),
            )),
            std::env::temp_dir(),
            Arc::new(ToolState::new()),
        )
    }

    fn tool(registry: &Arc<BackgroundRegistry>) -> BashOutputTool {
        BashOutputTool::new(Arc::clone(registry) as Arc<dyn BackgroundTaskRegistry>)
    }

    fn spawn(registry: &Arc<BackgroundRegistry>, command: &str) -> crate::core::TaskId {
        registry.spawn(command, Path::new("/tmp")).expect("spawn")
    }

    fn text(result: crate::core::tool::AgentToolResult) -> String {
        result
            .content
            .into_iter()
            .find_map(|b| match b {
                crate::core::types::ContentBlock::Text { text, .. } => Some(text),
                _ => None,
            })
            .expect("text content")
    }

    /// A wait on a task that finishes within the budget returns its final
    /// increment and exit code, without the still-running note.
    #[tokio::test]
    async fn wait_returns_when_the_task_finishes() {
        let registry = Arc::new(BackgroundRegistry::new());
        let id = spawn(&registry, "echo done; sleep 0.1; echo more");
        let result = tool(&registry)
            .execute(
                "c1",
                serde_json::json!({"shell_id": id.0, "wait": true, "timeout_ms": 10_000}),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap();
        let out = text(result);
        assert!(out.contains("done"), "output observed: {out}");
        assert!(
            !out.contains("still running"),
            "finished within wait: {out}"
        );
    }

    /// An expired wait leaves the task running: the result carries the
    /// increment so far plus the still-running note.
    #[tokio::test]
    async fn expired_wait_leaves_the_task_running() {
        let registry = Arc::new(BackgroundRegistry::new());
        let id = spawn(&registry, "sleep 30");
        let started = std::time::Instant::now();
        let result = tool(&registry)
            .execute(
                "c1",
                serde_json::json!({"shell_id": id.0, "wait": true, "timeout_ms": 300}),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap();
        let out = text(result);
        assert!(out.contains("still running"), "expiry note present: {out}");
        assert!(started.elapsed() >= Duration::from_millis(300));
        assert!(
            registry.status(&id, 0).unwrap().is_running,
            "the task must survive an expired wait"
        );
        registry.kill(&id).await.unwrap();
    }

    /// A wait on an already-finished task returns immediately.
    #[tokio::test]
    async fn wait_on_finished_task_returns_immediately() {
        let registry = Arc::new(BackgroundRegistry::new());
        let id = spawn(&registry, "true");
        registry.wait_exit(&id).await.unwrap();
        let started = std::time::Instant::now();
        let result = tool(&registry)
            .execute(
                "c1",
                serde_json::json!({"shell_id": id.0, "wait": true, "timeout_ms": 10_000}),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(text(result).contains("[exited with code 0]"));
    }

    /// A cancelled read only ends the wait: the task keeps running, no kill
    /// is issued, and the output produced so far stays unread.
    #[tokio::test]
    async fn cancelled_read_leaves_the_task_running() {
        let registry = Arc::new(BackgroundRegistry::new());
        let id = spawn(&registry, "echo pending; sleep 30");
        let signal = CancellationToken::new();
        let read_signal = signal.clone();
        let registry_arc = Arc::clone(&registry);
        let shell_id = id.0.clone();
        let handle = tokio::spawn(async move {
            tool(&registry_arc)
                .execute(
                    "c1",
                    serde_json::json!({"shell_id": shell_id, "wait": true, "timeout_ms": 60_000}),
                    read_signal,
                    &ctx(),
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        signal.cancel();
        // The read must unwind well before the 60s cap.
        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("read unwound")
            .expect("read task joined");
        let err = result.expect_err("a cancelled read reports the cancellation");
        assert!(format!("{err}").contains("keeps running"), "got: {err}");
        // The task was never killed by the cancelled read.
        assert!(registry.status(&id, 0).unwrap().is_running);
        let poll = registry.poll(&id).await.unwrap();
        assert!(
            poll.new_output.contains("pending"),
            "the cancelled read consumed nothing: {poll:?}"
        );
        registry.kill(&id).await.unwrap();
    }

    /// An unknown id points the model at what `BashOutput` can read.
    #[tokio::test]
    async fn unknown_id_names_the_readable_kinds() {
        let registry = Arc::new(BackgroundRegistry::new());
        let err = tool(&registry)
            .execute(
                "c1",
                serde_json::json!({"shell_id": "subagent_3"}),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("task not found: subagent_3"), "got: {msg}");
        assert!(msg.contains("TaskList"), "got: {msg}");
    }

    /// A single read is bounded like a foreground result: past the budget
    /// only the tail of the increment comes back, with an omission marker.
    #[tokio::test]
    async fn oversized_read_keeps_the_tail() {
        let registry = Arc::new(BackgroundRegistry::new());
        let id = spawn(&registry, "seq 1 5000");
        let out = text(
            tool(&registry)
                .execute(
                    "c1",
                    serde_json::json!({"shell_id": id.0, "wait": true, "timeout_ms": 10_000}),
                    CancellationToken::new(),
                    &ctx(),
                )
                .await
                .unwrap(),
        );
        assert!(
            out.starts_with("[earlier output of this read omitted: "),
            "omission marker first: {}",
            &out[..out.len().min(120)]
        );
        assert!(
            out.contains("\n5000\n"),
            "tail kept: {}",
            &out[out.len().saturating_sub(80)..]
        );
        assert!(!out.contains("\n1\n2\n3\n"), "head dropped");
        assert!(out.contains("[exited with code 0]"));
    }

    /// A multi-byte character split across two writes is never read in
    /// halves: the partial suffix waits for the rest of the character.
    #[tokio::test]
    async fn poll_holds_back_a_partial_utf8_character() {
        let registry = Arc::new(BackgroundRegistry::new());
        let id = spawn(
            &registry,
            r"printf '\344\270'; sleep 0.4; printf '\255\346\226\207\n'",
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
        let first = registry.poll(&id).await.unwrap();
        assert_eq!(first.new_output, "", "half a character is held back");
        registry.wait_exit(&id).await.unwrap();
        let second = registry.poll(&id).await.unwrap();
        assert_eq!(second.new_output, "中文\n");
    }

    #[test]
    fn incomplete_utf8_suffix_measures_only_a_cut_sequence() {
        assert_eq!(incomplete_utf8_suffix(b"ab"), 0);
        assert_eq!(incomplete_utf8_suffix("中".as_bytes()), 0);
        assert_eq!(incomplete_utf8_suffix(&"中".as_bytes()[..2]), 2);
        assert_eq!(incomplete_utf8_suffix(&"a中".as_bytes()[..2]), 1);
        assert_eq!(incomplete_utf8_suffix(b"a\xff"), 0);
        assert_eq!(incomplete_utf8_suffix(b""), 0);
    }
}

#[cfg(test)]
mod line_framing_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Chunk-boundary line framing: partial lines carry across chunks, a
    /// trailing line without a newline is emitted at EOF, and CRLF lines
    /// lose the `\r` in the emitted text only.
    #[tokio::test]
    async fn line_events_handle_crlf_and_trailing_partial_line() {
        let registry = BackgroundRegistry::new();
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let lines_c = Arc::clone(&lines);
        let id = registry
            .spawn_with_line_events(
                r"printf 'alpha\r\nbeta\r\n'; printf 'no-newline-tail'",
                Path::new("/tmp"),
                Box::new(move |_id, line| lines_c.lock().unwrap().push(line)),
            )
            .unwrap();
        registry.wait_exit(&id).await.unwrap();

        let got = lines.lock().unwrap().clone();
        assert_eq!(got, vec!["alpha", "beta", "no-newline-tail"]);

        // The ring buffer keeps the raw byte stream (CRLF intact), so
        // `poll()` accounting matches the process output exactly.
        let poll = registry.poll(&id).await.unwrap();
        assert!(
            poll.new_output.contains("alpha\r\nbeta\r\n"),
            "raw bytes preserved: {:?}",
            poll.new_output
        );
        assert!(poll.new_output.contains("no-newline-tail"));
        assert_eq!(
            poll.total_bytes,
            "alpha\r\nbeta\r\nno-newline-tail".len() as u64
        );
    }

    /// `emit_lines` across chunk boundaries: a half line carries over, and
    /// multiple lines in one chunk all emit in order.
    #[test]
    fn emit_lines_carries_partial_lines_across_chunks() {
        let id = crate::core::TaskId("bg_test".into());
        let got: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut carry: Vec<u8> = Vec::new();
        let on_output = {
            let got = Arc::clone(&got);
            move |_id: &crate::core::TaskId, line: String| got.lock().unwrap().push(line)
        };

        emit_lines(&id, b"fir", &mut carry, &on_output);
        assert!(got.lock().unwrap().is_empty(), "no newline yet");
        emit_lines(&id, b"st\nsecond\r\nth", &mut carry, &on_output);
        emit_lines(&id, b"ird", &mut carry, &on_output);
        assert_eq!(
            *got.lock().unwrap(),
            vec!["first".to_string(), "second".to_string()]
        );
        // The trailing partial line stays in the carry for the EOF path.
        assert_eq!(carry, b"third");
    }
}

#[cfg(test)]
mod sandbox_tests {
    use super::*;

    #[tokio::test]
    async fn spawn_sandboxed_uses_the_injected_wrapper() {
        let wrapped = Arc::new(AtomicU64::new(0));
        let wrapped2 = Arc::clone(&wrapped);
        let wrap: SandboxCommandBuilder = Arc::new(move |command, cwd| {
            wrapped2.fetch_add(1, Ordering::Relaxed);
            let mut c = tokio::process::Command::new("printf");
            c.arg("wrapped").current_dir(cwd);
            let _ = command;
            c
        });
        let registry = BackgroundRegistry::new().with_sandbox(wrap);
        let id = registry
            .spawn_sandboxed("anything", Path::new("/tmp"))
            .unwrap();
        // Poll until the wrapped `printf wrapped` output lands.
        let mut out = String::new();
        for _ in 0..50 {
            let poll = registry.poll(&id).await.unwrap();
            out.push_str(&poll.new_output);
            if out.contains("wrapped") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(out.contains("wrapped"), "wrapped command ran: {out}");
        assert_eq!(wrapped.load(Ordering::Relaxed), 1, "wrapper invoked once");
    }

    #[tokio::test]
    async fn trait_spawn_stays_bare_with_a_wrapper_installed() {
        let wrapped = Arc::new(AtomicU64::new(0));
        let wrapped2 = Arc::clone(&wrapped);
        let wrap: SandboxCommandBuilder = Arc::new(move |command, cwd| {
            wrapped2.fetch_add(1, Ordering::Relaxed);
            let mut c = tokio::process::Command::new("printf");
            c.arg("bare").current_dir(cwd);
            let _ = command;
            c
        });
        let registry = BackgroundRegistry::new().with_sandbox(wrap);
        let id = registry.spawn("printf bare", Path::new("/tmp")).unwrap();
        let mut out = String::new();
        for _ in 0..50 {
            let poll = registry.poll(&id).await.unwrap();
            out.push_str(&poll.new_output);
            if out.contains("bare") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(out.contains("bare"), "bare sh ran: {out}");
        assert_eq!(
            wrapped.load(Ordering::Relaxed),
            0,
            "trait spawn must not route through the wrapper"
        );
    }
}
