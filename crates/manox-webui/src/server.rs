//! The in-process HTTP + WebSocket server: serves the embedded webview
//! bundle and upgrades `/ws` into per-connection bridges.
//!
//! The service binds `127.0.0.1:0` (loopback, random port). Static assets
//! carry no token; `/` injects the per-boot token so the browser's
//! `web-bridge` can authenticate its socket, and the socket additionally
//! checks the Origin header so a hostile page on loopback cannot probe the
//! server (DNS rebinding / CSWSH). Each connection registers with the
//! main-thread pump and tears down via `detach_session` on close.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Router;
use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use include_dir::{Dir, include_dir};

/// The committed `apps/web/webui/dist` build — embedded into the binary so the app
/// stays single-process. Rebuilt by `npm run build` in `apps/web/webui/`.
static WEBUI_DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../apps/web/webui/dist");

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct AppState {
    token: String,
    server: Arc<manox_session_core::agent_server::AgentServer>,
    port: u16,
}

/// Bind a loopback listener, publish the endpoint, then serve forever. Runs
/// on the global tokio runtime (`manox_agent::runtime::handle`) so no new runtime
/// is created.
pub(crate) async fn bind_and_serve() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let url = format!("http://127.0.0.1:{port}/");
    let server = crate::webui_agent_server()
        .ok_or_else(|| anyhow::anyhow!("webui agent server not started"))?;
    crate::service_started(crate::WebuiService {
        url,
        token: token.clone(),
        port,
    });
    let state = AppState {
        token,
        server,
        port,
    };
    axum::serve(listener, build_router(state)).await?;
    Ok(())
}

pub(crate) fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/{*path}", get(assets))
        .route("/ws", get(ws_upgrade))
        .with_state(state)
}

async fn index(State(state): State<AppState>) -> Response {
    let html = format!(
        r#"<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>manox webui</title>
    <link rel="stylesheet" href="/assets/webview/bundle.css" />
    <script>window.__MANOX_TOKEN__ = '{token}';</script>
  </head>
  <body>
    <div id="root"></div>
    <script src="/assets/webview/bundle.js"></script>
  </body>
</html>"#,
        token = state.token,
    );
    (
        StatusCode::OK,
        [(
            header::CONTENT_SECURITY_POLICY,
            "default-src 'self'; script-src 'self' 'unsafe-inline'; \
             style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             connect-src 'self' ws: wss:",
        )],
        Html(html),
    )
        .into_response()
}

/// Serve an embedded asset under `dist/webview/` (e.g. `webview/bundle.js`).
async fn assets(axum::extract::Path(path): axum::extract::Path<String>) -> Response {
    let Some(file) = WEBUI_DIST.get_file(&path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let content_type = match path.rsplit('.').next() {
        Some("js") => "application/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("map") => "application/json",
        _ => "application/octet-stream",
    };
    ([(header::CONTENT_TYPE, content_type)], file.contents()).into_response()
}

/// Only same-origin browser sockets are allowed: the page loads from
/// `http://127.0.0.1:<port>`, so its WS Origin is that exact origin. A
/// missing Origin (non-browser client) or a foreign host is rejected.
fn origin_allowed(origin: &str, port: u16) -> bool {
    let Ok(uri) = Uri::try_from(origin) else {
        return false;
    };
    let scheme = uri.scheme_str().unwrap_or("");
    let host = uri.host().unwrap_or("");
    let port_ok = uri.port_u16().map(|p| p == port).unwrap_or(false);
    scheme == "http" && (host == "127.0.0.1" || host == "localhost") && port_ok
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
    let origin_ok = headers
        .get(header::ORIGIN)
        .and_then(|o| o.to_str().ok())
        .is_some_and(|o| origin_allowed(o, state.port));
    if !origin_ok {
        return StatusCode::FORBIDDEN.into_response();
    }
    ws.on_upgrade(move |socket| handle_ws(socket, state.server))
}

async fn handle_ws(socket: WebSocket, server: Arc<manox_session_core::agent_server::AgentServer>) {
    let id = NEXT_CONN_ID.fetch_add(1, Ordering::SeqCst);
    // The browser speaks the typed FromClient/FromServer protocol directly;
    // the WebSocketConnection pumps frames to/from the AgentServer. The
    // browser must send the Initialize handshake first (client_id webui-{id}
    // is minted by the browser; the id here is only for logging/tracing).
    let _ = id;
    let conn = crate::ws_connection::WebSocketConnection::new(socket);
    server.accept(conn);
}

#[cfg(test)]
mod tests {
    use super::AppState;
    use manox_session_core::agent_server::AgentServer;

    fn test_state() -> AppState {
        AppState {
            token: "tok123".to_string(),
            server: Arc::new(AgentServer::new(std::path::PathBuf::from("/"))),
            port: 4321,
        }
    }

    /// `/` injects the per-boot token into the page so the webview bridge can
    /// authenticate its socket — the single handshake entry point.
    #[tokio::test]
    async fn index_injects_token() {
        let resp = index(State(test_state())).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(html.contains("__MANOX_TOKEN__ = 'tok123'"));
        assert!(html.contains("/assets/webview/bundle.js"));
    }

    /// The committed `apps/web/webui/dist` build is embedded; `/assets/webview/bundle.js`
    /// must resolve so the browser actually boots the app (an empty shell is a
    /// routing regression, not the intended home state).
    #[tokio::test]
    async fn assets_serves_embedded_bundle() {
        let resp = assets(Path("webview/bundle.js".to_string())).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert_eq!(content_type, "application/javascript");
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert!(!body.is_empty(), "bundle.js must be non-empty");
    }

    #[tokio::test]
    async fn assets_404s_unknown_path() {
        let resp = assets(Path("nope/missing.js".to_string())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Only same-origin loopback browsers may open the socket.
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
}
