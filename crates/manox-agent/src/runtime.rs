//! Process-global tokio runtime handle.
//!
//! `init()` builds a multi-threaded tokio runtime at App startup; `handle()`
//! returns the global `Handle`. `LanguageModel::stream_completion` spawns tokio
//! tasks to run reqwest streaming HTTP, forwarding events back to the gpui side
//! via `async_channel` (executor-agnostic, pollable on the gpui executor).
//!
//! Multiple manox processes may share one state root: cross-process
//! coordination lives at resource granularity (per-session write leases in
//! [`crate::session_lease`], SQLite WAL for threads.db, per-file locks for
//! the shared state files, the gateway lock in session-core), never at
//! process startup.

use std::sync::{Once, OnceLock};

use tokio::runtime::Runtime;

static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();
static INIT: Once = Once::new();

/// Build a 2-worker multi-threaded tokio runtime and register its global `Handle`. Call at App startup.
///
/// Idempotent: the runtime and the handle are process-lifetime resources,
/// so a second `init()` (e.g. a later test in the same process) is a no-op.
pub fn init() {
    INIT.call_once(|| {
        let runtime = Runtime::new().expect("failed to build tokio runtime");
        let _ = HANDLE.set(runtime.handle().clone());
        // The runtime is intentionally forgotten: it lives for the process lifetime, with worker threads driving IO.
        std::mem::forget(runtime);
    });
}

/// Throwaway HOME for test processes: the provider-config lookup and the
/// session/threads.db paths all resolve under `$HOME`, and a test process
/// must never read or write the developer's real store. Redirecting HOME
/// once per process (before the first `init`) sends the provider-config
/// lookup and the session paths to a temp dir. Never restored: the test
/// process is disposable.
#[cfg(any(test, feature = "test-support"))]
static TEST_HOME: OnceLock<std::path::PathBuf> = OnceLock::new();

#[cfg(any(test, feature = "test-support"))]
pub fn hermetic_home_for_test() {
    TEST_HOME.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let home = std::env::temp_dir().join(format!(
            "manox-hermetic-home-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).expect("create hermetic test home");
        // SAFETY: test setup only; the OnceLock runs this exactly once and
        // callers serialize behind their crate's test globals lock before
        // the first runtime::init.
        unsafe { std::env::set_var("HOME", &home) };
        home
    });
}

/// [`init`] with the hermetic HOME redirect applied first — the test-side
/// entry point (see [`hermetic_home_for_test`]).
#[cfg(any(test, feature = "test-support"))]
pub fn init_hermetic_for_test() {
    hermetic_home_for_test();
    init();
}

/// Returns the global tokio `Handle`. Panics if `init` was not called.
pub fn handle() -> &'static tokio::runtime::Handle {
    HANDLE
        .get()
        .expect("tokio runtime not initialized; call manox_agent::init first")
}

/// Returns the global tokio `Handle`, or `None` before `init` / after process
/// teardown. Safe to call from `Drop` implementations where panicking would
/// abort — the worktree auto-cleanup path uses this to fire-and-forget a git
/// `worktree remove` without risking a panic if the runtime is gone.
pub fn try_handle() -> Option<&'static tokio::runtime::Handle> {
    HANDLE.get()
}
