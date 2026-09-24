//! The AHP host's runtime half.
//!
//! [`manox_ahp`] is the protocol: JSON-RPC, channels, the state store, the
//! action pipeline. It knows nothing about manox — its `Backend` trait is the
//! only window, and a test doubles it with a scripted backend. This crate is
//! that trait's real implementor, plus the shapes the protocol asks a runtime
//! for.
//!
//! The split matters in one direction. `manox-ahp` must not depend on this crate
//! (a transport that knows the runtime cannot be exercised against a fake one),
//! and `manox-agent` must not depend on either (the kernel does not know AHP
//! exists — capabilities reach it through its own hooks).
//!
//! [`runtime_trait::SessionRuntime`] is the seam the adapter holds. The gateway
//! in `manox-session-core` implements it for as long as v2 lives, which is what
//! lets the adapter name no gateway type.

pub mod ahp;
pub mod error;
pub mod journal_query;
pub mod paths;
pub mod runtime_trait;
pub mod translate;

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

    pub(crate) fn init_globals() {
        raise_fd_limit();
        INIT_ONCE.call_once(|| {
            manox_agent::runtime::init();
            manox_agent::provider_glue::init();
        });
    }
}
