//! Transport abstraction + in-process implementation + backpressure.
//!
//! A [`RpcConnection`] is one full-duplex wire between the agent server and a
//! frontend. Transports (in-process channel, WebSocket, napi, tauri) implement
//! it; [`in_process_pair`] wires two ends over bounded `async_channel`s for the
//! gpui desktop app. [`RpcPeer`] correlates outstanding request/response and
//! call/reply pairs by [`crate::MsgId`].
//!
//! Scope decision (ε closeout, 2026-08-31): no tauri transport is planned —
//! the transport surface stays in-process (desktop) and serde-serialized
//! (napi, and the WS gateway in `manox-session-core::ws`). A tauri shell,
//! if ever pursued, opens its own plan.

use std::collections::HashMap;
use std::sync::Arc;

use async_channel::{Receiver, Sender};
use parking_lot::Mutex;

use crate::msg::{FromClient, FromServer, MsgId, RpcError};
use crate::server::ServerNote;

/// Per-client event channel capacity. Bounded so a slow client cannot grow
/// server memory without bound; overflow is governed by [`BackpressurePolicy`].
pub const BACKPRESSURE_CAPACITY: usize = 1024;

/// Overflow policy when a client's event channel is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressurePolicy {
    /// Streaming payload: drop the message (caller may coalesce and re-send
    /// with a gap marker). The connection stays up.
    ///
    /// Legacy (v1) class only: under protocol v2 (§D.7) the Drop class is
    /// DOOMED — durable delta traffic moves to
    /// [`crate::stream::StreamFrame::Entry`], which is [`Self::BoundedResync`]
    /// (L5: snapshots never drop, overflow resyncs; silent drops of journal
    /// entries would open gaps a client cannot close).
    Drop,
    /// Control / lifecycle message: the client is presumed dead; disconnect.
    /// §D.7 "control frames block, never drop": Request/Response/Reply and
    /// host traffic take this class (blocking send on the in-process pair).
    Disconnect,
    /// §D.7: `StreamItem(Snapshot | Projections)` and `StreamEnd` — must
    /// never be dropped; the sender blocks until capacity frees (same
    /// mechanics as [`Self::Disconnect`] on the in-process pair).
    NeverDrop,
    /// §D.7: `StreamItem(Entry)` — a bounded queue
    /// ([`crate::stream::ENTRY_BACKPRESSURE_CAPACITY`]); when full, the
    /// stream is ended with
    /// [`StreamEndReason::Resync`](crate::stream::StreamEndReason::Resync)
    /// and the client re-follows from a fresh snapshot (L5 — no server-side
    /// replay buffers).
    BoundedResync,
}

impl ServerNote {
    /// Legacy (v1) streaming classification — kept verbatim because live
    /// consumers (the session-core ws gateway and pumps) compare against
    /// `Drop`/`Disconnect`. The §D.7 successor strategy is expressed over
    /// the v2 vocabulary: [`crate::stream::StreamFrame::backpressure_policy`]
    /// (Snapshot/Projections/StreamEnd never drop; Entry bounded ⇒ resync)
    /// and [`crate::stream::v2_backpressure_policy`] for the host/legacy
    /// notification stream. Wiring those into the transports (and deleting
    /// the `Drop` class with the doomed notes) is the T4/T5 envelope
    /// migration.
    pub fn backpressure_policy(&self) -> BackpressurePolicy {
        match self {
            ServerNote::ModelText { .. } | ServerNote::ModelThinking { .. } => {
                BackpressurePolicy::Drop
            }
            _ => BackpressurePolicy::Disconnect,
        }
    }

    /// The session a notification belongs to, if any. `Error` carries an
    /// optional session id (a global error has none); the bare-model
    /// completion notes (`ModelText`/`ModelThinking`/`ModelToolCall`/
    /// `ModelChatDone`) are keyed by `request_id`, not session; the registry
    /// snapshots (`Ready`/`Models`/`ThreadsUpdated`/`Commands`) are global.
    /// A multiplexed client routes on this to demux one connection across
    /// many sessions.
    pub fn session_id(&self) -> Option<&str> {
        use ServerNote::*;
        match self {
            SessionCreated { session_id, .. } | SessionDisposed { session_id, .. } => {
                Some(session_id)
            }
            Error { session_id, .. } => session_id.as_deref(),
            Ready
            | Models { .. }
            | ThreadsUpdated { .. }
            | Commands { .. }
            | ModelText { .. }
            | ModelThinking { .. }
            | ModelToolCall { .. }
            | ModelChatDone { .. } => None,
        }
    }
}

