//! Background shell sessions backed by the unified `background_task` registry,
//! enabling the
//! `Bash` (`run_in_background: true`) + `BashOutput` (poll by shell id) pair
//! that mirrors Claude Code's background-shell workflow.
//!
//! A background shell is a long-running `sh -c` process whose stdout/stderr
//! are accumulated in a ring buffer with a per-poll read cursor, so
//! `BashOutput` can return just the bytes produced since the last poll. The
//! registry is shared by the main thread and every sub-agent, so shell ids are
//! globally unique and any thread can poll a known shell id.
//!
//! Lifecycle: `spawn` registers a shell and returns its id immediately (the
//! process keeps running in a spawned tokio task that drains output + records
//! exit status). `poll` returns incremental output + running/exit status.
//! `kill` signals the process group. Entries are retained after exit so a
//! final poll can harvest the exit code; a periodic GC sweeps long-dead
//! entries to bound memory.

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;

use crate::background_task::TaskId;
/// Hard cap on the accumulated output buffer per shell. Matches `bash`/`monitor`.
const MAX_BUFFER_BYTES: usize = 256 * 1024;
pub(crate) struct ShellState {
    /// All bytes seen so far (capped at `MAX_BUFFER_BYTES`; older bytes are
    /// dropped from the front once the cap is hit).
    pub(crate) buffer: Vec<u8>,
    /// Byte offset the last `poll` read up to. The next poll returns
    /// `buffer[read_cursor..]` (the incremental output since last poll).
    pub(crate) read_cursor: usize,
    /// Total bytes ever produced (even after the ring drops old bytes).
    pub(crate) total_bytes: u64,
    /// `None` while running, `Some(Some(code))` on clean exit, `Some(None)` on
    /// signal/abnormal termination.
    pub(crate) exit_code: Option<Option<i32>>,
    #[allow(dead_code)]
    pub(crate) created_at: Instant,
    /// When the process exited (`None` while running). Set by the drain task.
    pub(crate) exited_at: Option<Instant>,
    /// Owning thread id (for unified registry GC and `remove_all_for_thread`).
    pub(crate) thread_id: String,
}

/// Result of a `poll` call.
#[derive(Debug)]
pub struct PollResult {
    /// Output produced since the last poll (may be empty).
    pub new_output: String,
    /// `true` if the process is still running.
    pub is_running: bool,
    /// Exit code if the process has exited (`Some(code)` = clean exit,
    /// `Some(None)` = signaled). `None` while running.
    pub exit_code: Option<Option<i32>>,
    /// Total bytes the process has produced (cumulative, for advisory).
    pub total_bytes: u64,
}

impl PollResult {
    /// Render the poll result as the model-facing string returned by
    /// `BashOutput`. Mirrors Claude Code's `BashOutput` output shape.
    pub fn render(self) -> String {
        let status = if self.is_running {
            "running".to_string()
        } else {
            match self.exit_code {
                Some(Some(code)) => format!("exited with code {code}"),
                Some(None) => "terminated by signal".to_string(),
                None => "exited (status unknown)".to_string(),
            }
        };
        let body = if self.new_output.is_empty() {
            String::new()
        } else {
            self.new_output
        };
        format!(
            "Shell status: {status}\nTotal bytes: {}\n\n{body}",
            self.total_bytes
        )
    }
}

