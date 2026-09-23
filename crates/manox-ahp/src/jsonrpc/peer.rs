//! Correlation of outstanding requests and calls for one peer.
//!
//! The issuer registers a waiter for a fresh [`MsgId`] *before* sending, then
//! awaits the receiver under its own timeout; the responder calls
//! [`RpcPeer::complete`] when the matching reply arrives. Clones share one waiter
//! map, so a connection, its re-seat and its pump all coordinate through the same
//! handle.

use std::collections::HashMap;
use std::sync::Arc;

use ahp_types::messages::JsonRpcError;
use async_channel::{Receiver, Sender};
use parking_lot::Mutex;

use super::MsgId;

/// Waiter map: outstanding request/response and call/reply pairs.
type PendingMap = HashMap<MsgId, Sender<Result<serde_json::Value, JsonRpcError>>>;

/// Correlates outstanding request/response and call/reply pairs for one peer.
#[derive(Clone, Default)]
pub struct RpcPeer {
    pending: Arc<Mutex<PendingMap>>,
}

impl RpcPeer {
    /// A peer with an empty waiter map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a waiter for `id`; returns the receiver it resolves on.
    ///
    /// A duplicate registration of the same id is refused (`None`) and the
    /// **first** waiter stays registered. Replacing it would drop the earlier
    /// sender, closing that receiver immediately — and callers fold a closed
    /// receiver into a fail-closed rejection, which would auto-deny an approval
    /// before the user ever saw it. Callers must treat `None` as a routing bug
    /// and skip the delivery fail-closed.
    pub fn register(&self, id: MsgId) -> Option<Receiver<Result<serde_json::Value, JsonRpcError>>> {
        let (tx, rx) = async_channel::bounded(1);
        match self.pending.lock().entry(id) {
            std::collections::hash_map::Entry::Occupied(_) => None,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(tx);
                Some(rx)
            }
        }
    }

    /// Resolve the waiter for `id`; `false` when none was registered (already
    /// completed, cancelled or expired).
    pub fn complete(&self, id: MsgId, outcome: Result<serde_json::Value, JsonRpcError>) -> bool {
        match self.pending.lock().remove(&id) {
            Some(tx) => {
                let _ = tx.send_blocking(outcome);
                true
            }
            None => false,
        }
    }

    /// Cancel the waiter for `id`, resolving it with `error`.
    pub fn cancel(&self, id: MsgId, error: JsonRpcError) -> bool {
        self.complete(id, Err(error))
    }

    /// Whether a live waiter is registered for `id`.
    ///
    /// This is what tells "the owner's connection still carries the open waiter"
    /// (the reply will flow through it) from a re-seated owner whose waiter died
    /// with the old connection (re-mint the waiter on the new peer).
    pub fn has_waiter(&self, id: MsgId) -> bool {
        self.pending.lock().contains_key(&id)
    }

    /// Drop every waiter, resolving each with `error`.
    ///
    /// The gateway calls this on a same-client-id hand-shake swap, tagging the
    /// error so waiters can tell a connection swap from a delivery failure.
    pub fn cancel_all(&self, error: JsonRpcError) {
        let ids: Vec<MsgId> = self.pending.lock().keys().copied().collect();
        for id in ids {
            self.cancel(id, error.clone());
        }
    }

    /// Number of waiters currently outstanding (diagnostics and tests).
    pub fn outstanding(&self) -> usize {
        self.pending.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(code: i32) -> JsonRpcError {
        JsonRpcError {
            code,
            message: format!("code {code}"),
            data: None,
        }
    }

    /// The regression this type exists for: a second registration for the same
    /// id must not clobber the first waiter (which would close its receiver and
    /// make the caller fail-closed).
    #[test]
    fn duplicate_registration_keeps_the_first_waiter() {
        let peer = RpcPeer::new();
        let first = peer.register(MsgId(7)).expect("first registration wins");
        assert!(
            peer.register(MsgId(7)).is_none(),
            "the duplicate is refused"
        );
        assert!(peer.has_waiter(MsgId(7)));
        assert!(peer.complete(MsgId(7), Ok(serde_json::json!({"ok": true}))));
        assert_eq!(
            first.recv_blocking().expect("the first waiter resolves"),
            Ok(serde_json::json!({"ok": true}))
        );
        assert!(!peer.has_waiter(MsgId(7)), "completion retires the waiter");
    }

    #[test]
    fn cancel_resolves_only_an_existing_waiter() {
        let peer = RpcPeer::new();
        let waiter = peer.register(MsgId(1)).expect("registered");
        assert!(peer.cancel(MsgId(1), error(-32000)));
        assert_eq!(
            waiter.recv_blocking().expect("resolved"),
            Err(error(-32000))
        );
        assert!(
            !peer.cancel(MsgId(1), error(-32000)),
            "cancelling twice reports the second as a miss"
        );
    }

    #[test]
    fn cancel_all_retires_every_waiter() {
        let peer = RpcPeer::new();
        let a = peer.register(MsgId(1)).expect("a");
        let b = peer.register(MsgId(2)).expect("b");
        peer.cancel_all(error(-32001));
        assert_eq!(peer.outstanding(), 0);
        assert_eq!(a.recv_blocking().expect("a resolved"), Err(error(-32001)));
        assert_eq!(b.recv_blocking().expect("b resolved"), Err(error(-32001)));
    }
}
