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
use tokio_util::sync::CancellationToken;

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
        let is_running = entry
            .exit_code
            .lock()
            .expect("exit lock poisoned")
            .is_none();
        let exit_code = *entry.exit_code.lock().expect("exit lock poisoned");
        Ok(TaskStatusInfo {
            is_running,
            exit_code,
            output_tail,
        })
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
        let entry = self
            .tasks
            .lock()
            .expect("tasks lock poisoned")
            .get(&id.0)
            .cloned()
            .ok_or_else(|| pi::TaskError::NotFound(id.0.clone()))?;
        entry.touch();
        // An exited task's group id may have been recycled by the OS; do not
        // signal it.
        if entry
            .exit_code
            .lock()
            .expect("exit lock poisoned")
            .is_some()
        {
            return Ok(());
        }
        // Signal the recorded process group; the child handle is not touched
        // so no mutex guard crosses an await.
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
        "bash_output"
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
        "task_stop"
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
        self.registry
            .kill(&pi::TaskId(task_id.to_string()))
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;
        Ok(AgentToolResult::text(format!(
            "Stopped background task `{task_id}`"
        )))
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
