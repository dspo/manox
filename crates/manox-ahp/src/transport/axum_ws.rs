//! The WebSocket host transport (axum `WebSocketUpgrade`).
//!
//! WebSocket is AHP's most common transport and the one the `/ahp` gateway
//! route serves: one complete JSON-RPC message per text frame, server side
//! listening. `ahp-ws` (the SDK's adapter) dials only, so the accepting side is
//! written here: two tasks — one draining the host's outbound channel into text
//! frames, one decoding inbound frames into the host's inbound channel.

use axum::Router;
use axum::extract::{
    State,
    ws::{Message, WebSocket, WebSocketUpgrade},
};
use axum::response::Response;
use axum::routing::any;
use futures::{SinkExt, StreamExt};

use super::HostTransport;
use super::inproc::{ChanSink, ChanSource};
use crate::host::Host;
use crate::wire;

/// A router mounting the AHP endpoint at `path` (`/ahp` in the gateway).
///
/// The gateway composes this with its own listener so token checking, the
/// machine-singleton lock and the endpoint file stay in one place: one process,
/// one AHP host, one `/ahp` route.
pub fn router(path: &str, host: Host) -> Router {
    Router::new().route(path, any(upgrade)).with_state(host)
}

async fn upgrade(State(host): State<Host>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| serve(socket, host))
}

/// Serve an accepted WebSocket for its whole lifetime.
///
/// The upgrade callback's future owns the socket: returning from it closes the
/// connection. Spawning the pump tasks and returning immediately therefore kills
/// the socket the instant the handshake completes, so this awaits both halves
/// and only returns once the peer is gone.
pub async fn serve(socket: WebSocket, host: Host) {
    let (out_tx, out_rx) = async_channel::unbounded();
    let (in_tx, in_rx) = async_channel::unbounded();
    let (mut sink, mut stream) = socket.split();

    let writer = tokio::spawn(async move {
        while let Ok(msg) = out_rx.recv().await {
            let text = wire::to_text(&msg);
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
        let _ = sink.close().await;
    });

    let reader = tokio::spawn(async move {
        while let Some(frame) = stream.next().await {
            match frame {
                Ok(Message::Text(text)) => {
                    if let Ok(msg) = wire::parse_text(&text)
                        && in_tx.send(msg).await.is_err()
                    {
                        break;
                    }
                }
                Ok(Message::Binary(bytes)) => {
                    if let Ok(text) = std::str::from_utf8(&bytes)
                        && let Ok(msg) = wire::parse_text(text)
                        && in_tx.send(msg).await.is_err()
                    {
                        break;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    host.accept(HostTransport::new(
        Box::new(ChanSink { tx: out_tx }),
        Box::new(ChanSource { rx: in_rx }),
    ));
    let _ = tokio::join!(writer, reader);
}