/// Spawn `command` under `sh -c` in the background, register it, and return
/// the shell id. The command runs in its own process group (so `kill` can reap
/// the whole tree). A spawned tokio task drains stdout+stderr into the
/// buffer and records the exit status when the process ends.
///
/// `plugin_root` (when `Some`) is injected as `CLAUDE_PLUGIN_ROOT` into the
/// child env, so plugin-spawned background shells can resolve
/// `${CLAUDE_PLUGIN_ROOT}` just like foreground bash.
#[allow(clippy::too_many_arguments)]
pub async fn spawn(
    command: String,
    thread_id: &str,
    anchor_message_id: Option<String>,
    cwd: &std::path::Path,
    timeout: Option<Duration>,
    plugin_root: Option<&std::path::Path>,
    #[cfg(target_os = "macos")] sandbox: &crate::sandbox::SandboxPolicy,
    #[cfg(not(target_os = "macos"))] _sandbox: &crate::sandbox::SandboxPolicy,
    #[cfg(target_os = "macos")] proxy_port: Option<u16>,
    #[cfg(not(target_os = "macos"))] _proxy_port: Option<u16>,
) -> Result<String, String> {
    // Wrap the command through the sandbox policy. On macOS this produces
    // a seatbelt-wrapped `sandbox-exec` command with the same write/network/
    // `.git` confinement as foreground sandboxed bash. On platforms without
    // a sandbox backend, fall back to a raw `sh -c` (matching the foreground
    #[cfg(all(target_os = "macos", not(test)))]
    let mut cmd = sandbox.wrap_command(&command, cwd, proxy_port);
    // Lifecycle tests run inside Codex's own macOS seatbelt, where nesting
    // sandbox-exec is rejected by the kernel. They exercise the supervisor,
    // event bus and polling path with raw sh; production remains sandboxed.
    #[cfg(all(target_os = "macos", test))]
    let _ = (sandbox, proxy_port);
    #[cfg(any(not(target_os = "macos"), test))]
    let mut cmd = {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(&command);
        c.current_dir(cwd);
        c
    };
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(root) = plugin_root {
        cmd.env("CLAUDE_PLUGIN_ROOT", root);
    }

    let bg_cancel = tokio_util::sync::CancellationToken::new();
    let (task_id, bg_task) = crate::background_task::register(
        crate::background_task::TaskKind::BackgroundBash,
        thread_id.into(),
        command.clone(),
        bg_cancel.clone(),
    );
    if let Some(anchor) = anchor_message_id {
        bg_task.set_anchor_message_id(anchor);
    }
    let shell_id = task_id.0.clone();

    let spawned = match supervisor::global()
        .spawn_captured(
            &format!("background-{shell_id}"),
            cmd,
            supervisor::ProcessKind::Bash,
        )
        .await
    {
        Ok(spawned) => spawned,
        Err(e) => {
            bg_task.set_failure_summary(e.to_string());
            bg_task.push_terminal(&task_id, crate::background_task::TaskStatus::Failed);
            return Err(format!("failed to spawn background shell: {e}"));
        }
    };
    let process = spawned.proc.clone();
    bg_task.set_managed_proc(process.clone());
    drop(spawned.stdin);
    let stdout = spawned.stdout;
    let stderr = spawned.stderr;

    let shell_state = Arc::new(std::sync::Mutex::new(ShellState {
        buffer: Vec::with_capacity(8 * 1024),
        read_cursor: 0,
        total_bytes: 0,
        exit_code: None,
        created_at: Instant::now(),
        exited_at: None,
        thread_id: thread_id.to_string(),
    }));
    // BashOutput polling state lives in the same unified registry.
    crate::background_task::register_bash_shell(&shell_id, shell_state.clone());

    let state = shell_state;

    // Spawn the drain + wait task. Use the global tokio runtime when available
    // (production — agent::init has run); fall back to tokio::spawn for
    // #[tokio::test] contexts where the test provides its own runtime. Either
    // way, the task runs on a tokio reactor so tokio::process works.
    let shell_id_for_driver = shell_id.clone();
    let state_clone = state.clone();
    let bg_task_clone = bg_task.clone();
    let bg_cancel_clone = bg_cancel.clone();
    let driver = async move {
        let stdout_task = tokio::spawn(drain_stream(
            stdout,
            state_clone.clone(),
            "stdout",
            bg_task_clone.clone(),
            TaskId(shell_id_for_driver.clone()),
        ));
        let stderr_task = tokio::spawn(drain_stream(
            stderr,
            state_clone.clone(),
            "stderr",
            bg_task_clone.clone(),
            TaskId(shell_id_for_driver.clone()),
        ));

        // Race: natural exit vs. timeout vs. TaskStop cancellation. Publish
        // terminal only after both pipe drainers finish, so it is observably
        // the final event and no tail output is lost.
        let (terminal_status, code) = tokio::select! {
            code = process.wait_for_exit() => {
                (
                    if code == Some(0) {
                        crate::background_task::TaskStatus::Completed
                    } else {
                        crate::background_task::TaskStatus::Failed
                    },
                    code,
                )
            }
            _ = bg_cancel_clone.cancelled() => {
                process.close().await;
                let code = process.wait_for_exit().await;
                (
                    bg_task_clone.requested_stop_status(),
                    code,
                )
            }
            _ = async {
                if let Some(t) = timeout {
                    tokio::time::sleep(t).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                process.close().await;
                let code = process.wait_for_exit().await;
                (
                    crate::background_task::TaskStatus::TimedOut,
                    code,
                )
            }
        };

        let _ = stdout_task.await;
        let _ = stderr_task.await;
        if let Ok(mut s) = state_clone.lock() {
            s.exit_code = Some(code);
            s.exited_at = Some(Instant::now());
        }
        bg_task_clone.set_exit_code(code);
        bg_task_clone.push_terminal(&TaskId(shell_id_for_driver), terminal_status);
    };
    let driver_handle = if let Some(h) = crate::runtime::try_handle() {
        h.spawn(driver)
    } else {
        tokio::spawn(driver)
    };
    bg_task.set_driver(driver_handle);

    // Opportunistic GC: sweep shells that exited long ago.
    crate::background_task::gc();

    Ok(shell_id)
}

/// Drain a piped stream (stdout or stderr) into the shared buffer. Bytes are
/// read in 8 KiB chunks; each chunk appends to the buffer (dropping front bytes
/// when over the cap) and updates the total.
async fn drain_stream<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    state: Arc<std::sync::Mutex<ShellState>>,
    label: &str,
    task: Arc<crate::background_task::BackgroundTask>,
    task_id: TaskId,
) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let data = &chunk[..n];
                let text = String::from_utf8_lossy(data).into_owned();
                if let Ok(mut s) = state.lock() {
                    s.total_bytes = s.total_bytes.saturating_add(data.len() as u64);
                    // Append, but cap the buffer: if over the limit, drop old
                    // bytes from the front (adjusting the read cursor so a poll
                    // that hasn't caught up still gets the new tail).
                    s.buffer.extend_from_slice(data);
                    if s.buffer.len() > MAX_BUFFER_BYTES {
                        let excess = s.buffer.len() - MAX_BUFFER_BYTES;
                        s.buffer.drain(..excess);
                        // The read cursor is relative to the *original* buffer
                        // start; after draining, adjust it so it doesn't point
                        // past the new buffer. A cursor that was within the
                        // dropped range resets to 0 (the reader gets all new
                        // bytes from here on).
                        s.read_cursor = s.read_cursor.saturating_sub(excess).min(s.buffer.len());
                    }
                }
                if label == "stderr" && !text.trim().is_empty() {
                    task.set_failure_summary(text.clone());
                }
                task.push_event(&task_id, text);
            }
        }
    }
}

