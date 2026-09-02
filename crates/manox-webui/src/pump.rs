//! The foreground pump that drives WebUI commands on the app main thread.
//!
//! Each WS connection is handed directly to the shared `AgentServer` over a
//! `WebSocketConnection`; no legacy translation layer, no shuttle task.
//! The `AgentServer` is created once at app startup and stored in a global
//! `OnceLock` so the WS server can `accept()` connections.

use std::sync::Arc;

use manox_session_core::agent_server::AgentServer;

/// Create the single `AgentServer` and store it in the crate global. Call
/// once at app startup, before any WS connection is accepted.
pub fn spawn_server() {
    let cwd = crate::bridge::resolve_cwd();
    let server = AgentServer::new(std::path::PathBuf::from(cwd));
    let _ = crate::AGENT_SERVER.set(Arc::new(server));
}

/// The shared AgentServer handle, if `spawn_server` has run.
pub fn webui_agent_server() -> Option<Arc<AgentServer>> {
    crate::AGENT_SERVER.get().cloned()
}
