//! Context-free session orchestration core.
//!
//! Drives gpui-free `ThreadHandle`s through the `AgentServer` protocol gateway
//! for any host (the gpui desktop in-process, the napi binding, or a WS
//! client). The core owns no global state beyond the shared `agent` handles.
//! `model_chat` is the stateless bare-model completion channel (its original
//! consumer, the VS Code language-model provider, was removed with the
//! frontends — round 3 §二.10④; the channel stays as the declared dormant
//! face); `translate` projects `ThreadEvent`s onto wire calls.

pub mod agent_client;
pub mod agent_server;
/// The gateway's implementation of the AHP runtime seam.
pub mod ahp_gateway;
/// Kernel `ThreadEvent` → v2 `ServerCall` adjudication translation (the v2 half
/// of what was one module; the journal projection moved to `manox-ahp-runtime`).
pub mod translate;

/// Integration tests for the AHP adapter against the live gateway.
#[cfg(test)]
mod ahp_adapter_tests;
pub mod follow;
pub mod model_chat;
pub mod projection_cache;
pub mod projection_hub;
pub mod projections;
pub mod waterfall;
pub mod workspace_serve;
#[cfg(feature = "ws-gateway")]
pub mod ws;

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, Once};

    /// Session-creating tests mutate `HOME` and initialize `OnceLock`
    /// globals, so they must not interleave with each other.
    pub(crate) static GLOBALS_LOCK: Mutex<()> = Mutex::new(());
    static HOME_ONCE: Once = Once::new();
    static INIT_ONCE: Once = Once::new();

    /// macOS ships RLIMIT_NOFILE at 256: a parallel suite of session tests
    /// (sqlite WAL triples per db, leases, journal appenders) collides with
    /// it as EMFILE flakes (review #805 gate). Raise the soft limit once
    /// per process from every common test entry point.
    fn raise_fd_limit() {
        static RAISED: Once = Once::new();
        RAISED.call_once(|| {
            // SAFETY: setrlimit on our own process at test setup; the new
            // soft limit stays under the hard limit conventionally granted
            // to interactive shells (fails silently otherwise).
            unsafe {
                let mut rl: libc::rlimit = std::mem::zeroed();
                if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 {
                    let want = 4096.min(rl.rlim_max);
                    if rl.rlim_cur < want {
                        rl.rlim_cur = want;
                        let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &rl);
                    }
                }
            }
        });
    }

    /// Take the suite serialization lock. A panic in one test poisons the
    /// mutex; recovering the guard keeps the failure contained instead of
    /// cascading into every later test in the process.
    pub(crate) fn lock_globals() -> std::sync::MutexGuard<'static, ()> {
        raise_fd_limit();
        GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Point `HOME` at a throwaway directory so the thread db and provider
    /// config lookups stay out of the developer's real config. Never
    /// restored: the test process is disposable and provider registration
    /// reads `HOME` from a background thread.
    pub(crate) fn hermetic_home() {
        raise_fd_limit();
        HOME_ONCE.call_once(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let home = std::env::temp_dir()
                .join(format!("manox-actor-test-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&home).unwrap();
            // SAFETY: test setup, serialized behind GLOBALS_LOCK.
            unsafe { std::env::set_var("HOME", home) };
        });
    }

    /// The tokio runtime and provider registry are process-wide `OnceLock`
    /// globals; initialize them exactly once, lightweight variants only
    /// (`manox_agent::init` would also boot MCP/LSP/plugin subsystems).
    /// Process-global serialization for tests that touch the thread store
    /// and the durable caches outside the agent_server suite's own guard.
    pub fn lock_globals_for_cache_test() -> std::sync::MutexGuard<'static, ()> {
        lock_globals()
    }

    pub(crate) fn init_globals() {
        raise_fd_limit();
        INIT_ONCE.call_once(|| {
            manox_agent::runtime::init();
            manox_agent::provider_glue::init();
        });
    }
}