/// One full-duplex connection between the agent server and a frontend.
pub trait RpcConnection: Send + Sync {
    /// Server → client.
    fn send_to_client(&self, msg: FromServer);
    /// Client → server.
    fn send_to_server(&self, msg: FromClient);
    /// Server consumes client messages.
    fn client_rx(&self) -> Receiver<FromClient>;
    /// Client consumes server messages.
    fn server_rx(&self) -> Receiver<FromServer>;
    /// Close both directions.
    fn disconnect(&self);
}

/// In-process [`RpcConnection`] over bounded channels. Cloneable: each clone
/// shares the channel pair, so both the pump (reading server→client) and the
/// workspace (sending client→server) can hold a reference.
#[derive(Clone)]
pub struct InProcessConnection {
    c2s_tx: Sender<FromClient>,
    c2s_rx: Receiver<FromClient>,
    s2c_tx: Sender<FromServer>,
    s2c_rx: Receiver<FromServer>,
}

/// Two [`InProcessConnection`] ends sharing one pair of channels.
///
/// Unbounded by design: both ends live in one process, so the only cost of
/// queue depth is memory — while a bounded pair deadlocks the moment both
/// directions fill at once (each side parked in `send_blocking` waiting for
/// the other to consume; the gpui main thread frozen mid-submit is the
/// round-10 "UI shows no reaction" repro). Flooding policies still apply per
/// message via [`BackpressurePolicy`] on the bounded test pair.
pub fn in_process_pair() -> (InProcessConnection, InProcessConnection) {
    let (c2s_tx, c2s_rx) = async_channel::unbounded();
    let (s2c_tx, s2c_rx) = async_channel::unbounded();
    let client = InProcessConnection {
        c2s_tx: c2s_tx.clone(),
        c2s_rx: c2s_rx.clone(),
        s2c_tx: s2c_tx.clone(),
        s2c_rx: s2c_rx.clone(),
    };
    let server = InProcessConnection {
        c2s_tx,
        c2s_rx,
        s2c_tx,
        s2c_rx,
    };
    (client, server)
}

/// Capacity-injectable variant for backpressure tests: a small buffer makes
/// overflow reachable without flooding. Production callers use the unbounded
/// [`in_process_pair`].
pub fn in_process_pair_with_capacity(cap: usize) -> (InProcessConnection, InProcessConnection) {
    let (c2s_tx, c2s_rx) = async_channel::bounded(cap);
    let (s2c_tx, s2c_rx) = async_channel::bounded(cap);
    let client = InProcessConnection {
        c2s_tx: c2s_tx.clone(),
        c2s_rx: c2s_rx.clone(),
        s2c_tx: s2c_tx.clone(),
        s2c_rx: s2c_rx.clone(),
    };
    let server = InProcessConnection {
        c2s_tx,
        c2s_rx,
        s2c_tx,
        s2c_rx,
    };
    (client, server)
}

impl RpcConnection for InProcessConnection {
    fn send_to_client(&self, msg: FromServer) {
        // Full channel: apply backpressure policy for streaming notes; every
        // other message is delivered blocking so control traffic is never lost
        // silently. A closed channel means the peer disconnected — drop.
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

/// Waiter map: outstanding request/response and call/reply pairs.
type PendingMap = HashMap<MsgId, Sender<Result<serde_json::Value, RpcError>>>;

/// Correlates outstanding request/response and call/reply pairs for one peer.
///
/// The issuer registers a waiter for a fresh [`MsgId`] before sending the
/// request/call, then awaits the returned receiver (applying its own timeout).
/// The responder side calls [`RpcPeer::complete`] when the matching
/// reply/response arrives. [`RpcPeer::cancel`] resolves a waiter with an error
/// (e.g. when the peer disconnects mid-call).
///
/// The peer is a shared handle: clones observe the same waiter map.
#[derive(Clone, Default)]
pub struct RpcPeer {
    pending: Arc<Mutex<PendingMap>>,
}

impl RpcPeer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a waiter for `id`; returns the receiver it resolves on.
    ///
    /// A duplicate registration of the same id is refused: `None` answers
    /// and the FIRST waiter stays registered (GW2 — the pre-fix map insert
    /// clobbered the earlier sender, dropping it, which closed the first
    /// waiter's receiver immediately; callers fold a closed receiver into a
    /// fail-closed rejection, so a double-routed `ServerCall` auto-denied
    /// the approval before the user could answer). Callers must treat
    /// `None` as a routing bug and skip the delivery fail-closed.
    pub fn register(&self, id: MsgId) -> Option<Receiver<Result<serde_json::Value, RpcError>>> {
        let (tx, rx) = async_channel::bounded(1);
        match self.pending.lock().entry(id) {
            std::collections::hash_map::Entry::Occupied(_) => None,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(tx);
                Some(rx)
            }
        }
    }

