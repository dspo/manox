// Background task registry implementation and the poll/stop tools built on
// it. A background task is a `sh -c` process whose output accumulates in a
// ring buffer; `poll` returns only the increment since the last poll, so the
// model can follow a long-running command without re-reading history.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pi::BackgroundTaskRegistry;
use pi::tool::{AgentTool, AgentToolResult, ToolError};
use serde_json::Value as JsonValue;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Per-line stdout callback for line-event monitors.
pub type LineEventCallback = Box<dyn Fn(&pi::TaskId, String) + Send + Sync + 'static>;
/// Exit callback for line-event monitors: the outer `Option` is `None` when
/// the child handle was gone, the inner one is `None` for signal deaths.
pub type ExitEventCallback = Box<dyn Fn(&pi::TaskId, Option<Option<i32>>) + Send + Sync + 'static>;

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
        }
    }

    fn push(&self, data: &[u8]) {
        let mut buf = self.buffer.lock().expect("buffer lock poisoned");
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

/// Default `BackgroundTaskRegistry`: `sh -c` processes with ring-buffered
/// output, per-task read cursors, and best-effort process-group kill.
pub struct BackgroundRegistry {
    tasks: Mutex<std::collections::HashMap<String, Arc<TaskEntry>>>,
    next_id: AtomicU64,
}

impl BackgroundRegistry {
    pub fn new() -> Self {
        BackgroundRegistry {
            tasks: Mutex::new(std::collections::HashMap::new()),
            next_id: AtomicU64::new(0),
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

    /// Spawn a background command with per-line callbacks.
    ///
    /// Same process management as `spawn()` (process group, ring buffer, wait
    /// reaping), but each stdout line also triggers `on_output` and the exit
    /// triggers `on_exit`. The ring buffer still accumulates so the task
    /// remains observable via `poll()` / `status()`.
    pub fn spawn_with_line_events(
        &self,
        command: &str,
        cwd: &Path,
        on_output: LineEventCallback,
        on_exit: ExitEventCallback,
    ) -> Result<pi::TaskId, pi::TaskError> {
        self.gc();
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .process_group(0)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| pi::TaskError::Spawn(format!("{e}")))?;
        let pid = child.id().map(|p| p as i32).unwrap_or(-1);
        let id = pi::TaskId(format!(
            "mon_{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ));
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let entry = Arc::new(TaskEntry::new(child, pid));
        self.tasks
            .lock()
            .expect("tasks lock poisoned")
            .insert(id.0.clone(), Arc::clone(&entry));
        tokio::spawn(drain_task_with_line_events(
            stdout,
            stderr,
            entry,
            id.clone(),
            on_output,
            on_exit,
        ));
        Ok(id)
    }

    /// Synchronous core of `kill`: signal the task's process group without
    /// awaiting. Usable from non-async contexts (a `Drop` teardown cannot
    /// await); the drain task's `wait()` still reaps the child.
    ///
    /// An exited task's group id may have been recycled by the OS; do not
    /// signal it.
    pub fn kill_sync(&self, id: &pi::TaskId) -> Result<(), pi::TaskError> {
        let entry = self
            .tasks
            .lock()
            .expect("tasks lock poisoned")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| pi::TaskError::NotFound(id.0.clone()))?;
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
/// via `bash_output`.
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
        id: &pi::TaskId,
        max_tail_bytes: usize,
    ) -> Result<TaskStatusInfo, pi::TaskError> {
        let entry = self
            .tasks
            .lock()
            .expect("tasks lock poisoned")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| pi::TaskError::NotFound(id.0.clone()))?;
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
    pub async fn wait_exit(&self, id: &pi::TaskId) -> Result<(), pi::TaskError> {
        let entry = self
            .tasks
            .lock()
            .expect("tasks lock poisoned")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| pi::TaskError::NotFound(id.0.clone()))?;
        entry.touch();
        let mut rx = entry.exit.subscribe();
        if rx.borrow().is_some() {
            return Ok(());
        }
        loop {
            rx.changed()
                .await
                .map_err(|_| pi::TaskError::Other("task dropped before exit".into()))?;
            if rx.borrow().is_some() {
                return Ok(());
            }
        }
    }
}

/// Drain a task's pipes into its ring buffer, then record the exit code.
async fn drain_task(
    mut stdout: tokio::process::ChildStdout,
    mut stderr: tokio::process::ChildStderr,
    entry: Arc<TaskEntry>,
) {
    use tokio::io::AsyncReadExt;
    let mut out_chunk = [0u8; 8192];
    let mut err_chunk = [0u8; 8192];
    let mut out_done = false;
    let mut err_done = false;
    while !(out_done && err_done) {
        tokio::select! {
            n = stdout.read(&mut out_chunk), if !out_done => match n {
                Ok(0) | Err(_) => out_done = true,
                Ok(n) => entry.push(&out_chunk[..n]),
            },
            n = stderr.read(&mut err_chunk), if !err_done => match n {
                Ok(0) | Err(_) => err_done = true,
                Ok(n) => entry.push(&err_chunk[..n]),
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
    *entry.exit_code.lock().expect("exit lock poisoned") = Some(code);
    // Wake any waiter the moment the exit is recorded; a watcher awaiting
    // `wait_exit` resumes without polling.
    let _ = entry.exit.send(Some(code));
    entry.touch();
}

/// Drain a task's pipes into its ring buffer, calling `on_output` for each
/// stdout line and `on_exit` when the process exits.
///
/// stdout is read in raw chunks (like `drain_task`) so the ring buffer keeps
/// the exact byte stream — `poll()` accounting stays identical. Lines are
/// split out of the chunk stream separately: a partial line carries across
/// chunk boundaries, a trailing line without a final newline is emitted at
/// EOF, and a trailing `\r` from CRLF input is stripped from the emitted
/// line text only (the raw bytes in the ring buffer are untouched).
async fn drain_task_with_line_events(
    mut stdout: tokio::process::ChildStdout,
    mut stderr: tokio::process::ChildStderr,
    entry: Arc<TaskEntry>,
    id: pi::TaskId,
    on_output: LineEventCallback,
    on_exit: ExitEventCallback,
) {
    use tokio::io::AsyncReadExt;
    let mut out_chunk = [0u8; 8192];
    let mut err_chunk = [0u8; 8192];
    let mut out_done = false;
    let mut err_done = false;
    let mut carry: Vec<u8> = Vec::new();
    while !(out_done && err_done) {
        tokio::select! {
            n = stdout.read(&mut out_chunk), if !out_done => match n {
                Ok(0) | Err(_) => {
                    // EOF: emit a trailing partial line (no final newline).
                    if !carry.is_empty() {
                        let tail = String::from_utf8_lossy(&carry).into_owned();
                        carry.clear();
                        on_output(&id, tail);
                    }
                    out_done = true;
                }
                Ok(n) => {
                    let data = &out_chunk[..n];
                    entry.push(data);
                    emit_lines(&id, data, &mut carry, &*on_output);
                }
            },
            n = stderr.read(&mut err_chunk), if !err_done => match n {
                Ok(0) | Err(_) => err_done = true,
                Ok(n) => entry.push(&err_chunk[..n]),
            },
        }
    }
    let mut child = entry.child.lock().expect("child lock poisoned").take();
    let code = match child.as_mut() {
        Some(c) => c.wait().await.ok().map(|s| s.code()),
        None => None,
    }
    .flatten();
    *entry.exit_code.lock().expect("exit lock poisoned") = Some(code);
    let _ = entry.exit.send(Some(code));
    on_exit(&id, Some(code));
    entry.touch();
}

#[async_trait::async_trait]
impl BackgroundTaskRegistry for BackgroundRegistry {
    fn spawn(&self, command: &str, cwd: &Path) -> Result<pi::TaskId, pi::TaskError> {
        self.gc();
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .process_group(0)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| pi::TaskError::Spawn(format!("{e}")))?;
        let pid = child.id().map(|p| p as i32).unwrap_or(-1);
        let id = pi::TaskId(format!(
            "bg_{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ));
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let entry = Arc::new(TaskEntry::new(child, pid));
        self.tasks
            .lock()
            .expect("tasks lock poisoned")
            .insert(id.0.clone(), Arc::clone(&entry));
        tokio::spawn(drain_task(stdout, stderr, entry));
        Ok(id)
    }

    async fn poll(&self, id: &pi::TaskId) -> Result<pi::PollResult, pi::TaskError> {
        let entry = self
            .tasks
            .lock()
            .expect("tasks lock poisoned")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| pi::TaskError::NotFound(id.0.clone()))?;

        entry.touch();

        let buf = entry.buffer.lock().expect("buffer lock poisoned");

        let mut cursor = entry.read_cursor.lock().expect("cursor lock poisoned");
        // The buffer's first byte sits at logical offset `total_drained`, so
        // the read start is the cursor clamped into the retained window.
        let drained = entry.total_drained.load(Ordering::Relaxed);
        let start = cursor.saturating_sub(drained).min(buf.len() as u64) as usize;
        let new_output = String::from_utf8_lossy(&buf[start..]).to_string();
        *cursor = drained + buf.len() as u64;
        let is_running = entry
            .exit_code
            .lock()
            .expect("exit lock poisoned")
            .is_none();
        Ok(pi::PollResult {
            new_output,
            is_running,
            exit_code: *entry.exit_code.lock().expect("exit lock poisoned"),
            total_bytes: entry.total_bytes.load(Ordering::Relaxed),
        })
    }

    async fn kill(&self, id: &pi::TaskId) -> Result<(), pi::TaskError> {
        self.kill_sync(id)
    }
}

/// Split `data` into lines against `\n`, joining onto any partial line
/// carried over from a previous chunk. Each complete line is emitted without
/// its newline (and without a trailing `\r` from CRLF input); the remainder
/// after the last newline stays in `carry` for the next chunk.
fn emit_lines(
    id: &pi::TaskId,
    data: &[u8],
    carry: &mut Vec<u8>,
    on_output: &(dyn Fn(&pi::TaskId, String) + Send + Sync),
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
fn render_poll(poll: &pi::PollResult) -> String {
    let mut out = if poll.new_output.is_empty() {
        "(no new output)".to_string()
    } else {
        poll.new_output.clone()
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

/// `bash_output` tool — poll a background task for output since the last poll.
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
        "Fetch incremental output from a background command started with `run_in_background: true`. \
         Pass the returned shell id; each poll returns only the output produced since the last poll."
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
                }
            },
            "required": ["shell_id"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        _signal: CancellationToken,
        _ctx: &dyn pi::tool::ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let shell_id = params["shell_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("shell_id is required".into()))?;
        let poll = self
            .registry
            .poll(&pi::TaskId(shell_id.to_string()))
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;
        Ok(AgentToolResult::text(render_poll(&poll)))
    }
}

/// `task_stop` tool — terminate a background task's process group.
///
/// With a WebSocket monitor registry attached, ids unknown to the background
/// registry fall back to cancelling the WS monitor (`ws_N` ids), so a single
/// tool stops every background task kind.
pub struct TaskStopTool {
    registry: Arc<dyn BackgroundTaskRegistry>,
    ws_registry: Option<Arc<crate::monitor::WsMonitorRegistry>>,
}

impl TaskStopTool {
    pub fn new(registry: Arc<dyn BackgroundTaskRegistry>) -> Self {
        TaskStopTool {
            registry,
            ws_registry: None,
        }
    }

    /// Attach the WebSocket monitor registry as a stop fallback for `ws_N`
    /// ids.
    pub fn with_ws_registry(mut self, ws_registry: Arc<crate::monitor::WsMonitorRegistry>) -> Self {
        self.ws_registry = Some(ws_registry);
        self
    }
}

#[async_trait::async_trait]
impl AgentTool for TaskStopTool {
    fn name(&self) -> &str {
        "TaskStop"
    }

    fn description(&self) -> &str {
        "Stop a background task started with `run_in_background: true` or a monitor. Kills its process group."
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
        _ctx: &dyn pi::tool::ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let task_id = params["task_id"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("task_id is required".into()))?;
        match self.registry.kill(&pi::TaskId(task_id.to_string())).await {
            Ok(()) => Ok(AgentToolResult::text(format!(
                "Stopped background task `{task_id}`"
            ))),
            Err(pi::TaskError::NotFound(_)) if self.ws_registry.is_some() => {
                let ws = self.ws_registry.as_ref().expect("checked is_some");
                if ws.cancel_str(task_id) {
                    Ok(AgentToolResult::text(format!(
                        "Stopped WebSocket monitor `{task_id}`"
                    )))
                } else {
                    Err(ToolError::ExecutionFailed(format!(
                        "task not found: {task_id}"
                    )))
                }
            }
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
mod task_stop_ws_fallback_tests {
    use super::*;
    use crate::monitor::WsMonitorRegistry;
    use pi::tool::{AgentTool, LocalToolContext, ToolState};
    use std::sync::Arc;

    fn ctx() -> LocalToolContext {
        LocalToolContext::new(
            Arc::new(pi::env::TokioExecutionEnv::new(std::env::temp_dir())),
            std::env::temp_dir(),
            Arc::new(ToolState::new()),
        )
    }

    /// A `mon_N` command monitor lives in the shared BackgroundRegistry, so
    /// TaskStop reaches it without any fallback.
    #[tokio::test]
    async fn task_stop_kills_command_monitor_via_bg_registry() {
        let registry = Arc::new(BackgroundRegistry::new());
        let tool = TaskStopTool::new(Arc::clone(&registry) as Arc<dyn BackgroundTaskRegistry>);
        let id = registry
            .spawn_with_line_events(
                "sleep 30",
                Path::new("/tmp"),
                Box::new(|_id, _line| {}),
                Box::new(|_id, _code| {}),
            )
            .unwrap();
        assert!(id.0.starts_with("mon_"));
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
        assert!(stopped, "command monitor stopped through bg registry");
    }

    /// A `ws_N` id is unknown to the background registry; with a ws registry
    /// attached, TaskStop falls back to cancelling the WebSocket monitor.
    #[tokio::test]
    async fn task_stop_falls_back_to_ws_registry() {
        let registry = Arc::new(BackgroundRegistry::new());
        let ws_registry = Arc::new(WsMonitorRegistry::new());
        let cancel = CancellationToken::new();
        let ws_id = ws_registry.register("wss://example.com/ws".into(), cancel.clone());
        let tool = TaskStopTool::new(Arc::clone(&registry) as Arc<dyn BackgroundTaskRegistry>)
            .with_ws_registry(Arc::clone(&ws_registry));
        let result = tool
            .execute(
                "c1",
                serde_json::json!({"task_id": ws_id.0}),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(cancel.is_cancelled(), "ws monitor cancelled via fallback");
    }

    /// Without a ws registry, an unknown id surfaces a not-found error
    /// rather than silently succeeding.
    #[tokio::test]
    async fn task_stop_unknown_id_errors_without_ws_registry() {
        let registry = Arc::new(BackgroundRegistry::new());
        let tool = TaskStopTool::new(Arc::clone(&registry) as Arc<dyn BackgroundTaskRegistry>);
        let err = tool
            .execute(
                "c1",
                serde_json::json!({"task_id": "ws_404"}),
                CancellationToken::new(),
                &ctx(),
            )
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("not found"), "got: {err}");
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
                Box::new(|_id, _code| {}),
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
        let id = pi::TaskId("mon_test".into());
        let got: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let mut carry: Vec<u8> = Vec::new();
        let on_output = {
            let got = Arc::clone(&got);
            move |_id: &pi::TaskId, line: String| got.lock().unwrap().push(line)
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
