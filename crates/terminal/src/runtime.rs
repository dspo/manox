//! Process-global tokio runtime handle for the terminal's async pumps.
//!
//! Mirrors `manox_agent::runtime` in shape but stays crate-local: the terminal's
//! PTY/event/readiness pumps run on whatever runtime the host registers, so
//! the crate never reaches into another crate's globals. The host calls
//! [`set_runtime`] once at startup, before constructing any `Terminal`.

use std::sync::OnceLock;

static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// Register the runtime the terminal pumps spawn on. Call at App startup.
/// First registration wins.
pub fn set_runtime(handle: tokio::runtime::Handle) {
    if HANDLE.set(handle).is_err() {
        tracing::warn!("terminal runtime already registered; ignoring re-registration");
    }
}

/// The registered runtime handle. Panics if [`set_runtime`] was not called.
pub fn handle() -> &'static tokio::runtime::Handle {
    HANDLE
        .get()
        .expect("terminal runtime not initialized; call terminal::runtime::set_runtime first")
}

/// The registered runtime handle, or `None` before registration.
pub fn try_handle() -> Option<&'static tokio::runtime::Handle> {
    HANDLE.get()
}