    /// Resolve the waiter for `id`. Returns `false` when no waiter exists
    /// (already cancelled or expired).
    pub fn complete(&self, id: &MsgId, outcome: Result<serde_json::Value, RpcError>) -> bool {
        match self.pending.lock().remove(id) {
            Some(tx) => {
                let _ = tx.send_blocking(outcome);
                true
            }
            None => false,
        }
    }

    /// Cancel the waiter for `id`, resolving it with `err`.
    pub fn cancel(&self, id: &MsgId, err: RpcError) -> bool {
        self.complete(id, Err(err))
    }

    /// Drop all waiters, resolving each with `err` (peer disconnect).
    pub fn cancel_all(&self, err: RpcError) {
        let ids: Vec<MsgId> = self.pending.lock().keys().cloned().collect();
        for id in ids {
            self.cancel(&id, err.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Backpressure overflow, deterministic and single-threaded: a full
    /// buffer silently drops a Drop-policy note and reliably delivers a
    /// Disconnect-policy one once space frees. "send_blocking blocks" is
    /// async_channel's own guarantee and is not under test here.
    #[test]
    fn backpressure_drop_policy_silently_drops_when_full() {
        let (client, server) = in_process_pair_with_capacity(1);
        // Fill the single buffer slot with a Disconnect-policy note —
        // send_blocking succeeds immediately while space exists.
        server.send_to_client(FromServer::Notification {
            note: ServerNote::SessionCreated {
                session_id: "s1".into(),
            },
        });
        // The buffer is full: a streaming side-note is dropped by policy, not
        // queued — the connection stays up.
        server.send_to_client(FromServer::Notification {
            note: ServerNote::ModelThinking {
                request_id: "r1".into(),
                text: "delta".into(),
            },
        });
        let rx = client.server_rx();
        assert!(matches!(
            rx.recv_blocking().unwrap(),
            FromServer::Notification {
                note: ServerNote::SessionCreated { .. }
            }
        ));
        // The dropped note never occupied a slot: the next receive is empty
        // without waiting (try_recv, not a timeout).
        assert!(rx.try_recv().is_err());
        // Space freed: a further control note is delivered in order.
        server.send_to_client(FromServer::Notification {
            note: ServerNote::ModelToolCall {
                request_id: "r1".into(),
                id: "tc1".into(),
                name: "Bash".into(),
                input: serde_json::json!({}),
            },
        });
        assert!(matches!(
            rx.recv_blocking().unwrap(),
            FromServer::Notification {
                note: ServerNote::ModelToolCall { .. }
            }
        ));
    }

    #[test]
    fn in_process_pair_delivers_both_directions() {
        let (client, server) = in_process_pair();
        client.send_to_server(FromClient::Reply {
            id: MsgId::new("1"),
            outcome: Ok(serde_json::json!(null)),
        });
        let got = server.client_rx().recv_blocking().unwrap();
        assert!(matches!(got, FromClient::Reply { .. }));

        server.send_to_client(FromServer::Notification {
            note: ServerNote::Ready,
        });
        let got = client.server_rx().recv_blocking().unwrap();
        assert!(matches!(
            got,
            FromServer::Notification {
                note: ServerNote::Ready
            }
        ));
    }

    /// Round-10 regression: the production pair is unbounded, so a burst on
    /// one (or both) directions never parks a sender — the historical bounded
    /// pair deadlocked the gpui main thread mid-submit once both directions
    /// filled (each side parked in `send_blocking` waiting for the other's
    /// consumer). Flood far past the old [`BACKPRESSURE_CAPACITY`] with NO
    /// consumer attached, then drain and count.
    #[test]
    fn in_process_pair_never_blocks_on_bidirectional_flood() {
        let (client, server) = in_process_pair();
        let n = BACKPRESSURE_CAPACITY * 3;
        // Both directions flood with no consumer: every send must return
        // immediately (a bounded pair would park here and the test would
        // hang, failing by timeout).
        for i in 0..n {
            client.send_to_server(FromClient::Reply {
                id: MsgId::new(format!("c-{i}")),
                outcome: Ok(serde_json::json!(null)),
            });
            server.send_to_client(FromServer::Notification {
                note: ServerNote::SessionCreated {
                    session_id: format!("s-{i}"),
                },
            });
        }
        let c2s = server.client_rx();
        let s2c = client.server_rx();
        for i in 0..n {
            assert!(matches!(
                c2s.recv_blocking().unwrap(),
                FromClient::Reply { id, .. } if id.0 == format!("c-{i}")
            ));
            assert!(matches!(
                s2c.recv_blocking().unwrap(),
                FromServer::Notification {
                    note: ServerNote::SessionCreated { session_id }
                } if session_id == format!("s-{i}")
            ));
        }
    }

    #[test]
    fn streaming_note_drops_when_full_control_disconnects() {
        assert_eq!(
            ServerNote::ModelText {
                request_id: "r".into(),
                text: "x".into()
            }
            .backpressure_policy(),
            BackpressurePolicy::Drop
        );
        assert_eq!(
            ServerNote::SessionCreated {
                session_id: "t".into()
            }
            .backpressure_policy(),
            BackpressurePolicy::Disconnect
        );
    }

    #[test]
    fn disconnect_closes_channels() {
        let (client, _server) = in_process_pair();
        let rx = client.server_rx();
        client.disconnect();
        assert!(rx.recv_blocking().is_err());
    }

    #[test]
    fn rpc_peer_register_complete_resolves() {
        let peer = RpcPeer::new();
        let rx = peer
            .register(MsgId::new("c-1"))
            .expect("fresh id registers");
        assert!(peer.complete(&MsgId::new("c-1"), Ok(serde_json::json!({"ok": true}))));
        let outcome = rx.recv_blocking().unwrap();
        assert_eq!(outcome.unwrap(), serde_json::json!({"ok": true}));
        // Completing again finds no waiter.
        assert!(!peer.complete(&MsgId::new("c-1"), Ok(serde_json::json!(null))));
    }

    /// GW2 regression: a duplicate `register` of the same MsgId must NOT
    /// clobber the first waiter. Pre-fix the map insert dropped the earlier
    /// sender, closing its receiver immediately — a double-routed
    /// ServerCall then read the closed receiver as a fail-closed rejection
    /// and auto-denied before the user answered. The refused second
    /// registration answers `None` and the first waiter still resolves on
    /// `complete`.
    #[test]
    fn rpc_peer_duplicate_register_keeps_the_first_waiter_alive() {
        let peer = RpcPeer::new();
        let first = peer
            .register(MsgId::new("dup"))
            .expect("fresh id registers");
        assert!(
            peer.register(MsgId::new("dup")).is_none(),
            "a duplicate MsgId registration must be refused, not clobber"
        );
        // The first waiter survives the refused duplicate: it is still open
        // (pre-fix its sender was dropped here, so recv failed closed).
        assert!(
            first.is_empty() && !first.is_closed(),
            "the first waiter's receiver must stay open after a refused duplicate"
        );
        assert!(peer.complete(&MsgId::new("dup"), Ok(serde_json::json!({"ok": 1}))));
        assert_eq!(
            first.recv_blocking().unwrap().unwrap(),
            serde_json::json!({"ok": 1})
        );
    }

    #[test]
    fn rpc_peer_cancel_resolves_with_error() {
        let peer = RpcPeer::new();
        let rx = peer
            .register(MsgId::new("c-2"))
            .expect("fresh id registers");
        assert!(peer.cancel(&MsgId::new("c-2"), RpcError::new(-1, "gone")));
        let outcome = rx.recv_blocking().unwrap();
        assert_eq!(outcome.unwrap_err().message, "gone");
    }

    #[test]
    fn rpc_peer_cancel_all_resolves_every_waiter() {
        let peer = RpcPeer::new();
        let r1 = peer.register(MsgId::new("a")).expect("fresh id registers");
        let r2 = peer.register(MsgId::new("b")).expect("fresh id registers");
        peer.cancel_all(RpcError::new(-2, "disconnect"));
        assert_eq!(r1.recv_blocking().unwrap().unwrap_err().code, -2);
        assert_eq!(r2.recv_blocking().unwrap().unwrap_err().code, -2);
    }
}
