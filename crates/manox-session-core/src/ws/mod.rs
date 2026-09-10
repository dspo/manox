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

/// The listener lifecycle, one slot: [`ListenerState::Starting`] from
/// `start` until the bind publishes, [`ListenerState::Up`] after. The
/// reservation makes a second `start` (sequential OR racing) a loud no-op
/// instead of a second listener silently overwriting the published
/// endpoint (round 3 §三.2 / OPEN-2).
#[derive(Clone, Debug)]
enum ListenerState {
    Starting,
    Up(GatewayEndpoint),
}

static ENDPOINT: OnceLock<Mutex<Option<ListenerState>>> = OnceLock::new();

fn endpoint_slot() -> &'static Mutex<Option<ListenerState>> {
    ENDPOINT.get_or_init(|| Mutex::new(None))
}

/// The bound endpoint, if the listener is up.
pub fn service_endpoint() -> Option<GatewayEndpoint> {
    match endpoint_slot().lock().unwrap().clone() {
        Some(ListenerState::Up(endpoint)) => Some(endpoint),
        _ => None,
    }
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
    // Second-start guard (§三.2): the slot is RESERVED under one lock hold
    // before the bind is spawned, so neither a sequential nor a racing
    // second `start` can bind a second listener or overwrite the published
    // endpoint — it warns and returns instead.
    {
        let mut slot = endpoint_slot().lock().unwrap();
        if slot.is_some() {
            tracing::warn!(
                "gateway WS listener already started; ignoring the second start \
                 (the published endpoint stays the first bind's)"
            );
            return;
        }
        *slot = Some(ListenerState::Starting);
    }
    let server = crate::agent_server::global(cwd);
    manox_agent::runtime::handle().spawn(async move {
        if let Err(e) = listener::bind_and_serve(server, port).await {
            tracing::error!(error = %e, "gateway WS listener failed to start");
            // A failed bind leaves the reservation pointing at nothing —
            // release the slot so a later start can retry.
            let mut slot = endpoint_slot().lock().unwrap();
            if matches!(*slot, Some(ListenerState::Starting)) {
                *slot = None;
            }
        }
    });
}

/// Record the bound endpoint and persist it (mode 0600 keeps the token local
/// to the user) so out-of-process tooling can reach the service.
pub(crate) fn publish(endpoint: GatewayEndpoint) {
    *endpoint_slot().lock().unwrap() = Some(ListenerState::Up(endpoint.clone()));
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
    // §三.2: the file is CREATED with mode 0600 (OpenOptions::mode), never
    // write-then-chmod — the old order left a 0644 window on multi-user
    // machines carrying the live per-boot token. The follow-up
    // set_permissions only tightens legacy files written by older builds;
    // it is not part of the fresh-file path's security.
    #[cfg(unix)]
    let write_result = (|| -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(payload.to_string().as_bytes())?;
        file.sync_all()
    })();
    #[cfg(not(unix))]
    let write_result = std::fs::write(&path, payload.to_string());
    if let Err(e) = write_result {
        tracing::warn!(error = %e, path = %path.display(), "failed to write gateway-ws.json");
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// §三.2 / OPEN-2, the transport e2e: bind → publish (0600 file) →
    /// no-token upgrade rejected 401 → token upgrade succeeds. This is the
    /// face `cx web` serves; until W7 it had no integration coverage at all.
    // The globals guard is the suite's test-serialization mutex; the
    // current-thread test runtime has no re-entrant taker, so holding it
    // across the test's own awaits is safe.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn ws_end_to_end_binds_publishes_and_authenticates() {
        let _g = crate::test_support::lock_globals();
        crate::test_support::hermetic_home();
        crate::test_support::init_globals();
        manox_agent::thread_store::init();
        // The endpoint slot is process-global; own it for this test and
        // leave it empty again on the way out.
        *endpoint_slot().lock().unwrap() = None;

        let server = std::sync::Arc::new(
            crate::agent_server::AgentServer::new_without_store_watcher(PathBuf::from("/")),
        );
        let serve = tokio::spawn(async move {
            let _ = listener::bind_and_serve(server, 0).await;
        });

        // Bind + publish (bounded wait).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let endpoint = loop {
            if let Some(endpoint) = service_endpoint() {
                break endpoint;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "listener never published its endpoint"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };

        // The published file exists and carries the token at mode 0600.
        let file = manox_agent::paths::manox_config_dir()
            .expect("config dir")
            .join("gateway-ws.json");
        let text = std::fs::read_to_string(&file).expect("gateway-ws.json written");
        assert!(text.contains(&endpoint.token), "the file carries the token");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "gateway-ws.json stays user-only");
        }

        // No token → 401 (the HTTP rejection surfaces as tungstenite's
        // Http error carrying the status).
        let denied =
            tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/ws", endpoint.port)).await;
        assert!(denied.is_err(), "a tokenless upgrade must be rejected");
        match denied.unwrap_err() {
            tokio_tungstenite::tungstenite::Error::Http(resp) => {
                assert_eq!(resp.status(), 401, "the tokenless leg answers 401");
            }
            other => panic!("expected the HTTP 401 rejection, got {other:?}"),
        }

        // With the token → the upgrade succeeds (then close).
        let allowed = tokio_tungstenite::connect_async(format!(
            "ws://127.0.0.1:{}/ws?token={}",
            endpoint.port, endpoint.token
        ))
        .await;
        assert!(allowed.is_ok(), "the tokened upgrade must succeed");
        let (mut socket, _resp) = allowed.unwrap();
        let _ = socket.close(None).await;

        serve.abort();
        *endpoint_slot().lock().unwrap() = None;
        manox_agent::thread_store::drop_global_for_test();
    }

    /// §三.2: the second-start guard. With the slot reserved, `start` must
    /// return without binding — the published endpoint stays the first
    /// bind's (pre-fix red: the second start overwrote it).
    #[allow(clippy::await_holding_lock)] // same test-guard rationale as above
    #[tokio::test]
    async fn second_start_is_a_loud_noop() {
        let _g = crate::test_support::lock_globals();
        crate::test_support::hermetic_home();
        crate::test_support::init_globals();
        manox_agent::thread_store::init();
        // Reserve, as a live listener would.
        *endpoint_slot().lock().unwrap() = Some(ListenerState::Starting);

        start(PathBuf::from("/"), 0);

        // The reservation is untouched and nothing published: a second
        // start neither bound nor overwrote (settle to catch a spawned
        // bind racing the assertion).
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(
            service_endpoint().is_none(),
            "a second start must not publish over a live reservation"
        );
        assert!(matches!(
            *endpoint_slot().lock().unwrap(),
            Some(ListenerState::Starting)
        ));

        *endpoint_slot().lock().unwrap() = None;
        manox_agent::thread_store::drop_global_for_test();
    }
}
