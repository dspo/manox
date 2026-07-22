//! A single managed third-party process: its identity, process group, graceful
//! shutdown hook, and reaper coordination.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::Notify;

/// Grace budget for the client's graceful shutdown hook (e.g. LSP `shutdown` +
/// `exit`) before falling back to `SIGTERM`.
const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(3);

/// Grace budget for the process to exit on `SIGTERM` before escalating to
/// `SIGKILL`.
const TERM_GRACE: Duration = Duration::from_secs(2);

/// Final grace for the kernel to report the `SIGKILL`'d process gone.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// What kind of third-party process this is — only affects logging today; the
/// lifecycle is identical across kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessKind {
    Lsp,
    Mcp,
    Bash,
    Marketplace,
}

impl ProcessKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProcessKind::Lsp => "lsp",
            ProcessKind::Mcp => "mcp",
            ProcessKind::Bash => "bash",
            ProcessKind::Marketplace => "marketplace",
        }
    }
}

/// A graceful shutdown callback the client owns (e.g. the LSP client sends
/// `shutdown` + `exit` over its stdin). Returning before completion is fine —
/// `close` bounds it with `GRACEFUL_TIMEOUT` and falls back to signals.
pub type GracefulShutdown = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

pub(super) struct ReaperHandles {
    pub exited: Arc<Notify>,
    pub exited_flag: Arc<AtomicBool>,
    pub exit_code: Arc<Mutex<Option<Option<i32>>>>,
}

/// One spawned third-party process.
///
/// The `tokio::process::Child` is NOT held here — it moves into a detached
/// reaper task (`spawn` starts one) that calls `wait()` to reap the process
/// when it exits on its own, avoiding zombies. `close` coordinates with that
/// reaper via `exited` / `exited_flag`, and reaps the whole process group via
/// `kill(-pgid, sig)` so grandchildren die too.
pub struct ManagedProcess {
    name: String,
    kind: ProcessKind,
    pgid: Option<u32>,
    graceful: Mutex<Option<GracefulShutdown>>,
    exited: Arc<Notify>,
    exited_flag: Arc<AtomicBool>,
    exit_code: Arc<Mutex<Option<Option<i32>>>>,
    closed: AtomicBool,
}

impl ManagedProcess {
    pub(super) fn new(name: String, kind: ProcessKind, pgid: Option<u32>) -> Self {
        Self {
            name,
            kind,
            pgid,
            graceful: Mutex::new(None),
            exited: Arc::new(Notify::new()),
            exited_flag: Arc::new(AtomicBool::new(false)),
            exit_code: Arc::new(Mutex::new(None)),
            closed: AtomicBool::new(false),
        }
    }

    /// The handles the reaper task needs to signal completion. Cloned into the
    /// detached `child.wait()` task at spawn time.
    pub(super) fn reaper_handles(&self) -> ReaperHandles {
        ReaperHandles {
            exited: self.exited.clone(),
            exited_flag: self.exited_flag.clone(),
            exit_code: self.exit_code.clone(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> ProcessKind {
        self.kind
    }

    pub fn pgid(&self) -> Option<u32> {
        self.pgid
    }

    /// Whether the underlying process has already been reported as exited.
    pub fn is_exited(&self) -> bool {
        self.exited_flag.load(Ordering::SeqCst)
    }

    /// Wait until the detached reaper has observed the child exit and return
    /// its numeric exit code. `None` means signal/unknown status.
    pub async fn wait_for_exit(&self) -> Option<i32> {
        loop {
            let notified = self.exited.notified();
            if self.exited_flag.load(Ordering::SeqCst) {
                return self
                    .exit_code
                    .lock()
                    .expect("exit-code mutex poisoned")
                    .unwrap_or(None);
            }
            notified.await;
        }
    }

    /// Attach a graceful shutdown hook. The client (which owns the protocol
    /// stream) calls this so `close` can tear the server down politely before
    /// signaling its process group.
    pub fn set_graceful(&self, graceful: GracefulShutdown) {
        *self.graceful.lock().expect("graceful mutex poisoned") = Some(graceful);
    }

    /// Idempotent: runs the graceful hook (bounded), then `SIGTERM`s the whole
    /// process group, waits, and escalates to `SIGKILL` if still alive.
    pub async fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let kind = self.kind.as_str();
        let name = self.name.clone();

        // Graceful — the client's polite teardown (bounded). Failures here are
        // expected for unresponsive servers; the signal fallback follows.
        let graceful = self
            .graceful
            .lock()
            .expect("graceful mutex poisoned")
            .clone();
        if let Some(g) = graceful {
            tracing::info!(target: "supervisor", %kind, %name, "running graceful shutdown");
            let _ = tokio::time::timeout(GRACEFUL_TIMEOUT, g()).await;
        }

        if self.exited_flag.load(Ordering::SeqCst) {
            tracing::info!(target: "supervisor", %kind, %name, "already exited before signal");
            return;
        }

        // SIGTERM the whole group. kill(-pgid, sig) targets every process in
        // process group `pgid`; the child leads its own group (pgid == pid).
        self.signal_group(libc::SIGTERM);
        if !self.wait_for_process_group_exit(TERM_GRACE).await {
            tracing::warn!(target: "supervisor", %kind, %name, "did not exit on SIGTERM, escalating to SIGKILL");
            self.signal_group(libc::SIGKILL);
            let _ = self.wait_for_process_group_exit(KILL_GRACE).await;
        }
    }

    /// A shell can exit successfully after starting a detached descendant in
    /// its process group. The direct child has already been reaped in that
    /// case, so `close` intentionally will not touch its potentially stale
    /// pgid later. Background command drivers call this immediately after
    /// observing the direct exit, while the pgid is still owned by that task,
    /// to prevent the descendant from becoming an orphan.
    pub async fn cleanup_process_group_after_exit(&self) {
        if !self.process_group_exists() {
            return;
        }
        self.signal_group(libc::SIGTERM);
        if !self.wait_for_process_group_exit(TERM_GRACE).await {
            self.signal_group(libc::SIGKILL);
            let _ = self.wait_for_process_group_exit(KILL_GRACE).await;
        }
    }

    async fn wait_for_process_group_exit(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if !self.process_group_exists() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn process_group_exists(&self) -> bool {
        let Some(pgid) = self.pgid else {
            return !self.exited_flag.load(Ordering::SeqCst);
        };
        let rc = unsafe { libc::kill(-(pgid as libc::pid_t), 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    /// Signal every process in this child's process group. `ESRCH` (already gone)
    /// is logged at debug, not error.
    pub(crate) fn signal_group(&self, sig: i32) {
        let Some(pgid) = self.pgid else { return };
        // kill(-pgid, sig) signals process group `pgid`. Negation is the POSIX
        // convention: a negative pid targets the group whose pgid == |pid|.
        let rc = unsafe { libc::kill(-(pgid as libc::pid_t), sig) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            tracing::debug!(target: "supervisor", pgid, sig, "kill returned {rc}: {err}");
        }
    }
}
