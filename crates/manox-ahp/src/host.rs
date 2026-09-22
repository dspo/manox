//! The host: one per process, owning the `serverSeq` domain, the channel store,
//! the live connections and host → client requests.
//!
//! Publishing is synchronous on purpose. The runtime appends a journal entry on
//! its own thread and must be able to turn it into an action envelope without
//! an async hop: [`Host::publish`] stamps the sequence, reduces the action into
//! the host's authoritative state and queues the envelope to every subscriber.
//! Nothing in that path awaits, so a stalled client can never stall the runtime.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ahp_types::actions::{ActionEnvelope, ActionOrigin, StateAction};
use ahp_types::common::Uri;
use ahp_types::messages::JsonRpcMessage;
use ahp_types::notifications::{
    PartialSessionSummary, SessionAddedParams, SessionRemovedParams, SessionSummaryChangedParams,
};
use ahp_types::state::{RootState, SessionSummary, Snapshot, SnapshotState};
use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use tokio::sync::oneshot;

use crate::backend::Backend;
use crate::channels::{Channel, ChannelStore, parse, root};
use crate::connection::Conn;
use crate::error::HostError;
use crate::sequencer::Sequencer;
use crate::transport::HostTransport;
use crate::wire;

/// The manox AHP host.
#[derive(Clone)]
pub struct Host {
    inner: Arc<Inner>,
}

/// Host state shared by every connection.
pub(crate) struct Inner {
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) seq: Sequencer,
    pub(crate) store: RwLock<ChannelStore>,
    pub(crate) conns: RwLock<Vec<Arc<Conn>>>,
    pub(crate) pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, HostError>>>>,
    next_request: AtomicU64,
    next_conn: AtomicU64,
}

impl Host {
    /// A host serving `backend`, with the root channel seeded from it.
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        let root: RootState = backend.root_state();
        Self {
            inner: Arc::new(Inner {
                backend,
                seq: Sequencer::default(),
                store: RwLock::new(ChannelStore::new(root)),
                conns: RwLock::new(Vec::new()),
                pending: Mutex::new(HashMap::new()),
                next_request: AtomicU64::new(0),
                next_conn: AtomicU64::new(0),
            }),
        }
    }

    /// The runtime behind this host.
    pub fn backend(&self) -> &Arc<dyn Backend> {
        &self.inner.backend
    }

    /// The highest stamped `serverSeq`.
    pub fn server_seq(&self) -> i64 {
        self.inner.seq.watermark()
    }

    /// Number of live connections.
    pub fn connection_count(&self) -> usize {
        self.inner.conns.read().len()
    }

    /// Accept a transport: one reader task and one writer task, until the peer
    /// closes.
    pub fn accept(&self, transport: HostTransport) {
        let (tx, rx) = async_channel::unbounded::<JsonRpcMessage>();
        let conn = Arc::new(Conn::new(
            self.inner.next_conn.fetch_add(1, Ordering::SeqCst) + 1,
            tx,
        ));
        self.inner.conns.write().push(conn.clone());

        let (mut sink, mut source) = transport.split();
        let writer_conn = conn.clone();
        tokio::spawn(async move {
            while let Ok(msg) = rx.recv().await {
                if sink.send(msg).await.is_err() {
                    break;
                }
            }
            writer_conn.kill();
        });

        let inner = self.inner.clone();
        tokio::spawn(async move {
            loop {
                match source.recv().await {
                    Ok(Some(msg)) => crate::router::handle(&inner, &conn, msg).await,
                    Ok(None) => break,
                    Err(err) => {
                        tracing::debug!(connection = conn.id(), "transport closed: {err}");
                        break;
                    }
                }
            }
            conn.kill();
            inner.conns.write().retain(|live| live.id() != conn.id());
        });
    }

    /// Publish one host-originated action (see [`Inner::publish`]).
    pub fn publish(
        &self,
        uri: &str,
        action: StateAction,
        origin: Option<ActionOrigin>,
    ) -> ActionEnvelope {
        self.inner.publish(uri, action, origin)
    }

    /// `root/sessionAdded` to root subscribers.
    pub fn session_added(&self, summary: SessionSummary) {
        self.inner.session_added(summary);
    }

    /// `root/sessionRemoved` to root subscribers.
    pub fn session_removed(&self, session_id: &str) {
        self.inner.session_removed(session_id);
    }

    /// `root/sessionSummaryChanged` to root subscribers.
    pub fn summary_changed(&self, session_id: &str, changes: PartialSessionSummary) {
        self.inner.summary_changed(session_id, changes);
    }

    /// Ask one client to do something (AHP allows host-initiated requests; the
    /// `resource*` family is the standard precedent, `x-manox/*` our extension).
    ///
    /// Fail-closed: a dropped or unanswered request is an error, never a silent
    /// success.
    pub async fn request(
        &self,
        conn: &Arc<Conn>,
        method: &str,
        params: Value,
    ) -> Result<Value, HostError> {
        let id = self.inner.next_request.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().insert(id, tx);
        conn.send(wire::request(id, method, params));
        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(err),
            Err(_) => {
                self.inner.pending.lock().remove(&id);
                Err(HostError::Backend(format!(
                    "client did not answer {method}"
                )))
            }
        }
    }
}

