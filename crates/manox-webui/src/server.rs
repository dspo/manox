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
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use futures::{SinkExt, StreamExt};
use include_dir::{Dir, include_dir};
use serde_json::Value;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::bridge::{self, Outbound};

/// The committed `webui/dist` build — embedded into the binary so the app
/// stays single-process. Rebuilt by `npm run build` in `webui/`.
static WEBUI_DIST: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../webui/dist");

static NEXT_CONN_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct AppState {
    token: String,
    conn_tx: UnboundedSender<crate::ToMain>,
    port: u16,
}

/// Bind a loopback listener, publish the endpoint, then serve forever. Runs
/// on the global tokio runtime (`agent::runtime::handle`) so no new runtime
/// is created.
pub(crate) async fn bind_and_serve() -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let token = uuid::Uuid::new_v4().simple().to_string();
    let url = format!("http://127.0.0.1:{port}/");
    let conn_tx =
        crate::main_channel_sender().ok_or_else(|| anyhow::anyhow!("webui pump not started"))?;
    crate::service_started(crate::WebuiService {
        url,
        token: token.clone(),
        port,
    });
    let state = AppState {
        token,
        conn_tx,
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
    ws.on_upgrade(move |socket| handle_ws(socket, state.conn_tx.clone()))
}

async fn handle_ws(socket: WebSocket, conn_tx: UnboundedSender<crate::ToMain>) {
    let (ws_tx, mut ws_rx) = socket.split();
    let (cmd_tx, cmd_rx) = unbounded_channel::<Value>();
    let (frame_tx, frame_rx) = unbounded_channel::<Value>();
    let (tick_tx, tick_rx) = unbounded_channel::<()>();
    let outbound = Arc::new(Outbound::new(frame_tx, tick_tx));
    let id = NEXT_CONN_ID.fetch_add(1, Ordering::SeqCst);
    if conn_tx
        .send(crate::ToMain::Connect(crate::ConnectionHandle {
            id,
            cmd_rx,
            outbound: outbound.clone(),
        }))
        .is_err()
    {
        return;
    }
    let sender = spawn_sender(tick_rx, frame_rx, ws_tx, outbound.clone());
    while let Some(Ok(msg)) = ws_rx.next().await {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if cmd_tx.send(v).is_err() {
            break;
        }
    }
    let _ = conn_tx.send(crate::ToMain::Disconnect(id));
    drop(sender);
    drop(outbound);
    drop(cmd_tx);
}

/// The per-connection outbound task: coalesces batched events into 33ms
/// frames and relays bypass frames (`thread_info`, `session_ready`, global
/// snapshots) immediately. Exits when the pump drops the connection's
/// outbound (closing the frame/tick channels) or the socket errors.
fn spawn_sender(
    mut tick_rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    mut frame_rx: tokio::sync::mpsc::UnboundedReceiver<Value>,
    ws_tx: futures::stream::SplitSink<WebSocket, Message>,
    outbound: Arc<Outbound>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ws_tx = ws_tx;
        let mut flush_at: Option<tokio::time::Instant> = None;
        let sleep = tokio::time::sleep(Duration::ZERO);
        tokio::pin!(sleep);
        loop {
            tokio::select! {
                frame = frame_rx.recv() => {
                    match frame {
                        Some(frame) => {
                            if ws_tx
                                .send(Message::Text(frame.to_string().into()))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                tick = tick_rx.recv() => {
                    if tick.is_none() {
                        break;
                    }
                    while tick_rx.try_recv().is_ok() {}
                    let deadline = tokio::time::Instant::now()
                        + Duration::from_millis(bridge::BATCH_MS);
                    flush_at = Some(deadline);
                    sleep.as_mut().reset(deadline);
                }
                _ = &mut sleep, if flush_at.is_some() => {
                    outbound.flush();
                    flush_at = None;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::extract::{Path, State};
    use tokio::sync::mpsc::unbounded_channel;

    fn test_state() -> AppState {
        AppState {
            token: "tok123".to_string(),
            conn_tx: unbounded_channel().0,
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

    /// The committed `webui/dist` build is embedded; `/assets/webview/bundle.js`
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
