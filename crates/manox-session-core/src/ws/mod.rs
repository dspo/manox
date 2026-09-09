//! The gateway's WebSocket transport arm: a loopback HTTP listener whose
//! `/ws` route upgrades token-authenticated clients into
//! [`WebSocketConnection`] bridges handed straight to the shared
//! [`AgentServer`](crate::agent_server::AgentServer). Every client speaks the
//! typed `FromClient`/`FromServer` protocol directly — no translation layer.
//!
//! History: this listener was the `manox-webui` crate's network face, serving
//! the embedded browser frontend; the frontend-removal decision deleted the
//! product face and kept the transport (the "keep the implemented http/ws
//! interfaces" ruling), moved here next to the gateway it feeds. The desktop
//! binary does not start it; `cx web` runs it headless, and out-of-process
//! clients discover the endpoint through `<config>/gateway-ws.json` (0600).

mod connection;
mod listener;

pub use connection::WebSocketConnection;

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

/// The bound listener's endpoint, published once at bind time.
#[derive(Clone, Debug)]
pub struct GatewayEndpoint {
    /// `http://127.0.0.1:<port>/` — the listener's base URL.
    pub url: String,
    /// The bound loopback port.
    pub port: u16,
    /// The per-boot token every WS client must present (`?token=`).
    pub token: String,
}

static ENDPOINT: OnceLock<Mutex<Option<GatewayEndpoint>>> = OnceLock::new();

fn endpoint_slot() -> &'static Mutex<Option<GatewayEndpoint>> {
    ENDPOINT.get_or_init(|| Mutex::new(None))
}

/// The bound endpoint, if the listener is up.
pub fn service_endpoint() -> Option<GatewayEndpoint> {
    endpoint_slot().lock().unwrap().clone()
}

/// The default project directory for the adopted
/// [`AgentServer`](crate::agent_server::AgentServer): the most recently
/// registered project the thread store knows, falling back to `$HOME` (the
/// old webui bridge's rule).
pub fn default_cwd() -> PathBuf {
    if let Some(store) = manox_agent::thread_store::try_global() {
        let known = store.read(|s| s.known_projects().to_vec());
        if let Some(project) = known.last() {
            return PathBuf::from(project);
        }
    }
    manox_agent::paths::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Start the WS gateway on the global tokio runtime: adopt the process-wide
/// `AgentServer` (get-or-init with `cwd`), bind `127.0.0.1:<port>` (0 →
/// random), publish the endpoint via [`service_endpoint`] and persist it to
/// `<config>/gateway-ws.json`. Fire-and-forget: a bind failure surfaces as a
/// tracing error and a permanently-`None` endpoint (callers waiting on
/// [`service_endpoint`] should bound their wait).
pub fn start(cwd: PathBuf, port: u16) {
    let server = crate::agent_server::global(cwd);
    manox_agent::runtime::handle().spawn(async move {
        if let Err(e) = listener::bind_and_serve(server, port).await {
            tracing::error!(error = %e, "gateway WS listener failed to start");
        }
    });
}

/// Record the bound endpoint and persist it (mode 0600 keeps the token local
/// to the user) so out-of-process tooling can reach the service.
pub(crate) fn publish(endpoint: GatewayEndpoint) {
    *endpoint_slot().lock().unwrap() = Some(endpoint.clone());
    let Ok(dir) = manox_agent::paths::manox_config_dir() else {
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            error = %e,
            "failed to create manox config dir for gateway-ws.json"
        );
        return;
    }
    let payload = serde_json::json!({
        "port": endpoint.port,
        "token": endpoint.token,
        "url": endpoint.url,
    });
    let path = dir.join("gateway-ws.json");
    if let Err(e) = std::fs::write(&path, payload.to_string()) {
        tracing::warn!(error = %e, path = %path.display(), "failed to write gateway-ws.json");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}