impl Inner {
    /// Connections observing `uri`.
    pub(crate) fn subscribers(&self, uri: &str) -> Vec<Arc<Conn>> {
        self.conns
            .read()
            .iter()
            .filter(|conn| conn.alive() && conn.is_subscribed(uri))
            .cloned()
            .collect()
    }

    /// Queue one envelope to every subscriber of its channel.
    pub(crate) fn broadcast(&self, envelope: &ActionEnvelope) {
        let msg = wire::action_notification(envelope.clone());
        for conn in self.subscribers(&envelope.channel) {
            conn.send(msg.clone());
        }
    }

    /// The live connection carrying `client_id`.
    pub(crate) fn connection_for_client(&self, client_id: &str) -> Option<Arc<Conn>> {
        self.conns
            .read()
            .iter()
            .find(|conn| conn.alive() && conn.client_id().as_deref() == Some(client_id))
            .cloned()
    }

    /// Re-seat a client id onto `conn`: a reconnecting client takes over its
    /// previous connection, which is dropped (its transport tasks end when its
    /// queue closes).
    pub(crate) fn reseat(&self, conn: &Arc<Conn>, client_id: &str) {
        if let Some(previous) = self.connection_for_client(client_id)
            && previous.id() != conn.id()
        {
            previous.kill();
            self.conns.write().retain(|live| live.id() != previous.id());
        }
    }

    /// Publish one host-originated action: stamp it, fold it into the host's
    /// state and broadcast it to the channel's subscribers.
    ///
    /// A rejected reduction is a host bug — the action came from our own
    /// translation — so it is logged loudly and still broadcast: clients reduce
    /// the same envelope, and the convergence gate turns a mismatch into a test
    /// failure rather than a silent divergence.
    pub(crate) fn publish(
        &self,
        uri: &str,
        action: StateAction,
        origin: Option<ActionOrigin>,
    ) -> ActionEnvelope {
        let envelope = self.stamp_and_fold(uri, action, origin);
        self.broadcast(&envelope);
        envelope
    }

    /// Stamp and fold without broadcasting (the dispatch path answers the
    /// originator through the broadcast of the accepted envelope).
    pub(crate) fn stamp_and_fold(
        &self,
        uri: &str,
        action: StateAction,
        origin: Option<ActionOrigin>,
    ) -> ActionEnvelope {
        let server_seq = self.seq.stamp() as u64;
        if let Some(channel) = parse(uri) {
            let outcome = self.store.write().apply(&channel, &action);
            if !matches!(outcome, ahp::reducers::ReduceOutcome::Applied) {
                tracing::warn!(
                    channel = uri,
                    action = %wire::action_tag(&action),
                    "host action reduced to {outcome:?}"
                );
            }
        } else {
            tracing::warn!(channel = uri, "host action on an unknown channel scheme");
        }
        ActionEnvelope {
            channel: uri.to_string(),
            action,
            server_seq,
            origin,
            rejection_reason: None,
        }
    }

    /// Echo a refused client action back to everyone observing the channel, so
    /// the originator learns its write did not land (AHP's rejection contract).
    pub(crate) fn reject(
        &self,
        uri: &str,
        action: StateAction,
        origin: Option<ActionOrigin>,
        reason: String,
    ) -> ActionEnvelope {
        let server_seq = self.seq.stamp() as u64;
        let envelope = ActionEnvelope {
            channel: uri.to_string(),
            action,
            server_seq,
            origin,
            rejection_reason: Some(reason),
        };
        self.broadcast(&envelope);
        envelope
    }

    /// The highest stamped `serverSeq`.
    pub(crate) fn watermark(&self) -> i64 {
        self.seq.watermark()
    }

