//! Process-global tokio runtime handle.
//!
//! `init(cx)` builds a multi-threaded tokio runtime at App startup; `handle()`
//! returns the global `Handle`. `LanguageModel::stream_completion` spawns tokio
//! tasks on the gpui executor to run reqwest streaming HTTP, forwarding events
//! back to the gpui side via `async_channel` (executor-agnostic, pollable on the
//! gpui executor).

use std::sync::OnceLock;

use gpui::App;
use tokio::runtime::Runtime;

static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Build a 2-worker multi-threaded tokio runtime and register its global `Handle`. Call at App startup.
pub fn init(_cx: &mut App) {
    let runtime = Runtime::new().expect("failed to build tokio runtime");
    let _ = HANDLE.set(runtime.handle().clone());
    // The runtime is intentionally forgotten: it lives for the process lifetime, with worker threads driving IO.
    std::mem::forget(runtime);
}

/// Returns the global tokio `Handle`. Panics if `init` was not called.
pub fn handle() -> &'static tokio::runtime::Handle {
    HANDLE
        .get()
        .expect("tokio runtime not initialized; call agent::init first")
}

/// Returns the global tokio `Handle`, or `None` before `init` / after process
/// teardown. Safe to call from `Drop` implementations where panicking would
/// abort — the worktree auto-cleanup path uses this to fire-and-forget a git
/// `worktree remove` without risking a panic if the runtime is gone.
pub fn try_handle() -> Option<&'static tokio::runtime::Handle> {
    HANDLE.get()
}
