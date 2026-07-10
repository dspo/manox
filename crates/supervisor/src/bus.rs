//! The process bus: spawns children into their own process groups, tracks
//! them, reaps them on exit, and tears them all down on `shutdown_all`.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::proc::{ManagedProcess, ProcessKind};

/// A lifecycle condition a client can hook a process to. Only `Shutdown` is
/// acted on today — `spawn` auto-registers every process for it. `Idle` (auto
/// close after inactivity) is tracked in issue #128.
#[derive(Debug)]
pub enum Condition {
    /// Close when `shutdown_all` runs (manox exit). `spawn` registers this
    /// implicitly, so an explicit `hook(Shutdown, ..)` is redundant — kept for
    /// API symmetry.
    Shutdown,
    /// Close after the process has been idle for the given duration. Not yet
    /// implemented (issue #128); `hook(Idle, ..)` logs a warning.
    Idle(Duration),
}

/// The stdout/stdin streams torn off the spawned child. The protocol layer
/// (rmcp transport or the LSP JSON-RPC framer) consumes these; the `Child`
/// itself stays with the bus's reaper task.
pub struct SpawnedProcess {
    pub proc: Arc<ManagedProcess>,
    pub stdout: ChildStdout,
    pub stdin: ChildStdin,
}

/// Process-wide registry of spawned third-party processes.
#[derive(Default)]
pub struct ProcessBus {
    procs: Mutex<Vec<Arc<ManagedProcess>>>,
}

impl ProcessBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn `cmd` in its own process group, capture stderr to `tracing`, start
    /// a detached reaper that calls `wait()` (so the child never zombies), and
    /// register it for `shutdown_all`. Returns the child's stdio streams for the
    /// protocol layer to use.
    ///
    /// The caller must be on the tokio runtime (this `tokio::spawn`s the reaper
    /// and stderr reader). Callers off-runtime (e.g. the gpui main thread)
    /// drive this through `agent::runtime::handle()`'s `block_on`/`spawn`.
    pub async fn spawn(
        &self,
        name: &str,
        mut cmd: Command,
        kind: ProcessKind,
    ) -> anyhow::Result<SpawnedProcess> {
        // Own process group: the child becomes leader of a new group whose pgid
        // equals its pid, so kill(-pid, sig) reaps the whole subtree.
        cmd.process_group(0);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Belt-and-suspenders: if the reaper task is ever dropped before wait()
        // returns (e.g. runtime teardown), tokio reaps the direct child itself.
        // shutdown_all still does killpg so grandchildren die too.
        cmd.kill_on_drop(true);

        let mut child: Child = cmd
            .spawn()
            .with_context(|| format!("spawning {kind:?} process `{name}`"))?;
        let pid = child.id();
        tracing::info!(target: "supervisor", %name, ?kind, pid, "process spawned");

        let stdout = child.stdout.take().expect("piped stdout");
        let stdin = child.stdin.take().expect("piped stdin");
        let stderr = child.stderr.take().expect("piped stderr");

        let proc = Arc::new(ManagedProcess::new(name.to_string(), kind, pid));
        let (exited, exited_flag) = proc.reaper_handles();

        let reaper_name = name.to_string();
        tokio::spawn(async move {
            let status = child.wait().await;
            exited_flag.store(true, std::sync::atomic::Ordering::SeqCst);
            match status {
                Ok(s) => tracing::info!(target: "supervisor", %reaper_name, ?s, "process exited"),
                Err(e) => tracing::warn!(target: "supervisor", %reaper_name, "wait failed: {e}"),
            }
            exited.notify_waiters();
        });

        let stderr_name = name.to_string();
        tokio::spawn(read_stderr(stderr_name, stderr));

        self.procs
            .lock()
            .expect("procs mutex poisoned")
            .push(proc.clone());
        Ok(SpawnedProcess {
            proc,
            stdout,
            stdin,
        })
    }

    /// Register a process against a condition. `Shutdown` is already implicit in
    /// `spawn`; `Idle` is not yet implemented (issue #128).
    pub fn hook(&self, cond: Condition, p: Arc<ManagedProcess>) {
        match cond {
            Condition::Shutdown => {
                let mut procs = self.procs.lock().expect("procs mutex poisoned");
                if !procs.iter().any(|q| Arc::ptr_eq(q, &p)) {
                    procs.push(p);
                }
            }
            Condition::Idle(_) => {
                tracing::warn!(
                    target: "supervisor",
                    name = %p.name(),
                    "Idle condition not yet implemented (see issue #128)"
                );
            }
        }
    }

    /// Close every spawned process (graceful → SIGTERM → SIGKILL, per group).
    /// Called from manox's exit path.
    pub async fn shutdown_all(&self) {
        let procs = self.procs.lock().expect("procs mutex poisoned").clone();
        if procs.is_empty() {
            return;
        }
        tracing::info!(target: "supervisor", count = procs.len(), "shutdown_all");
        // Concurrent close: independent groups, no cross-process ordering.
        let _ = futures::future::join_all(procs.into_iter().map(|p| async move {
            p.close().await;
        }))
        .await;
    }

    /// Synchronous best-effort reap for the process-exit path (after gpui tears
    /// down, before the OS reaps the process). Sends `SIGTERM` to every spawned
    /// process group via raw `killpg` — no async wait, because the process is
    /// about to exit and there is no budget for graceful-then-escalate.
    ///
    /// `SIGTERM` is what well-behaved LSP/MCP servers exit on (rust-analyzer,
    /// gopls, pyright, typescript-language-server all install handlers); any
    /// that finish after manox is gone get reaped by init reparenting. manox
    /// only signals processes it spawned itself — a server the user ran in
    /// another terminal is not in the registry and is untouched. Swap to
    /// `SIGKILL` here if a future server proves stubborn on `SIGTERM`.
    pub fn terminate_all(&self) {
        let procs = self.procs.lock().expect("procs mutex poisoned").clone();
        if procs.is_empty() {
            return;
        }
        tracing::info!(target: "supervisor", count = procs.len(), "terminate_all (SIGTERM)");
        for p in &procs {
            p.signal_group(libc::SIGTERM);
        }
    }
}

