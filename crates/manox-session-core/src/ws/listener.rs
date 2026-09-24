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
    /// The AHP host this listener also serves (`/ahp`): one process, one host,
    /// two protocols on the same loopback listener (v3 architecture §E.3).
    ahp: Arc<manox_ahp_runtime::ahp::runtime::AhpRuntime>,
}

/// Bind the loopback listener, publish the endpoint, then serve forever.
/// Runs on the global tokio runtime (the caller spawns it).
pub(super) async fn bind_and_serve(
    server: Arc<crate::agent_server::AgentServer>,
    port: u16,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    // Per-boot by default (the endpoint file is 0600 and rewritten each boot).
    // A remote client that keeps the address in its own settings needs a token
    // that outlives our restarts, so an explicit override wins when set.
    let token = std::env::var("MANOX_AHP_TOKEN")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
    let url = format!("http://127.0.0.1:{port}/");
    crate::ws::publish(crate::ws::GatewayEndpoint {
        url,
        port,
        token: token.clone(),
    });
    let cwd = super::default_cwd();
    let state = AppState {
        token,
        server,
        port,
        ahp: manox_ahp_runtime::ahp::runtime::runtime(cwd),
    };
    axum::serve(listener, build_router(state)).await?;
    Ok(())
}

/// Everything the listener does not serve is logged with its method and URI.
///
/// A remote client's first contact often misses the routes we expect (a base
/// URL probe, a manifest fetch, a different path); the failure it reports is
/// generic, so the request itself is the only reliable evidence.
async fn log_unmatched(
    uri: axum::http::Uri,
    headers: HeaderMap,
    method: axum::http::Method,
) -> Response {
    tracing::warn!(
        %method,
        %uri,
        origin = headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-"),
        user_agent = headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-"),
        "gateway: unmatched request"
    );
    StatusCode::NOT_FOUND.into_response()
}

fn build_router(state: AppState) -> Router {
    // Both routes ride one listener, one token and one machine lock; the AHP
    // route is the v3 face, `/ws` the retiring v2 face.
    let ahp = state.ahp.router("/ahp");
    Router::new()
        .route("/ws", get(ws_upgrade))
        .route("/ahp", get(ahp_upgrade))
        .with_state(state)
        .merge(ahp)
        .fallback(log_unmatched)
}

