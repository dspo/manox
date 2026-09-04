//! Context-free session orchestration core.
//!
//! Drives gpui-free `ThreadHandle`s through the `AgentServer` protocol gateway
//! for any host — the napi/vscode shell or the WebUI bridge. The core owns no
//! global state beyond the shared `agent` handles. `model_chat` is the
//! stateless bare-model completion channel shared with the VS Code
//! language-model provider; `translate` projects `ThreadEvent`s onto
//! `ServerNote`s.

pub mod agent_client;
pub mod agent_server;
pub mod model_chat;
pub mod projections;
pub mod translate;
pub mod waterfall;

/// Suite-wide test scaffolding: session-creating tests mutate `HOME` and
/// initialize `OnceLock` globals, so they must not interleave. Formerly the
/// tail of the retired `session` module (the actor-era command engine).
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, Once};

    /// Session-creating tests mutate `HOME` and initialize `OnceLock`
    /// globals, so they must not interleave with each other.
    pub(crate) static GLOBALS_LOCK: Mutex<()> = Mutex::new(());
    static HOME_ONCE: Once = Once::new();
    static INIT_ONCE: Once = Once::new();

    /// Take the suite serialization lock. A panic in one test poisons the
    /// mutex; recovering the guard keeps the failure contained instead of
    /// cascading into every later test in the process.
    pub(crate) fn lock_globals() -> std::sync::MutexGuard<'static, ()> {
        GLOBALS_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Point `HOME` at a throwaway directory so the thread db and provider
    /// config lookups stay out of the developer's real config. Never
    /// restored: the test process is disposable and provider registration
    /// reads `HOME` from a background thread.
    pub(crate) fn hermetic_home() {
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
    pub(crate) fn init_globals() {
        INIT_ONCE.call_once(|| {
            manox_agent::runtime::init();
            manox_agent::provider_glue::init();
        });
    }
}