/// Read stderr line-by-line and route each line to `tracing`. Keeps server
/// diagnostics visible without surfacing them on the protocol stream.
async fn read_stderr(name: String, stderr: tokio::process::ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if line.trim().is_empty() {
                    continue;
                }
                tracing::info!(target: "supervisor", %name, "stderr: {line}");
            }
            Ok(None) => break,
            Err(e) => {
                tracing::debug!(target: "supervisor", %name, "stderr reader ended: {e}");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Spawn a process that sleeps long enough to be alive at shutdown_all, then
    // confirm it and its group are gone afterward.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_all_reaps_sleep() {
        let bus = ProcessBus::new();
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let spawned = bus
            .spawn("sleep-test", cmd, ProcessKind::Lsp)
            .await
            .expect("spawn sleep");
        let pid = spawned.proc.pgid().expect("pgid");
        let bus_arc = std::sync::Arc::new(bus);
        let bus_for_close = bus_arc.clone();
        tokio::spawn(async move { bus_for_close.shutdown_all().await });
        // SIGTERM on a bare `sleep` kills it quickly; escalate closes it within
        // TERM_GRACE + KILL_GRACE otherwise.
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        while std::time::Instant::now() < deadline {
            if spawned.proc.is_exited() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(spawned.proc.is_exited(), "process should be reaped");
        // The pid must no longer be a live process.
        let still_alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        assert!(!still_alive, "pid {pid} still alive after shutdown_all");
    }

    // A script that forks a child, then sleeps. killpg must reap the grandchild
    // too — confirming the process-group reaping, not just the direct child.
    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_all_reaps_forked_subtree() {
        let bus = ProcessBus::new();
        // sh -c 'sleep 30 & wait' — the `sleep` is a grandchild of manox.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & wait");
        let spawned = bus.spawn("fork-test", cmd, ProcessKind::Lsp).await.unwrap();
        let pgid = spawned.proc.pgid().unwrap();
        // Let the grandchild `sleep` actually start.
        tokio::time::sleep(Duration::from_millis(300)).await;
        spawned.proc.close().await;
        // After close, no process in group `pgid` should remain.
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        loop {
            // kill(-pgid, 0): 0 means "test existence"; ESRCH => group empty.
            let rc = unsafe { libc::kill(-(pgid as libc::pid_t), 0) };
            if rc != 0 {
                break;
            }
            if std::time::Instant::now() >= deadline {
                panic!("process group {pgid} still alive after close");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}
