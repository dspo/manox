//! The loopback listener: binds `127.0.0.1:<port>`, serves the `/ws`
//! upgrade route, and hands every accepted socket to the `AgentServer`.
//!
//! Auth: every client must present the per-boot token (`?token=`); browser
//! clients additionally pass the same-origin check (a present WS Origin must
//! be this listener's own loopback origin — DNS-rebinding/CSWSH protection).
//! A missing Origin marks a non-browser client, for which the token alone
//! authenticates (the old webui server rejected those; the transport is now
//! a general gateway face).

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

#[derive(Clone)]
struct AppState {
    token: String,
    server: Arc<crate::agent_server::AgentServer>,
    port: u16,
}

/// Bind the loopback listener, publish the endpoint, then serve forever.
/// Runs on the global tokio runtime (the caller spawns it).
pub(super) async fn bind_and_serve(
    server: Arc<crate::agent_server::AgentServer>,
    port: u16,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let url = format!("http://127.0.0.1:{port}/");
    crate::ws::publish(crate::ws::GatewayEndpoint {
        url,
        port,
        token: token.clone(),
    });
    let state = AppState {
        token,
        server,
        port,
    };
    axum::serve(listener, build_router(state)).await?;
    Ok(())
}

fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/ws", get(ws_upgrade))
        .with_state(state)
}

/// Same-origin loopback browsers only: a page served from this listener has
/// the WS Origin `http://127.0.0.1:<port>`; foreign hosts, schemes, or ports
/// are rejected (DNS-rebinding / CSWSH protection).
fn origin_allowed(origin: &str, port: u16) -> bool {
    let Ok(uri) = Uri::try_from(origin) else {
        return false;
    };
    let scheme = uri.scheme_str().unwrap_or("");
    let host = uri.host().unwrap_or("");
    let port_ok = uri.port_u16().map(|p| p == port).unwrap_or(false);
    scheme == "http" && (host == "127.0.0.1" || host == "localhost") && port_ok
}

/// The Origin gate: a present Origin must pass [`origin_allowed`]; a missing
/// Origin (a non-browser client) passes — the token is its auth.
fn origin_ok(headers: &HeaderMap, port: u16) -> bool {
    match headers.get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        Some(origin) => origin_allowed(origin, port),
        None => true,
    }
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    let token_ok = params.get("token").is_some_and(|t| t == &state.token);
    if !token_ok {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !origin_ok(&headers, state.port) {
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.on_upgrade(move |socket| handle_ws(socket, state.server))
}

async fn handle_ws(socket: WebSocket, server: Arc<crate::agent_server::AgentServer>) {
    // The client speaks the typed FromClient/FromServer protocol directly;
    // the WebSocketConnection pumps frames to/from the AgentServer. The
    // client must send the Initialize handshake first (§D.2).
    let conn = super::WebSocketConnection::new(socket);
    server.accept(conn);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only same-origin loopback browsers pass the Origin check.
    #[test]
    fn origin_allowed_checks_loopback_scheme_and_port() {
        assert!(origin_allowed("http://127.0.0.1:4321", 4321));
        assert!(origin_allowed("http://localhost:4321", 4321));
        assert!(!origin_allowed("http://127.0.0.1:9999", 4321), "wrong port");
        assert!(
            !origin_allowed("https://127.0.0.1:4321", 4321),
            "wrong scheme"
        );
        assert!(
            !origin_allowed("http://evil.com:4321", 4321),
            "foreign host"
        );
        assert!(!origin_allowed("", 4321), "missing origin");
        assert!(!origin_allowed("file:///x", 4321), "non-http origin");
    }

    /// The gateway face: a present Origin is validated; a missing Origin
    /// (non-browser client) passes — the token is its auth.
    #[test]
    fn origin_gate_validates_present_origins_only() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "http://127.0.0.1:4321".parse().unwrap());
        assert!(origin_ok(&headers, 4321), "same-origin browser");
        headers.insert(header::ORIGIN, "http://evil.com:4321".parse().unwrap());
        assert!(!origin_ok(&headers, 4321), "foreign browser origin");
        assert!(origin_ok(&HeaderMap::new(), 4321), "non-browser client");
    }
}
