//! Process-global registry of background shell sessions, enabling the
//! `Bash` (`run_in_background: true`) + `BashOutput` (poll by shell id) pair
//! that mirrors Claude Code's background-shell workflow.
//!
//! A background shell is a long-running `sh -c` process whose stdout/stderr
//! are accumulated in a ring buffer with a per-poll read cursor, so
//! `BashOutput` can return just the bytes produced since the last poll. The
//! registry is a process-wide singleton (`OnceLock`) shared by the main thread
//! and every sub-agent — shell ids are globally unique, matching Claude Code's
//! behavior where any thread can poll any shell id.
//!
//! Lifecycle: `spawn` registers a shell and returns its id immediately (the
//! process keeps running in a spawned tokio task that drains output + records
//! exit status). `poll` returns incremental output + running/exit status.
//! `kill` signals the process group. Entries are retained after exit so a
//! final poll can harvest the exit code; a periodic GC sweeps long-dead
//! entries to bound memory.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::Child;

/// Hard cap on the accumulated output buffer per shell. Matches `bash`/`monitor`.
const MAX_BUFFER_BYTES: usize = 256 * 1024;
/// Shells whose process exited more than this long ago are eligible for GC.
const GC_AFTER_EXIT: Duration = Duration::from_secs(300);

/// A registered background shell: the child process handle, accumulated
/// output, and lifecycle state.
struct BackgroundShell {
    /// Wrapped in a mutex so the drain task (writer) and `poll` (reader) can
    /// share access. The buffer is a byte ring with a read cursor.
    state: Arc<std::sync::Mutex<ShellState>>,
}

struct ShellState {
    /// All bytes seen so far (capped at `MAX_BUFFER_BYTES`; older bytes are
    /// dropped from the front once the cap is hit).
    buffer: Vec<u8>,
    /// Byte offset the last `poll` read up to. The next poll returns
    /// `buffer[read_cursor..]` (the incremental output since last poll).
    read_cursor: usize,
    /// Total bytes ever produced (even after the ring drops old bytes).
    total_bytes: u64,
    /// `None` while running, `Some(Some(code))` on clean exit, `Some(None)` on
    /// signal/abnormal termination.
    exit_code: Option<Option<i32>>,
    /// When the shell was spawned (for GC bookkeeping).
    #[allow(dead_code)] // retained for future lifecycle diagnostics
    created_at: Instant,
    /// When the process exited (`None` while running). Set by the drain task.
    exited_at: Option<Instant>,
    /// The child PID (== process group id, since we use `process_group(0)`).
    /// Stored so `kill()` can signal the group even after the `Child` handle
    /// is moved into the drain task.
    pid: Option<u32>,
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
        format!("Shell status: {status}\nTotal bytes: {}\n\n{body}", self.total_bytes)
    }
}

/// The process-global background shell registry.
static REGISTRY: OnceLock<std::sync::Mutex<BackgroundShellRegistry>> = OnceLock::new();

struct BackgroundShellRegistry {
    shells: HashMap<String, BackgroundShell>,
    next_id: u64,
}

/// Access the process-global registry. Lazily initialized on first call.
fn registry() -> &'static std::sync::Mutex<BackgroundShellRegistry> {
    REGISTRY.get_or_init(|| std::sync::Mutex::new(BackgroundShellRegistry { shells: HashMap::new(), next_id: 1 }))
}

/// Spawn `command` under `sh -c` in the background, register it, and return
/// the shell id. The command runs in its own process group (so `kill` can reap
/// the whole tree). A spawned tokio task drains stdout+stderr into the
/// buffer and records the exit status when the process ends.
///
/// `plugin_root` (when `Some`) is injected as `CLAUDE_PLUGIN_ROOT` into the
/// child env, so plugin-spawned background shells can resolve
/// `${CLAUDE_PLUGIN_ROOT}` just like foreground bash.
pub fn spawn(
    command: String,
    cwd: &std::path::Path,
    timeout: Option<Duration>,
    plugin_root: Option<&std::path::Path>,
    sandbox: &crate::sandbox::SandboxPolicy,
) -> Result<String, String> {
    // Wrap the command through the sandbox policy (seatbelt on macOS) so
    // background shells get the same write/network/.git confinement as
    // foreground sandboxed bash.
    let mut cmd = sandbox.wrap_command(&command, cwd);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.kill_on_drop(false);
    // Own process group so kill reaches grandchildren.
    #[cfg(unix)]
    cmd.process_group(0);
    if let Some(root) = plugin_root {
        cmd.env("CLAUDE_PLUGIN_ROOT", root);
    }

    let mut child: Child = cmd.spawn().map_err(|e| format!("failed to spawn background shell: {e}"))?;
    let pid = child.id();

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let shell_id = {
        let mut reg = registry().lock().expect("background shell registry poisoned");
        let id = format!("bash_{}", reg.next_id);
        reg.next_id += 1;
        reg.shells.insert(
            id.clone(),
            BackgroundShell {
                state: Arc::new(std::sync::Mutex::new(ShellState {
                    buffer: Vec::with_capacity(8 * 1024),
                    read_cursor: 0,
                    total_bytes: 0,
                    exit_code: None,
                    created_at: Instant::now(),
                    exited_at: None,
                    pid,
                })),
            },
        );
        id
    };

    let state = {
        let reg = registry().lock().expect("background shell registry poisoned");
        reg.shells.get(&shell_id).expect("just inserted").state.clone()
    };

    // Spawn the drain + wait task. Use the global tokio runtime when available
    // (production — agent::init has run); fall back to tokio::spawn for
    // #[tokio::test] contexts where the test provides its own runtime. Either
    // way, the task runs on a tokio reactor so tokio::process works.
    let state_clone = state.clone();
    let driver = async move {
        let stdout_task = tokio::spawn(drain_stream(stdout, state_clone.clone(), "stdout"));
        let stderr_task = tokio::spawn(drain_stream(stderr, state_clone.clone(), "stderr"));

        let wait = async {
            let status = child.wait().await;
            if let Ok(mut s) = state_clone.lock() {
                s.exit_code = Some(status.ok().and_then(|st| st.code()));
                s.exited_at = Some(Instant::now());
            }
        };
        if let Some(t) = timeout {
            tokio::select! {
                _ = wait => {}
                _ = tokio::time::sleep(t) => {
                    kill_process_group(&child);
                    if let Ok(mut s) = state_clone.lock() {
                        s.exit_code = Some(None);
                        s.exited_at = Some(Instant::now());
                    }
                }
            }
        } else {
            wait.await;
        }

        let _ = stdout_task.await;
        let _ = stderr_task.await;
    };
    if let Some(h) = crate::runtime::try_handle() {
        h.spawn(driver);
    } else {
        tokio::spawn(driver);
    }

    // Opportunistic GC: sweep shells that exited long ago.
    gc();

    Ok(shell_id)
}

