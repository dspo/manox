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

use ahp_types::messages::{JsonRpcError, JsonRpcMessage};
use parking_lot::Mutex;

use crate::jsonrpc::{MsgId, RpcPeer};

/// Per-connection rewriting of the wire, applied on both directions.
///
/// Some clients speak a dialect of the spec: they compute their own session URIs
/// from the provider, build derived chat URIs, send a command against a different
/// channel than the spec says, or omit `params.channel` on `reconnect`. Those
/// adaptations belong here — at the boundary of the connection that has the
/// dialect — never in the host, because publishing one client's dialect would
/// impose it on every other client.
///
/// The default is the spec, verbatim: [`IdentityDialect`].
pub trait Dialect: Send + Sync + 'static {
    /// Rewrite one outbound message (host → client).
    fn outgoing(&self, message: JsonRpcMessage) -> JsonRpcMessage {
        message
    }

    /// Rewrite one inbound message (client → host).
    fn incoming(&self, message: JsonRpcMessage) -> JsonRpcMessage {
        message
    }
}

/// The spec, verbatim.
pub struct IdentityDialect;

impl Dialect for IdentityDialect {}

/// A live AHP connection.
pub struct Conn {
    id: u64,
    client_id: Mutex<Option<String>>,
    locale: Mutex<Option<String>>,
    subscriptions: Mutex<HashSet<String>>,
    out: async_channel::Sender<JsonRpcMessage>,
    alive: AtomicBool,
    dialect: Box<dyn Dialect>,
    /// Waiters for the host → client requests this connection carries.
    ///
    /// Per connection, not per host: a request targets one client, and when that
    /// client goes away its waiters must be retired (the re-seat case the v2 peer
    /// learned: a closed receiver the caller folds into a fail-closed rejection
    /// is worse than an explicit cancellation).
    pending: RpcPeer,
    /// Host → client request methods this client declared it can answer.
    ///
    /// Read from the client's `initialize`/`reconnect` `_meta` (see
    /// `router::declared_client_requests`), because AHP's `ClientCapabilities`
    /// declares `mcpApps` alone and has no field for a host-initiated request.
    /// Empty means the client declared nothing, which selects it for nothing.
    client_requests: Mutex<Vec<String>>,
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
            dialect: Box::new(IdentityDialect),
            pending: RpcPeer::new(),
            client_requests: Mutex::new(Vec::new()),
        }
    }

    /// Record the host → client request methods this connection declared.
    pub fn set_client_requests(&self, methods: Vec<String>) {
        *self.client_requests.lock() = methods;
    }

    /// Whether this client declared it can answer `method`.
    ///
    /// The gate for host-initiated requests: a client that did not declare a
    /// method is never asked, so an unanswerable request fails at selection
    /// rather than after a 300-second deadline.
    pub fn can_answer(&self, method: &str) -> bool {
        self.client_requests
            .lock()
            .iter()
            .any(|declared| declared == method)
    }

    /// Install the dialect this connection speaks (see [`Dialect`]).
    pub fn set_dialect(&mut self, dialect: Box<dyn Dialect>) {
        self.dialect = dialect;
    }

    /// Register the waiter for a host → client request (see [`RpcPeer::register`]).
    pub(crate) fn register_waiter(
        &self,
        id: MsgId,
    ) -> Option<async_channel::Receiver<Result<serde_json::Value, JsonRpcError>>> {
        self.pending.register(id)
    }

    /// Resolve a host → client request with the client's answer.
    pub(crate) fn complete_waiter(
        &self,
        id: MsgId,
        outcome: Result<serde_json::Value, JsonRpcError>,
    ) -> bool {
        self.pending.complete(id, outcome)
    }

    /// Retire every outstanding waiter of this connection with `error`.
    pub(crate) fn cancel_waiters(&self, error: JsonRpcError) {
        self.pending.cancel_all(error);
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
        let msg = self.dialect.outgoing(msg);
        if self.out.try_send(msg).is_err() {
            self.alive.store(false, Ordering::SeqCst);
        }
    }

    /// Rewrite one inbound message into what the host expects.
    pub(crate) fn interpret(&self, msg: JsonRpcMessage) -> JsonRpcMessage {
        self.dialect.incoming(msg)
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