/// The AHP face's upgrade gate.
///
/// The token can arrive in several carriers and the client decides which one:
/// the endpoint file advertises `?token=`, VS Code's address parser reads
/// `?tkn=`, and a `connectionToken` in its settings may surface as an
/// `Authorization` header instead. All of them are the same secret, so all of
/// them are accepted — and every rejection is logged, because "WebSocket
/// connection failed" on the client side says nothing about why.
async fn ahp_upgrade(
    ws: WebSocketUpgrade,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    if !authorized(&state, &params, &headers) {
        tracing::warn!(
            origin = headers
                .get(header::ORIGIN)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("-"),
            user_agent = headers
                .get(header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("-"),
            // The token itself stays out of the log; four characters are enough
            // to tell "you pasted a stale one" from "the client sent none".
            sent_token_prefix = params
                .get("token")
                .or_else(|| params.get("tkn"))
                .map(|value| value.chars().take(4).collect::<String>())
                .unwrap_or_else(|| "-".to_string()),
            expected_token_prefix = state.token.chars().take(4).collect::<String>(),
            has_authorization = headers.contains_key(header::AUTHORIZATION),
            has_connection_token = headers.contains_key("x-connection-token"),
            "gateway: refusing /ahp upgrade — no matching token"
        );
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !origin_ok(&headers, state.port) {
        tracing::warn!("gateway: refusing /ahp upgrade — foreign origin");
        return StatusCode::FORBIDDEN.into_response();
    }
    tracing::info!(
        origin = headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-"),
        "gateway: accepted /ahp upgrade"
    );
    ws.on_upgrade(move |socket| async move {
        manox_ahp::transport::axum_ws::serve(socket, (*state.ahp.host()).clone()).await;
        tracing::info!("gateway: /ahp connection closed");
    })
}

/// Whether any accepted carrier presents the per-boot token.
fn authorized(state: &AppState, params: &HashMap<String, String>, headers: &HeaderMap) -> bool {
    let matches = |value: &str| value.trim() == state.token;
    let query = ["token", "tkn"]
        .iter()
        .any(|key| params.get(*key).is_some_and(|value| matches(value)));
    let header = [
        header::AUTHORIZATION.as_str(),
        "x-connection-token",
        "x-manox-token",
    ]
    .iter()
    .filter_map(|name| headers.get(*name).and_then(|value| value.to_str().ok()))
    .any(|value| matches(value.strip_prefix("Bearer ").unwrap_or(value)));
    query || header
}

/// Same-origin loopback browsers only: a page served from this listener has
/// the WS Origin `http://127.0.0.1:<port>`; foreign hosts, schemes, or ports
/// are rejected (DNS-rebinding / CSWSH protection).
fn origin_allowed(origin: &str, port: u16) -> bool {
    let Ok(uri) = Uri::try_from(origin) else {
        return false;
    };
    let scheme = uri.scheme_str().unwrap_or("");
    // The origin gate exists for *browsers* (CSWSH / DNS rebinding): a page can
    // only carry an `http(s)` origin, so a present origin naming some other
    // scheme is not a browser and cannot mount that attack. VS Code dials remote
    // agent hosts with `Origin: vscode-file://vscode-app`; rejecting it would
    // lock out the reference client for a threat it cannot pose. The token stays
    // the trust boundary there, exactly as it is for a client sending no origin.
    // An origin we cannot even parse (no host) is refused rather than waved
    // through: it is not evidence of anything.
    if scheme != "http" && scheme != "https" {
        return true;
    }
    let host = uri.host().unwrap_or("");
    let port_ok = uri.port_u16().map(|p| p == port).unwrap_or(false);
    // An `http(s)` origin must be this listener's own loopback origin; in
    // particular an `https` page on our plain-http port is not ours.
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

    /// Browsers must be same-origin loopback; everything else is not a browser
    /// and rides the token instead (the reference client dials with
    /// `Origin: vscode-file://vscode-app`).
    #[test]
    fn origin_allowed_checks_browser_origins_only() {
        assert!(origin_allowed("http://127.0.0.1:4321", 4321));
        assert!(origin_allowed("http://localhost:4321", 4321));
        assert!(!origin_allowed("http://127.0.0.1:9999", 4321), "wrong port");
        assert!(
            !origin_allowed("https://127.0.0.1:4321", 4321),
            "https loopback is not this listener"
        );
        assert!(
            !origin_allowed("http://evil.com:4321", 4321),
            "foreign browser origin"
        );
        assert!(
            origin_allowed("vscode-file://vscode-app", 4321),
            "the reference client is not a browser"
        );
        assert!(
            !origin_allowed("file:///x", 4321),
            "an origin with no host cannot be validated, so it is refused"
        );
    }

    /// The gateway face: a present **browser** Origin is validated; a missing
    /// Origin and a non-browser one pass — the token is their auth.
    #[test]
    fn origin_gate_validates_present_browser_origins_only() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "http://127.0.0.1:4321".parse().unwrap());
        assert!(origin_ok(&headers, 4321), "same-origin browser");
        headers.insert(header::ORIGIN, "http://evil.com:4321".parse().unwrap());
        assert!(!origin_ok(&headers, 4321), "foreign browser origin");
        assert!(origin_ok(&HeaderMap::new(), 4321), "no origin at all");
        headers.insert(header::ORIGIN, "vscode-file://vscode-app".parse().unwrap());
        assert!(origin_ok(&headers, 4321), "non-browser client origin");
    }
}
