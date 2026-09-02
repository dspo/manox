//! A WebSocket-backed [`RpcConnection`]: the browser speaks the typed
//! `FromClient`/`FromServer` protocol directly over the socket, no legacy
//! translation layer in between.
//!
//! One connection carries the whole protocol for one browser tab. Inbound WS
//! text/binary frames are parsed as `FromClient` and fed to the AgentServer;
//! `FromServer` messages are serialized back to the socket. The browser is
//! expected to send the `Initialize` handshake first (client_id `webui-{id}`).
//!
//! `AgentServer::accept` is `&self`/`Sync`, so a connection can be accepted
//! straight from the WS handler without routing through the pump task.

use std::sync::Arc;

use async_channel::{Receiver, Sender, bounded};
use axum::extract::ws::{Message, WebSocket};
use futures::{SinkExt, StreamExt};
use manox_protocol::transport::{BackpressurePolicy, RpcConnection};
use manox_protocol::{FromClient, FromServer};
use tokio::task::JoinHandle;

/// Channel capacity mirrors the in-process transport's backpressure bound.
const CAPACITY: usize = manox_protocol::transport::BACKPRESSURE_CAPACITY;

/// A [`RpcConnection`] bridging an axum WebSocket to the typed protocol.
///
/// Two pump tasks run on the agent runtime: one reads WS frames → parses
/// `FromClient` → `c2s_tx` (which the AgentServer consumes via `client_rx`);
/// the other reads `s2c_rx` (what the AgentServer writes via
/// `send_to_client`) → serializes `FromServer` → WS text frames.
pub struct WebSocketConnection {
    c2s_tx: Sender<FromClient>,
    c2s_rx: Receiver<FromClient>,
    s2c_tx: Sender<FromServer>,
    s2c_rx: Receiver<FromServer>,
    /// Keeps the pumps alive for the connection's lifetime.
    _pumps: [JoinHandle<()>; 2],
}

impl WebSocketConnection {
    /// Wrap an already-upgraded axum WebSocket and spawn the two pump tasks on
    /// the agent runtime. Hand the result to `AgentServer::accept`.
    pub fn new(socket: WebSocket) -> Arc<Self> {
        let (mut ws_tx, mut ws_rx) = socket.split();
        let (c2s_tx, c2s_rx) = bounded(CAPACITY);
        let (s2c_tx, s2c_rx) = bounded(CAPACITY);

        // Inbound: WS frames -> FromClient -> c2s channel. Dropping the sender
        // on WS close lets the AgentServer's `client_rx` see EOF and tear down.
        let inbound_c2s = c2s_tx.clone();
        let inbound: JoinHandle<()> = manox_agent::runtime::handle().spawn(async move {
            while let Some(frame) = ws_rx.next().await {
                let Ok(frame) = frame else { break };
                let text = match frame {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => match String::from_utf8(b.into()) {
                        Ok(s) => s,
                        Err(_) => continue,
                    },
                    _ => continue,
                };
                let Ok(fc) = serde_json::from_str::<FromClient>(&text) else {
                    continue;
                };
                if inbound_c2s.send(fc).await.is_err() {
                    break;
                }
            }
            drop(inbound_c2s);
        });

        // Outbound: s2c channel -> FromServer -> WS text frames.
        let outbound_s2c_rx = s2c_rx.clone();
        let outbound: JoinHandle<()> = manox_agent::runtime::handle().spawn(async move {
            while let Ok(msg) = outbound_s2c_rx.recv().await {
                let Ok(text) = serde_json::to_string(&msg) else {
                    continue;
                };
                if ws_tx.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
        });

        Arc::new(Self {
            c2s_tx,
            c2s_rx,
            s2c_tx,
            s2c_rx,
            _pumps: [inbound, outbound],
        })
    }
}

impl RpcConnection for WebSocketConnection {
    fn send_to_client(&self, msg: FromServer) {
        // Match the in-process transport's backpressure semantics: streaming
        // notes drop under pressure, control traffic blocks.
        match &msg {
            FromServer::Notification { note }
                if note.backpressure_policy() == BackpressurePolicy::Drop =>
            {
                let _ = self.s2c_tx.try_send(msg);
            }
            _ => {
                let _ = self.s2c_tx.send_blocking(msg);
            }
        }
    }

    fn send_to_server(&self, msg: FromClient) {
        let _ = self.c2s_tx.send_blocking(msg);
    }

    fn client_rx(&self) -> Receiver<FromClient> {
        self.c2s_rx.clone()
    }

    fn server_rx(&self) -> Receiver<FromServer> {
        self.s2c_rx.clone()
    }

    fn disconnect(&self) {
        self.c2s_tx.close();
        self.s2c_tx.close();
    }
}
