//! Unified client-side connection + handshake wrapper for the [`AgentServer`].
//!
//! Every host (gpui desktop, napi/VS Code, WebUI bridge) repeats the same
//! three steps to reach the server: build an in-process connection pair, hand
//! the server end to [`AgentServer::accept`], and declare itself with the
//! `Initialize` handshake. `AgentClient` owns that sequence so a host only
//! picks a transport and a `client_id`; the pump and command paths then read
//! and write through the exposed connection.
//!
//! T-A scope: the wrapper is a faithful extraction — hosts keep their own pump
//! tasks and send shapes. It deliberately does not yet multiplex sessions or
//! await the `Ready` note; those land with the transport/multiplexing steps.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use manox_protocol::handshake::{HookKind, Initialize};
use manox_protocol::transport::{InProcessConnection, RpcConnection, in_process_pair};
use manox_protocol::{ClientCall, ClientNote, FromClient, MsgId, RpcError};

use crate::agent_server::AgentServer;

/// Monotonic source for [`AgentClient::send_call`] request ids, so concurrent
/// calls on one connection never collide on `MsgId`.
static CALL_SEQ: AtomicU64 = AtomicU64::new(0);

/// A client-side connection to an [`AgentServer`]: owns the transport's client
/// end and performs the `Initialize` handshake on construction.
pub struct AgentClient {
    conn: InProcessConnection,
    client_id: String,
}

impl AgentClient {
    /// Build an in-process connection to `server`, register it via
    /// [`AgentServer::accept`], and send the `Initialize` handshake. The
    /// message sequence matches the pre-existing per-host handshakes exactly
    /// (`MsgId::new("init")`, the caller's `capabilities` and `sessions`).
    pub fn connect(
        server: &AgentServer,
        client_id: impl Into<String>,
        capabilities: Vec<HookKind>,
        sessions: Vec<String>,
    ) -> Self {
        let (client_conn, server_conn) = in_process_pair();
        server.accept(Arc::new(server_conn));
        let client_id = client_id.into();
        client_conn.send_to_server(FromClient::Request {
            id: MsgId::new("init"),
            call: ClientCall::Initialize(Initialize {
                client_id: client_id.clone(),
                capabilities,
                sessions,
            }),
        });
        Self {
            conn: client_conn,
            client_id,
        }
    }

    /// The declared client identity.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Borrow the underlying connection (for the host's pump `server_rx()` and
    /// its own sends).
    pub fn conn(&self) -> &InProcessConnection {
        &self.conn
    }

    /// Consume the wrapper, returning the underlying connection for hosts that
    /// need to own it outright (e.g. move it into a pump thread).
    pub fn into_conn(self) -> InProcessConnection {
        self.conn
    }

    /// Send a [`ClientCall`] as a `Request` with a fresh unique [`MsgId`];
    /// returns the id so the caller can correlate the eventual `Response`.
    pub fn send_call(&self, call: ClientCall) -> MsgId {
        let id = MsgId::new(format!("call-{}", CALL_SEQ.fetch_add(1, Ordering::Relaxed)));
        self.conn.send_to_server(FromClient::Request {
            id: id.clone(),
            call,
        });
        id
    }

    /// Send a fire-and-forget [`ClientNote`].
    pub fn send_note(&self, note: ClientNote) {
        self.conn.send_to_server(FromClient::Notification { note });
    }

    /// Answer a server-originated `ServerCall` (Approve / PlanVerdict / ...).
    pub fn send_reply(&self, id: MsgId, outcome: Result<serde_json::Value, RpcError>) {
        self.conn.send_to_server(FromClient::Reply { id, outcome });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use manox_protocol::msg::FromServer;
    use manox_protocol::server::ServerNote;

    use super::*;
    use crate::test_support::{hermetic_home, init_globals, lock_globals};

    /// Poll the client end of `conn` from the test thread until a message
    /// arrives (the dispatch task runs on the agent runtime); `try_recv` +
    /// sleep mirrors the `agent_server` test helper and avoids blocking forever
    /// on a misrouted message.
    fn recv(conn: &InProcessConnection) -> FromServer {
        let rx = conn.server_rx();
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            match rx.try_recv() {
                Ok(m) => return m,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => panic!("timed out waiting for a server message"),
            }
        }
    }

    /// `connect` must reproduce the legacy handshake: the server acks the
    /// `Initialize` request and then announces `Ready`, both observable on the
    /// client end the wrapper exposes.
    #[test]
    fn connect_handshake_yields_ack_and_ready() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let server = AgentServer::new(std::path::PathBuf::from("/"));
        let client = AgentClient::connect(
            &server,
            "ta-test",
            vec![
                HookKind::Approve,
                HookKind::PlanVerdict,
                HookKind::AskUserQuestion,
            ],
            vec![],
        );
        assert_eq!(client.client_id(), "ta-test");
        let first = recv(client.conn());
        assert!(
            matches!(first, FromServer::Response { outcome: Ok(_), .. }),
            "expected Initialize ack, got {first:?}"
        );
        let second = recv(client.conn());
        assert!(
            matches!(
                second,
                FromServer::Notification {
                    note: ServerNote::Ready
                }
            ),
            "expected Ready, got {second:?}"
        );
    }

    /// A call sent through the wrapper reaches the server and the response
    /// echoes the generated request id back to the caller's connection.
    #[test]
    fn send_call_round_trips_response_id() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let server = AgentServer::new(std::path::PathBuf::from("/"));
        let client = AgentClient::connect(&server, "ta-call", vec![], vec![]);
        // Drain the handshake (ack + Ready).
        let _ = recv(client.conn());
        let _ = recv(client.conn());
        let id = client.send_call(ClientCall::ListThreads);
        // ListThreads responds (empty store) and additionally pushes a
        // ThreadsUpdated notification; find the Response and check its id.
        let mut got_response = false;
        for _ in 0..4 {
            match recv(client.conn()) {
                FromServer::Response { id: rid, .. } if rid == id => {
                    got_response = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(got_response, "no Response matched {id:?}");
    }
}
