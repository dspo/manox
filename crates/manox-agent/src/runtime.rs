//! Process-global tokio runtime handle.
//!
//! `init()` builds a multi-threaded tokio runtime at App startup; `handle()`
//! returns the global `Handle`. `LanguageModel::stream_completion` spawns tokio
//! tasks to run reqwest streaming HTTP, forwarding events back to the gpui side
//! via `async_channel` (executor-agnostic, pollable on the gpui executor).
//!
//! `init()` also acquires a process-exclusive flock on `~/.manox/runtime.lock`
//! to prevent multiple manox instances from concurrently accessing the same
//! store.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::sync::{Once, OnceLock};

use tokio::runtime::Runtime;

static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();
static INIT: Once = Once::new();

/// Path to the runtime lock file under `~/.manox/`.
fn lock_path() -> std::path::PathBuf {
    crate::paths::manox_config_dir()
        .unwrap_or_else(|_| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
        .join("runtime.lock")
}

/// Try to acquire an exclusive process-level lock on `~/.manox/runtime.lock`.
/// Exits the process with an error message if another instance already holds it.
fn acquire_runtime_lock() {
    let path = lock_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap_or_else(|e| {
            eprintln!("无法创建 runtime.lock ({}): {e:#}", path.display());
            std::process::exit(1);
        });
    // SAFETY: `flock` is a standard POSIX operation; the fd is valid and owned
    // by this process. We leak the File so the lock is held for the lifetime.
    let fd = file.as_raw_fd();
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        let is_contended = err
            .raw_os_error()
            .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN);
        if is_contended {
            eprintln!(
                "错误: 另一个 manox 实例已持有此 store (lock: {})",
                path.display()
            );
            std::process::exit(1);
        }
        eprintln!("警告: runtime.lock 加锁失败 ({}): {}", err, path.display());
    }
    // Leak the File so the fd stays open and the lock is held for the process
    // lifetime.
    std::mem::forget(file);
}

/// Build a 2-worker multi-threaded tokio runtime and register its global `Handle`. Call at App startup.
///
/// Idempotent: the runtime, the lock, and the handle are process-lifetime
/// resources, so a second `init()` (e.g. a later test in the same process)
/// is a no-op instead of re-acquiring the flock — re-acquiring a lock the
/// process already holds opens a second file description that `flock` denies,
/// which previously exited the whole test binary.
pub fn init() {
    INIT.call_once(|| {
        acquire_runtime_lock();
        let runtime = Runtime::new().expect("failed to build tokio runtime");
        let _ = HANDLE.set(runtime.handle().clone());
        // The runtime is intentionally forgotten: it lives for the process lifetime, with worker threads driving IO.
        std::mem::forget(runtime);
    });
}

/// Throwaway HOME for test processes (review round 3, P0-2): the
/// single-instance flock lives at `$HOME/.manox/runtime.lock`, so a
/// developer's live app turns every test binary that calls [`init`] into a
/// silent mid-suite `exit(1)` — no failures list, no panic location, and
/// every later test in the binary never runs. Redirecting HOME once per
/// process (before the first `init`) sends the flock, the provider-config
/// lookup, and the session paths to a temp dir. Never restored: the test
/// process is disposable.
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