/// Drain a piped stream (stdout or stderr) into the shared buffer. Bytes are
/// read in 8 KiB chunks; each chunk appends to the buffer (dropping front bytes
/// when over the cap) and updates the total.
async fn drain_stream<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    state: Arc<std::sync::Mutex<ShellState>>,
    _label: &str,
) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let data = &chunk[..n];
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
            }
        }
    }
}

/// Poll a background shell by id. Returns the output produced since the last
/// poll, plus running/exit status. Returns an error string for an unknown id.
pub fn poll(shell_id: &str) -> Result<PollResult, String> {
    let reg = registry().lock().expect("background shell registry poisoned");
    let Some(shell) = reg.shells.get(shell_id) else {
        return Err(format!("Unknown shell id: {shell_id}"));
    };
    let state = shell.state.clone();
    drop(reg);

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

/// Kill a background shell's process group. No-op if the shell already exited.
pub fn kill(shell_id: &str) -> Result<(), String> {
    let reg = registry().lock().expect("background shell registry poisoned");
    let Some(shell) = reg.shells.get(shell_id) else {
        return Err(format!("Unknown shell id: {shell_id}"));
    };
    let state = shell.state.clone();
    drop(reg);

    let s = state.lock().expect("shell state poisoned");
    if s.exit_code.is_some() {
        return Ok(());
    }
    // Kill the process group using the stored pid (== pgid).
    if let Some(pid) = s.pid {
        #[cfg(unix)]
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
        return Ok(());
    }
    Err(format!("background shell {shell_id} has no stored pid"))
}

/// Run a garbage-collection pass: remove shells whose process exited more than
/// `GC_AFTER_EXIT` ago. Called opportunistically by `spawn`/`poll` to bound
/// memory without a dedicated timer.
fn gc() {
    let mut reg = registry().lock().expect("background shell registry poisoned");
    let now = Instant::now();
    reg.shells.retain(|_, shell| {
        let s = shell.state.lock().expect("shell state poisoned");
        match s.exited_at {
            Some(t) => now.duration_since(t) < GC_AFTER_EXIT,
            None => true,
        }
    });
}


/// Kill the child's whole process group. On Unix the child runs in its own
/// group (set via `process_group(0)`), so `killpg` reaps grandchildren too.
#[cfg(unix)]
fn kill_process_group(child: &tokio::process::Child) {
    if let Some(pid) = child.id() {
        // SAFETY: `killpg` is a libc call with no Rust invariants to uphold;
        // the pid is the child's own group id (set by `process_group(0)`).
        unsafe {
            libc::killpg(pid as i32, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_process_group(_child: &tokio::process::Child) {}

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
            &cwd,
            None,
            None,
            &crate::sandbox::SandboxPolicy::for_project(&cwd),
        )
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
            &cwd,
            None,
            None,
            &crate::sandbox::SandboxPolicy::for_project(&cwd),
        )
        .expect("spawn");

        tokio::time::sleep(Duration::from_millis(50)).await;
        let r1 = poll(&id).expect("poll 1");
        assert!(r1.new_output.contains('a'), "got: {}", r1.new_output);
        assert!(!r1.new_output.contains('c'), "c not yet: {}", r1.new_output);

        tokio::time::sleep(Duration::from_millis(250)).await;
        let r2 = poll(&id).expect("poll 2");
        assert!(r2.new_output.contains('b'), "got: {}", r2.new_output);
        assert!(r2.new_output.contains('c'), "got: {}", r2.new_output);
        assert!(!r2.new_output.contains('a'), "a already consumed: {}", r2.new_output);
    }
}