    /// A snapshot for `channel`, or `None` for stateless channels.
    pub(crate) fn snapshot(&self, channel: &Channel) -> Option<Snapshot> {
        let state: SnapshotState = self.store.read().snapshot(channel)?;
        Some(Snapshot {
            resource: channel.uri(),
            state,
            from_seq: self.seq.watermark(),
        })
    }

    /// Send one protocol notification to a channel's subscribers.
    pub(crate) fn notify(&self, uri: &str, method: &str, params: Value) {
        let msg = wire::notification(method, params);
        for conn in self.subscribers(uri) {
            conn.send(msg.clone());
        }
    }

    /// Send one protocol notification to root subscribers.
    pub(crate) fn notify_root(&self, method: &str, params: Value) {
        self.notify(root::URI, method, params);
    }

    /// `root/sessionAdded` to root subscribers.
    pub(crate) fn session_added(&self, summary: SessionSummary) {
        let params = SessionAddedParams {
            channel: root::URI.to_string(),
            summary,
        };
        self.notify_root(
            "root/sessionAdded",
            serde_json::to_value(params).unwrap_or(Value::Null),
        );
    }

    /// `root/sessionRemoved` to root subscribers.
    pub(crate) fn session_removed(&self, session_id: &str) {
        let params = SessionRemovedParams {
            channel: root::URI.to_string(),
            session: crate::channels::session::uri(session_id),
        };
        self.notify_root(
            "root/sessionRemoved",
            serde_json::to_value(params).unwrap_or(Value::Null),
        );
    }

    /// `root/sessionSummaryChanged` to root subscribers — the session list
    /// stays in sync without every client subscribing to every session.
    pub(crate) fn summary_changed(&self, session_id: &str, changes: PartialSessionSummary) {
        let params = SessionSummaryChangedParams {
            channel: root::URI.to_string(),
            session: crate::channels::session::uri(session_id),
            changes,
        };
        self.notify_root(
            "root/sessionSummaryChanged",
            serde_json::to_value(params).unwrap_or(Value::Null),
        );
    }

    /// Resolve a pending host → client request.
    pub(crate) fn resolve_request(&self, id: u64, outcome: Result<Value, HostError>) {
        if let Some(tx) = self.pending.lock().remove(&id) {
            let _ = tx.send(outcome);
        }
    }

    /// Make sure a session's state is loaded, seeding from the backend.
    pub(crate) fn ensure_session(&self, session_id: &str) -> Result<(), HostError> {
        if self.store.read().session(session_id).is_some() {
            return Ok(());
        }
        let state = self
            .backend
            .session_state(session_id)
            .ok_or_else(|| HostError::SessionNotFound(session_id.to_string()))?;
        let chats: Vec<Uri> = state
            .chats
            .iter()
            .map(|chat| chat.resource.clone())
            .collect();
        self.store.write().insert_session(session_id, state);
        for uri in chats {
            if let Some(chat_id) = crate::channels::chat::id(&uri)
                && let Some(chat) = self.backend.chat_state(chat_id)
            {
                self.store.write().insert_chat(session_id, chat_id, chat);
            }
        }
        Ok(())
    }

    /// Make sure a chat's state and its session link are loaded.
    pub(crate) fn ensure_chat(&self, chat_id: &str) -> Result<String, HostError> {
        if let Some(owner) = self.store.read().chat_session(chat_id).map(str::to_string) {
            return Ok(owner);
        }
        let session_id = self
            .backend
            .session_state_for_chat(chat_id)
            .ok_or_else(|| HostError::NotFound(crate::channels::chat::uri(chat_id)))?;
        self.ensure_session(&session_id)?;
        if self.store.read().chat(chat_id).is_none() {
            let state = self
                .backend
                .chat_state(chat_id)
                .ok_or_else(|| HostError::NotFound(crate::channels::chat::uri(chat_id)))?;
            self.store.write().insert_chat(&session_id, chat_id, state);
        }
        Ok(session_id)
    }

    /// Make sure a terminal's state is loaded.
    pub(crate) fn ensure_terminal(&self, terminal_id: &str) -> Result<(), HostError> {
        if self.store.read().terminal(terminal_id).is_some() {
            return Ok(());
        }
        let state = self
            .backend
            .terminal_state(terminal_id)
            .ok_or_else(|| HostError::NotFound(crate::channels::terminal::uri(terminal_id)))?;
        self.store.write().insert_terminal(terminal_id, state);
        Ok(())
    }
}