/// Poll a background shell by id. Returns the output produced since the last
/// poll, plus running/exit status. Returns an error string for an unknown id.
pub fn poll(shell_id: &str) -> Result<PollResult, String> {
    let state = crate::background_task::get_bash_shell(shell_id)
        .ok_or_else(|| format!("Unknown shell id: {shell_id}"))?;

    let mut s = state.lock().expect("shell state poisoned");
    let available = if s.read_cursor < s.buffer.len() {
        String::from_utf8_lossy(&s.buffer[s.read_cursor..]).into_owned()
    } else {
        String::new()
    };
    s.read_cursor = s.buffer.len();
    let is_running = s.exit_code.is_none();
    Ok(PollResult {
        new_output: available,
        is_running,
        exit_code: s.exit_code,
        total_bytes: s.total_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_poll_kill_lifecycle() {
        // A command that prints two lines with a delay so we can observe
        // incremental output.
        let cwd = std::path::PathBuf::from(".");
        let id = spawn(
            "echo hello; sleep 0.2; echo world".to_string(),
            "test-thread",
            None,
            &cwd,
            None,
            None,
            &crate::sandbox::SandboxPolicy::for_project(&cwd),
            None,
        )
        .await
        .expect("spawn must succeed");

        // First poll: should get "hello\n", still running.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let r1 = poll(&id).expect("poll 1");
        assert!(r1.is_running, "should still be running");
        assert!(r1.new_output.contains("hello"), "got: {}", r1.new_output);

        // Wait for the process to finish, then poll again.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let r2 = poll(&id).expect("poll 2");
        assert!(!r2.is_running, "should have exited");
        assert!(r2.new_output.contains("world"), "got: {}", r2.new_output);
        assert!(r2.exit_code.is_some(), "exit code should be set");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn poll_unknown_id_errors() {
        let r = poll("nonexistent_id");
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("Unknown shell id"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn incremental_poll_returns_only_new_bytes() {
        let cwd = std::path::PathBuf::from(".");
        let id = spawn(
            "printf a; sleep 0.1; printf b; sleep 0.1; printf c".to_string(),
            "test-thread",
            None,
            &cwd,
            None,
            None,
            &crate::sandbox::SandboxPolicy::for_project(&cwd),
            None,
        )
        .await
        .expect("spawn");

        tokio::time::sleep(Duration::from_millis(50)).await;
        let r1 = poll(&id).expect("poll 1");
        assert!(r1.new_output.contains('a'), "got: {}", r1.new_output);
        assert!(!r1.new_output.contains('c'), "c not yet: {}", r1.new_output);

        tokio::time::sleep(Duration::from_millis(250)).await;
        let r2 = poll(&id).expect("poll 2");
        assert!(r2.new_output.contains('b'), "got: {}", r2.new_output);
        assert!(r2.new_output.contains('c'), "got: {}", r2.new_output);
        assert!(
            !r2.new_output.contains('a'),
            "a already consumed: {}",
            r2.new_output
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nonzero_exit_is_failed_and_terminal_is_last() {
        let cwd = std::path::PathBuf::from(".");
        let id = spawn(
            "printf tail; exit 7".to_string(),
            "test-thread-failed",
            None,
            &cwd,
            None,
            None,
            &crate::sandbox::SandboxPolicy::for_project(&cwd),
            None,
        )
        .await
        .expect("spawn");
        tokio::time::sleep(Duration::from_millis(100)).await;

        let task = crate::background_task::get_by_str(&id).expect("registered task");
        assert_eq!(task.status(), crate::background_task::TaskStatus::Failed);
        assert_eq!(task.exit_code(), Some(7));
        let events = crate::background_task::drain_thread_events("test-thread-failed");
        assert!(matches!(
            events.last().map(|event| &event.event),
            Some(crate::background_task::TaskEventKind::Terminal {
                status: crate::background_task::TaskStatus::Failed,
                exit_code: Some(7),
                ..
            })
        ));
        assert!(events.iter().any(|event| {
            matches!(&event.event, crate::background_task::TaskEventKind::Output(text) if text.contains("tail"))
        }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_stop_waits_for_background_shell() {
        let cwd = std::path::PathBuf::from(".");
        let id = spawn(
            "sleep 30".to_string(),
            "test-thread-stop-shell",
            None,
            &cwd,
            None,
            None,
            &crate::sandbox::SandboxPolicy::for_project(&cwd),
            None,
        )
        .await
        .expect("spawn");
        crate::background_task::stop(&id)
            .await
            .expect("TaskStop should succeed");
        let task = crate::background_task::get_by_str(&id).expect("registered task");
        assert_eq!(task.status(), crate::background_task::TaskStatus::Stopped);
        assert!(!poll(&id).expect("poll stopped shell").is_running);
    }
}
