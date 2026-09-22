//! One client connection.
//!
//! A connection owns its subscription set, its client identity and an unbounded
//! outbound queue that a dedicated writer task drains into the transport. The
//! queue is unbounded on purpose: the host publishes from runtime threads that
//! must never block, and per the v2 C6 ruling the in-process carrier must not
//! deadlock. WebSocket backpressure is handled at the transport's own queue and
//! by reconnect-with-snapshot (W5 adds `delivery.maxLatencyMs` coalescing).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use ahp_types::messages::JsonRpcMessage;
use parking_lot::Mutex;

/// A live AHP connection.
pub struct Conn {
    id: u64,
    client_id: Mutex<Option<String>>,
    locale: Mutex<Option<String>>,
    subscriptions: Mutex<HashSet<String>>,
    out: async_channel::Sender<JsonRpcMessage>,
    alive: AtomicBool,
}

impl Conn {
    /// A connection writing into `out`.
    pub(crate) fn new(id: u64, out: async_channel::Sender<JsonRpcMessage>) -> Self {
        Self {
            id,
            client_id: Mutex::new(None),
            locale: Mutex::new(None),
            subscriptions: Mutex::new(HashSet::new()),
            out,
            alive: AtomicBool::new(true),
        }
    }

    /// The process-local connection number (logging only).
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The client-provided identity, once `initialize`/`reconnect` named one.
    ///
    /// AHP uses it for reconnection; the host uses it to re-seat a reconnected
    /// client onto the same logical session ownership.
    pub fn client_id(&self) -> Option<String> {
        self.client_id.lock().clone()
    }

    /// The client's preferred locale (advisory; the runtime never localises).
    pub fn locale(&self) -> Option<String> {
        self.locale.lock().clone()
    }

    pub(crate) fn set_identity(&self, client_id: String, locale: Option<String>) {
        *self.client_id.lock() = Some(client_id);
        if locale.is_some() {
            *self.locale.lock() = locale;
        }
    }

    /// URIs this connection currently observes.
    pub fn subscriptions(&self) -> Vec<String> {
        self.subscriptions.lock().iter().cloned().collect()
    }

    /// Register a subscription; `false` when it was already present.
    pub(crate) fn subscribe(&self, uri: &str) -> bool {
        self.subscriptions.lock().insert(uri.to_string())
    }

    /// Release a subscription.
    pub(crate) fn unsubscribe(&self, uri: &str) {
        self.subscriptions.lock().remove(uri);
    }

    /// Whether this connection observes `uri`.
    pub(crate) fn is_subscribed(&self, uri: &str) -> bool {
        self.subscriptions.lock().contains(uri)
    }

    /// Queue one outbound message. A closed queue marks the connection dead
    /// (its writer task is gone), never blocks.
    pub(crate) fn send(&self, msg: JsonRpcMessage) {
        if self.out.try_send(msg).is_err() {
            self.alive.store(false, Ordering::SeqCst);
        }
    }

    /// Whether the connection can still be written to.
    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst) && !self.out.is_closed()
    }

    /// Mark the connection dead (its writer task ended).
    pub(crate) fn kill(&self) {
        self.alive.store(false, Ordering::SeqCst);
        self.out.close();
    }
}
