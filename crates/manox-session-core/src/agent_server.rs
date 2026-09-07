//! AgentServer — the single protocol gateway.
//!
//! The only public surface between frontends and the gpui-free kernel: every
//! client (gpui desktop, WebUI, future VS Code) speaks [`manox_protocol`] over
//! an [`RpcConnection`], and the server drives kernel [`ThreadHandle`]s from
//! those messages. Kernel [`ThreadEvent`]s are projected through
//! [`crate::translate`] into [`ServerNote`] (streamed to the owning client) or
//! [`ServerCall`] (a round-trip the owning ∩ capable client must answer), so
//! the kernel stays free of transport and frontend concerns.
//!
//! Scope: connection/handshake, session ownership, the full
//! `ClientCall`/`ClientNote` dispatch, the `Note` event pump, and the
//! event-driven `ServerCall` round-trips — `Approve` (β-3a) plus
//! `AskUserQuestion` and `PlanVerdict` (β-3b-i, the latter pump-initiated
//! on PlanReady). `CapabilityClient` rewiring (BrowserOp/ClipboardRead/
//! OpenExternal), terminal, and model_chat are β-3b-ii.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use manox_protocol::base64_bytes;
use manox_protocol::client::ImageAttachment;
use manox_protocol::handshake::{ClientHello, HookKind, Initialize, PROTOCOL_EPOCH};
use manox_protocol::journal::StreamId;
use manox_protocol::stream::{HostEvent, StreamEndReason, StreamKind};
use manox_protocol::{
    ClientCall, ClientNote, FromClient, FromServer, ModelInfo, MsgId, RpcConnection, RpcError,
    RpcPeer, ServerCall, ServerNote, ThreadListItem,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

use manox_agent::language_model::{MessageContent, ReasoningEffort};
use manox_agent::thread::{PermissionMode, ThreadHandle};
use manox_agent::thread_engine::BackendNotice;
use manox_agent::{MessageUiMetadata, Thread, ThreadEvent, ThreadId};

use crate::follow::{self, StreamHandle};
use crate::journal_query;
use crate::translate::{Translated, translate};

/// How long the server waits for a client to answer a `ServerCall` before
/// treating it as fail-closed. Generous: a human reviewing a plan or an
/// approval may take minutes. The kernel never sets its own timeout — that
/// would duplicate the peer's correlation/timeout machinery.
const CALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// One live session: the strong `ThreadHandle` (the retention owner) and its
/// event pump. Dropping the `JoinHandle` alone only DETACHES the pump — it
/// keeps running (its own `ThreadHandle` clone keeps the subscription alive),
/// so every removal path must call [`ServerSession::stop_pump`] before the
/// entry leaves the table; the `Drop` impl is the safety net that makes the
/// guarantee structural (GW2).
struct ServerSession {
    thread: ThreadHandle,
    /// Cancellation token for the pump loop's `tokio::select!` (the
    /// [`crate::follow::StreamHandle`] pattern): cancel wakes a pump parked
    /// in `rx.recv()`.
    pump_cancel: tokio_util::sync::CancellationToken,
    /// The pump task. Aborted alongside the token in [`Self::stop_pump`] —
    /// the abort covers a pump parked inside a long `route_call` await that
    /// never re-enters the select.
    pump: tokio::task::JoinHandle<()>,
    turn_active: Arc<AtomicBool>,
    pending_submits: Arc<StdMutex<Vec<QueuedSubmit>>>,
}

impl ServerSession {
    /// Terminate this session's pump (GW2): cancel the token and abort the
    /// task (double insurance — either alone leaves a window). Idempotent;
    /// runs before the entry is dropped so a concurrent reopen can never
    /// observe a live session with a dead table entry, and a replaced entry
    /// can never leave a second pump subscribed to the same thread.
    fn stop_pump(&self) {
        self.pump_cancel.cancel();
        self.pump.abort();
    }
}

impl Drop for ServerSession {
    /// Safety net: a session entry leaving the table by ANY path (explicit
    /// removal, map replacement, whole-server drop) takes its pump with it.
    /// Without this, `JoinHandle` drop merely detached the pump: its
    /// `ThreadHandle` clone kept `thread.subscribe()`'s unbounded channel
    /// open, so `rx.recv()` never closed and the pump plus the engine actor
    /// leaked process-wide (GW2).
    fn drop(&mut self) {
        self.stop_pump();
    }
}

/// Bumps the server's finished-pump counter when the pump task exits — by
/// token cancellation, subscription close, or `JoinHandle::abort` (the abort
/// drops the task future, running this guard's `Drop`). Paired with the
/// spawn counter it makes the live pump count observable for the GW2
/// double-pump regressions.
struct PumpExitGuard(Arc<AgentServerInner>);

impl Drop for PumpExitGuard {
    fn drop(&mut self) {
        self.0.pumps_finished.fetch_add(1, Ordering::SeqCst);
    }
}

/// A submission parked while a turn runs; drained into one follow-up turn when
/// the turn settles, mirroring the legacy host's queued-follow-up behavior.
struct QueuedSubmit {
    client_id: String,
    text: String,
    images: Vec<(String, String)>,
    ui: MessageUiMetadata,
    /// The Submit's origin RPC id (echo retirement, §F.2). A drained batch
    /// merges into one turn, so the last non-None origin wins.
    origin: Option<String>,
}

/// One connected frontend.
struct ClientEntry {
    conn: Arc<dyn RpcConnection>,
    peer: RpcPeer,
    hello: ClientHello,
    /// Monotonically increasing generation assigned on each handshake. Used by
    /// `remove_client` to avoid deleting a newer entry that replaced this one.
    generation: u64,
}

/// The single gateway. Cloning shares the inner state.
pub struct AgentServer(Arc<AgentServerInner>);

struct AgentServerInner {
    cwd: PathBuf,
    sessions: Mutex<HashMap<String, ServerSession>>,
    clients: Mutex<HashMap<String, ClientEntry>>,
    /// session_id → client_ids that own (view) it. A session may have several
    /// owners; each receives its streamed notes.
    session_owners: Mutex<HashMap<String, Vec<String>>>,
    /// Live §D.1 streams: `(client_id, stream_id)` → control handle. The
    /// key pair mirrors the stream id's per-connection uniqueness (§D.1).
    streams: Mutex<HashMap<(String, StreamId), StreamHandle>>,
    call_seq: AtomicU64,
    /// GW3 (§D.4): per-session adjudication delivery counter — the `dlv-`
    /// id's monotonic suffix. Per-session (not per-server) so the two
    /// transports of `dual_path_transport_consistency` mint identical ids
    /// for identical scripts after session-id normalization.
    delivery_seq: Mutex<HashMap<String, u64>>,
    /// GW3 (§D.4): in-flight waterfall deliveries — `delivery_id` →
    /// (recipient client_id → cancel token). A `CancelDelivery` call flips
    /// the sender's token; the delivery's reply waiter folds that into the
    /// funnel as an expired reply, converging the waterfall fail-closed
    /// through the existing expire path. Registered for the fan-out window
    /// only (the [`DeliveryGuard`] removes the entry at settlement — Drop
    /// covers a pump abort too).
    pending_deliveries:
        Mutex<HashMap<String, HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// In-flight bare-model completions by request id (the LanguageModelChat
    /// provider path); cancellation tokens shared with the spawned streams.
    model_chats: Arc<StdMutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// Monotonically increasing counter for client entry generations, used to
    /// detect stale entries during same-client-id reconnection.
    next_generation: AtomicU64,
    /// §E.3 Q-face cache: `(thread_id, cursor)` → the folded conversation
    /// info payload (recomputed only when the cursor advances).
    conversation_info_cache: Arc<StdMutex<journal_query::ConversationInfoCache>>,
    /// GW2 pump observability: every `spawn_pump` bumps `pumps_spawned`;
    /// every pump exit — token cancel, subscription close, or task abort
    /// (the [`PumpExitGuard`]'s Drop runs in all three) — bumps
    /// `pumps_finished`. Their difference is the live pump count the
    /// double-pump regressions assert on.
    pumps_spawned: AtomicU64,
    pumps_finished: AtomicU64,
}

impl AgentServerInner {
    /// Live pump count (GW2 observability): spawned minus finished. A
    /// session's pump counts as finished once its task exits by token
    /// cancel, subscription close, or abort — the double-pump regressions
    /// poll this to a deadline instead of racing the runtime.
    #[cfg(test)]
    fn live_pumps(&self) -> u64 {
        self.pumps_spawned.load(Ordering::SeqCst) - self.pumps_finished.load(Ordering::SeqCst)
    }

    /// Install a session entry, terminating the pump of any entry it
    /// replaces (GW2: a replaced `ServerSession` must never leave its pump
    /// subscribed to the same thread beside the replacement's).
    fn insert_session(&self, session_id: String, session: ServerSession) {
        let replaced = { self.sessions.lock().insert(session_id, session) };
        if let Some(old) = replaced {
            tracing::warn!("replaced a live session entry; terminating the superseded pump");
            old.stop_pump();
        }
    }

    /// Register a live stream and return its control handle.
    fn track_stream(&self, client_id: &str, stream_id: &StreamId, handle: StreamHandle) {
        self.streams
            .lock()
            .insert((client_id.to_string(), stream_id.clone()), handle);
    }

    /// Forget a stream after its task sent the terminal `StreamEnd`
    /// (identity-guarded so a re-open with the same id is never deleted by
    /// the superseded task).
    fn untrack_stream(&self, client_id: &str, stream_id: &StreamId, handle: &StreamHandle) {
        let mut streams = self.streams.lock();
        let key = (client_id.to_string(), stream_id.clone());
        if streams
            .get(&key)
            .is_some_and(|live| live.is_same_handle(handle))
        {
            streams.remove(&key);
        }
    }

    /// End every live stream of a session with `reason` (dispose /
    /// ownership-lost: §D.1 `Closed`). Returns the ended handles' ids for
    /// logging.
    fn end_streams_for_session(&self, session_id: &str, reason: StreamEndReason) {
        let keys: Vec<(String, StreamId)> = self
            .streams
            .lock()
            .iter()
            .filter(|(_, h)| h.session_id() == session_id)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            let handle = self.streams.lock().remove(&key);
            if let Some(handle) = handle {
                handle.end(reason.clone());
            }
        }
    }

    /// End every stream owned by a disconnected client (§D.1 `Closed`).
    fn end_streams_for_client(&self, client_id: &str) {
        let keys: Vec<(String, StreamId)> = self
            .streams
            .lock()
            .keys()
            .filter(|(cid, _)| cid == client_id)
            .cloned()
            .collect();
        for key in keys {
            let handle = self.streams.lock().remove(&key);
            if let Some(handle) = handle {
                handle.end(StreamEndReason::Closed);
            }
        }
    }
}

/// The process-global server (L11: one `AgentServer` per process — the
/// desktop, the embedded web UI and every future frontend route through it,
/// so ownership/routing tables are shared). First caller wins; later cwd
/// arguments are ignored (a second window shares the first window's cwd).
pub fn global(cwd: std::path::PathBuf) -> std::sync::Arc<AgentServer> {
    static GLOBAL: std::sync::OnceLock<std::sync::Arc<AgentServer>> = std::sync::OnceLock::new();
    GLOBAL
        .get_or_init(|| std::sync::Arc::new(AgentServer::new(cwd)))
        .clone()
}

impl AgentServer {
    pub fn new(cwd: PathBuf) -> Self {
        Self(Arc::new(AgentServerInner {
            cwd,
            sessions: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            session_owners: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            call_seq: AtomicU64::new(0),
            delivery_seq: Mutex::new(HashMap::new()),
            pending_deliveries: Mutex::new(HashMap::new()),
            model_chats: Arc::new(StdMutex::new(HashMap::new())),
            next_generation: AtomicU64::new(1),
            conversation_info_cache: Arc::new(StdMutex::new(
                journal_query::ConversationInfoCache::default(),
            )),
            pumps_spawned: AtomicU64::new(0),
            pumps_finished: AtomicU64::new(0),
        }))
    }

    /// Accept a connection: spawn the handshake + dispatch task. The
    /// connection drives itself thereafter.
    pub fn accept(&self, conn: Arc<dyn RpcConnection>) {
        let inner = self.0.clone();
        manox_agent::runtime::handle().spawn(async move {
            inner.serve_connection(conn).await;
        });
    }

    /// Test-only: set a scripted engine on a session before any turn runs, so
    /// the event pump can be exercised without a live provider.
    #[cfg(test)]
    pub fn set_session_engine_for_test(
        &self,
        session_id: &str,
        engine: Arc<dyn manox_agent::thread_engine::ThreadEngine>,
        events: tokio::sync::mpsc::UnboundedReceiver<BackendNotice>,
    ) {
        if let Some(thread) = self.0.session_thread(session_id) {
            thread.with_mut(|t| t.set_engine_for_test(engine, events));
        }
    }
}

impl AgentServerInner {
    /// Drive one connection: handshake, then dispatch until disconnect.
    async fn serve_connection(self: Arc<Self>, conn: Arc<dyn RpcConnection>) {
        let rx = conn.client_rx();
        // ── Handshake: the first message must be Initialize. ─────────────
        let (client_id, generation) = match rx.recv().await {
            Ok(FromClient::Request {
                id,
                call:
                    ClientCall::Initialize(Initialize {
                        client_id,
                        capabilities,
                        sessions,
                        protocol_epoch,
                    }),
            }) => {
                if client_id.is_empty() {
                    conn.send_to_client(FromServer::Response {
                        id,
                        outcome: Err(RpcError::new(-1, "empty client_id")
                            .with_code(manox_protocol::msg::CODE_GATEWAY_BAD_REQUEST)),
                    });
                    return;
                }
                // C1 (L12 epoch negotiation): 0 is the pre-epoch v1
                // generation (the serde default of a missing field) and
                // stays accepted through the dual-protocol window;
                // PROTOCOL_EPOCH is the generation this server speaks. Any
                // other value is a generation whose frames this server would
                // misread — refuse at the handshake with the stable §D.7
                // code instead of interpreting future frames as current ones.
                if protocol_epoch != 0 && protocol_epoch != PROTOCOL_EPOCH {
                    conn.send_to_client(FromServer::Response {
                        id,
                        outcome: Err(RpcError::new(
                            -1,
                            format!(
                                "unsupported protocol epoch {protocol_epoch} \
                                 (server speaks {PROTOCOL_EPOCH}; v1 clients omit the field)"
                            ),
                        )
                        .with_code(manox_protocol::msg::CODE_PROTOCOL_UNSUPPORTED_EPOCH)),
                    });
                    return;
                }
                // Same client_id reconnect: the old entry is stale (the client
                // dropped its previous in-process connection, but the server-side
                // dispatch loop never noticed). Cancel any outstanding
                // ServerCall waiters on the old peer, close the old channel so
                // its serve_connection loop exits promptly, then re-seat the
                // entry with a fresh generation.
                if let Some(old) = self.clients.lock().get(&client_id) {
                    old.peer.cancel_all(
                        RpcError::new(-1, "client reconnected")
                            .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL),
                    );
                    old.conn.disconnect();
                    // §D.1: the replaced connection's streams die with it
                    // (`Closed`). Safe here — the new connection cannot have
                    // opened any stream yet (handshake is first).
                    self.end_streams_for_client(&client_id);
                }
                let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
                let hello = ClientHello {
                    client_id: client_id.clone(),
                    capabilities,
                    sessions,
                };
                self.clients.lock().insert(
                    client_id.clone(),
                    ClientEntry {
                        conn: conn.clone(),
                        peer: RpcPeer::new(),
                        hello: hello.clone(),
                        generation,
                    },
                );
                // GW10: a handshake REPLACES the ownership this client_id
                // holds. `remove_client`'s generation guard intentionally
                // skips a re-seated entry, so the old generation's owner
                // rows would otherwise survive and the pre-fix bare `push`
                // below duplicated them on every reconnect that re-declared
                // sessions — duplicated `owner_conns` frames and duplicate
                // `RpcPeer::register` of the same ServerCall MsgId (the GW2
                // auto-deny chain). Clear first, then re-add through the
                // deduping `add_owner`: the fresh hello's `sessions` list is
                // the authoritative ownership set.
                self.session_owners.lock().retain(|_, list| {
                    list.retain(|c| c != &client_id);
                    !list.is_empty()
                });
                for s in &hello.sessions {
                    self.add_owner(s, &client_id);
                }
                conn.send_to_client(FromServer::Response {
                    id,
                    outcome: Ok(json!({"ack": true})),
                });
                conn.send_to_client(FromServer::Notification {
                    note: ServerNote::Ready,
                });
                // GW1 dual emit + C1 epoch echo: the §D.5 Host mirror of the
                // handshake ack, directed to THIS connection (a handshake is
                // per-connection, never a broadcast). `Ready{epoch}` echoes
                // the epoch the connection operates under — PROTOCOL_EPOCH
                // for both accepted generations (a v1 client does not read
                // Host frames; the C4 close-out retires the note arm).
                conn.send_to_client(FromServer::Host {
                    host: HostEvent::Ready {
                        epoch: PROTOCOL_EPOCH,
                    },
                });
                (client_id, generation)
            }
            other => {
                let id = match other {
                    Ok(FromClient::Request { id, .. }) => id,
                    _ => MsgId::new("init"),
                };
                conn.send_to_client(FromServer::Response {
                    id,
                    outcome: Err(RpcError::new(-1, "expected Initialize first")
                        .with_code(manox_protocol::msg::CODE_GATEWAY_BAD_REQUEST)),
                });
                return;
            }
        };

        // ── Dispatch loop. ────────────────────────────────────────────────
        while let Ok(msg) = rx.recv().await {
            match msg {
                FromClient::Request { id, call } => {
                    // List-type calls also push a matching notification to
                    // the requesting client — the VS Code TS client reads
                    // results from notifications (push delivery), not from
                    // Response bodies (request-response). Both are sent for
                    // protocol completeness. GW1 dual emit: the §D.5
                    // HostEvent mirror rides along, directed to the
                    // requester exactly like the v1 note (same audience,
                    // same snapshot value; the C4 close-out retires the note
                    // arm). Note first, then the Host mirror, then the
                    // Response: v1 consumers see their familiar prefix.
                    let push_after = match &call {
                        ClientCall::ListModels => Some(ListPush::Models),
                        ClientCall::ListThreads => Some(ListPush::Threads),
                        ClientCall::ListCommands => Some(ListPush::Commands),
                        _ => None,
                    };
                    let outcome = handle_call(&self, &client_id, call).await;
                    if let Some(push) = push_after {
                        match push {
                            ListPush::Models => {
                                let models = self.models_snapshot();
                                conn.send_to_client(FromServer::Notification {
                                    note: ServerNote::Models {
                                        models: models.clone(),
                                    },
                                });
                                conn.send_to_client(FromServer::Host {
                                    host: HostEvent::Models { models },
                                });
                            }
                            ListPush::Threads => {
                                let threads = self.threads_snapshot();
                                conn.send_to_client(FromServer::Notification {
                                    note: ServerNote::ThreadsUpdated {
                                        threads: threads.clone(),
                                    },
                                });
                                conn.send_to_client(FromServer::Host {
                                    host: HostEvent::ThreadsUpdated { threads },
                                });
                            }
                            ListPush::Commands => {
                                let commands = self.commands_snapshot();
                                conn.send_to_client(FromServer::Notification {
                                    note: ServerNote::Commands {
                                        commands: commands.clone(),
                                    },
                                });
                                conn.send_to_client(FromServer::Host {
                                    host: HostEvent::Commands { commands },
                                });
                            }
                        }
                    }
                    conn.send_to_client(FromServer::Response { id, outcome });
                }
                FromClient::Notification { note } => {
                    handle_note(&self, &client_id, note).await;
                }
                FromClient::Reply { id, outcome } => {
                    let clients = self.clients.lock();
                    if let Some(entry) = clients.get(&client_id) {
                        entry.peer.complete(&id, outcome);
                    }
                }
                FromClient::StreamOpen {
                    stream_id,
                    stream_kind,
                } => {
                    self.open_stream(&client_id, conn.clone(), stream_id, stream_kind);
                }
                FromClient::StreamCancel { stream_id } => {
                    let handle = self
                        .streams
                        .lock()
                        .remove(&(client_id.clone(), stream_id.clone()));
                    match handle {
                        Some(handle) => handle.end(StreamEndReason::Cancelled),
                        // Unknown / already-ended stream: nothing to cancel
                        // (the terminal StreamEnd was already delivered).
                        None => {
                            tracing::debug!(stream = %stream_id.0, "stream cancel for unknown stream");
                        }
                    }
                }
            }
        }
        // Client disconnected: release ownerships; ownerless sessions drop.
        self.remove_client(&client_id, generation);
    }

    // ── Pure state accessors (no spawning). ─────────────────────────────────
    fn session_thread(&self, session_id: &str) -> Option<ThreadHandle> {
        self.sessions
            .lock()
            .get(session_id)
            .map(|s| s.thread.clone())
    }

    // ── §D.1 stream services. ───────────────────────────────────────────────
    fn open_stream(
        self: &Arc<Self>,
        client_id: &str,
        conn: Arc<dyn RpcConnection>,
        stream_id: StreamId,
        kind: StreamKind,
    ) {
        let StreamKind::FollowSession {
            session_id,
            max_messages,
        } = kind;
        let Some(thread) = self.session_thread(&session_id) else {
            // §D.7 `session/not-found` as a terminal failure frame.
            conn.send_to_client(FromServer::StreamEnd {
                stream_id,
                reason: StreamEndReason::Failure {
                    code: manox_protocol::msg::CODE_SESSION_NOT_FOUND.into(),
                    message: format!("unknown session {session_id}"),
                },
            });
            return;
        };
        let handle = StreamHandle::new(
            session_id.clone(),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(StdMutex::new(None)),
        );
        let key = (client_id.to_string(), stream_id.clone());
        self.track_stream(client_id, &stream_id, handle.clone());
        let inner = Arc::clone(self);
        let (k, h) = (key, handle.clone());
        // The task's JoinHandle is owned by the runtime; the stream's own
        // terminal StreamEnd + [`untrack_stream`] retire the registry entry.
        let _task = follow::spawn_follow_stream(
            conn,
            stream_id,
            session_id,
            max_messages,
            thread,
            &handle,
            move |_end| {
                inner.untrack_stream(&k.0, &k.1, &h);
            },
        );
    }

    /// Deliver a note to one connected client (request-scoped traffic such
    /// as bare-model stream deltas, which have no session ownership).
    ///
    /// The connection is cloned under the `clients` lock and the send runs
    /// outside it: a bounded network carrier can block inside
    /// `send_to_client`, and sending under the lock would stall every other
    /// client's routing, reply dispatch, and call registration while one
    /// peer is slow (same clone-then-send discipline as `route_note`).
    fn note_to_client(&self, client_id: &str, note: manox_protocol::ServerNote) {
        let conn = self
            .clients
            .lock()
            .get(client_id)
            .map(|entry| entry.conn.clone());
        if let Some(conn) = conn {
            conn.send_to_client(FromServer::Notification { note });
        }
    }

    /// GW1 (§D.5 dual emit): deliver a Host event to ONE connected client —
    /// the Host twin of [`Self::note_to_client`] for the directed host
    /// events (handshake `Ready`, the owner-controlled
    /// `SessionCreated`/`SessionDisposed`, requester-scoped list mirrors).
    /// Same clone-then-send discipline (GW4): the connection is cloned under
    /// the `clients` lock and the send runs outside it.
    fn host_to_client(&self, client_id: &str, host: manox_protocol::stream::HostEvent) {
        let conn = self
            .clients
            .lock()
            .get(client_id)
            .map(|entry| entry.conn.clone());
        if let Some(conn) = conn {
            conn.send_to_client(FromServer::Host { host });
        }
    }

    fn next_call_id(&self) -> MsgId {
        MsgId::new(format!(
            "call-{}",
            self.call_seq.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn owners(&self, session_id: &str) -> Vec<String> {
        self.session_owners
            .lock()
            .get(session_id)
            .cloned()
            .unwrap_or_default()
    }

    fn add_owner(&self, session_id: &str, client_id: &str) {
        let mut owners = self.session_owners.lock();
        let list = owners.entry(session_id.to_string()).or_default();
        if !list.contains(&client_id.to_string()) {
            list.push(client_id.to_string());
        }
    }

    fn remove_owner(&self, client_id: &str, session_id: &str) {
        let mut owners = self.session_owners.lock();
        if let Some(list) = owners.get_mut(session_id) {
            list.retain(|c| c != client_id);
            if list.is_empty() {
                owners.remove(session_id);
            }
        }
    }

    fn remove_client(&self, client_id: &str, generation: u64) {
        // Generation guard: if the entry for this client_id has been replaced
        // by a newer connection (same-client-id reconnect), do not delete it.
        let should_remove = self
            .clients
            .lock()
            .get(client_id)
            .is_some_and(|e| e.generation == generation);
        if !should_remove {
            return;
        }
        // Disconnect clears this connection's live streams (§D.1 `Closed`;
        // the sends into the closed connection are no-ops by then).
        self.end_streams_for_client(client_id);
        self.clients.lock().remove(client_id);
        let mut owners = self.session_owners.lock();
        let orphaned: Vec<String> = owners
            .iter_mut()
            .filter_map(|(sid, list)| {
                list.retain(|c| c != client_id);
                if list.is_empty() {
                    Some(sid.clone())
                } else {
                    None
                }
            })
            .collect();
        for sid in &orphaned {
            owners.remove(sid);
        }
        drop(owners);
        let mut sessions = self.sessions.lock();
        for sid in orphaned {
            // Ownership lost ⇒ every live stream of the session closes
            // (§D.1 `Closed`).
            self.end_streams_for_session(&sid, StreamEndReason::Closed);
            // GW2: an orphaned session's pump must not outlive the entry —
            // stop it explicitly (removal alone only detached the task).
            // Deferred reap (GW2 follow-up): an orphan whose turn is still
            // in flight keeps its entry and pump until the TurnFinished arm
            // settles it — stopping here would strand the store's `running`
            // flag and swallow the settle-time SessionStatus edges.
            let running = sessions
                .get(&sid)
                .is_some_and(|s| s.turn_active.load(Ordering::SeqCst));
            if !running && let Some(session) = sessions.remove(&sid) {
                session.stop_pump();
            }
        }
    }

    fn owner_conns(&self, session_id: &str) -> Vec<Arc<dyn RpcConnection>> {
        let owners = self.owners(session_id);
        if owners.is_empty() {
            return Vec::new();
        }
        let clients = self.clients.lock();
        owners
            .iter()
            .filter_map(|cid| clients.get(cid).map(|e| e.conn.clone()))
            .collect()
    }

    // ── Note routing. ──────────────────────────────────────────────────────
    /// §D.5: broadcast a host event to EVERY connected client (global,
    /// change-driven — not owner-scoped like `route_note`).
    fn broadcast_host(&self, host: manox_protocol::stream::HostEvent) {
        let frame = FromServer::Host { host };
        // Clone the connection list under the lock, then send outside it: a
        // stalled client on a bounded carrier must not freeze the gateway's
        // shared `clients` lock for every other path (pumps, reply dispatch,
        // call registration). Per-client non-blocking delivery is the
        // transport-policy question (§D.7); this keeps the blast radius of a
        // slow peer to the broadcasting task alone.
        let conns: Vec<Arc<dyn RpcConnection>> = self
            .clients
            .lock()
            .values()
            .map(|entry| entry.conn.clone())
            .collect();
        for conn in conns {
            conn.send_to_client(frame.clone());
        }
    }

    fn route_note(&self, session_id: &str, note: ServerNote) {
        let conns = self.owner_conns(session_id);
        if conns.is_empty() {
            tracing::trace!(session_id, "dropping note for ownerless session");
        }
        for conn in conns {
            conn.send_to_client(FromServer::Notification { note: note.clone() });
        }
    }

    /// GW1 (§D.5 dual emit): route a Host event to a session's owner set —
    /// the Host twin of [`Self::route_note`] (same audience, same
    /// clone-conns-then-send-outside-the-lock discipline; `owner_conns`
    /// already clones under the locks and returns).
    fn route_host(&self, session_id: &str, host: manox_protocol::stream::HostEvent) {
        let conns = self.owner_conns(session_id);
        for conn in conns {
            conn.send_to_client(FromServer::Host { host: host.clone() });
        }
    }

    fn note_error(&self, session_id: &str, message: &str) {
        // GW1 dual emit: the §D.5 `HostEvent::Error` mirror rides to the
        // SAME owner audience as the v1 note (a session-scoped error is not
        // broadcast to non-owners; the HostEvent vocabulary carries no
        // session id, the audience carries the scope).
        self.route_note(
            session_id,
            ServerNote::Error {
                session_id: Some(session_id.into()),
                message: message.into(),
            },
        );
        self.route_host(
            session_id,
            HostEvent::Error {
                message: message.into(),
            },
        );
    }

    // ── Snapshots (queries). ────────────────────────────────────────────────
    //
    // T10 (§D.6): the v1 `ThreadHistory`/`ThreadInfo` snapshot emitters are
    // gone. History replays through the §D.1 follow stream's opening
    // `Snapshot` frame; thread meta-info rides the projection baseline +
    // P-face deltas (§E); `has_interacted` is a projection key.
    fn threads_snapshot(&self) -> Vec<ThreadListItem> {
        let store = manox_agent::thread_store_global();
        store.read(|s| {
            s.summaries()
                .iter()
                .map(|t| ThreadListItem {
                    id: t.id.clone(),
                    title: t.display_title().to_string(),
                    updated_at: t.updated_at as i32,
                    running: s.is_running(&t.id),
                    // GW5: unread is client-owned — the server keeps no
                    // focus mirror, so the deprecated list field is always
                    // false (clients derive unread from the
                    // `SessionStatus.unread` settle deltas and clear it
                    // locally on focus). C4 removes the field.
                    unread: false,
                    errored: t.errored,
                    pending_auth: s.pending_auth_contains(&t.id),
                    pending_plan: s.pending_plan_contains(&t.id),
                    background_work: s.background_work_contains(&t.id),
                    model_id: t.model_id.clone(),
                    pinned: t.pinned,
                    archived: t.archived,
                    parent_id: t.parent_id.clone(),
                    depth: t.depth,
                })
                .collect()
        })
    }

    fn models_snapshot(&self) -> Vec<ModelInfo> {
        deduped_models(manox_agent::provider_glue::global().models())
            .iter()
            .map(model_to_wire)
            .collect()
    }

    fn commands_snapshot(&self) -> Value {
        let mut commands = Vec::new();
        for meta in manox_agent::slash_builtins::BUILTIN_SLASH_COMMANDS {
            commands.push(json!({
                "name": meta.name,
                "description": null,
                "kind": "command",
                "argument_hint": null,
                "i18n_key": meta.description_key,
            }));
        }
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::from_iter(
            manox_agent::slash_builtins::BUILTIN_SLASH_COMMANDS
                .iter()
                .map(|m| m.name.to_string()),
        );
        if let Some(registry) = manox_agent::command::try_global() {
            for (key, def) in registry.entries() {
                if seen.contains(key.as_str()) {
                    continue;
                }
                seen.insert(key.clone());
                commands.push(json!({
                    "name": key,
                    "description": def.description,
                    "kind": "command",
                    "argument_hint": def.argument_hint,
                }));
            }
        }
        if let Some(registry) = manox_agent::skill::try_global() {
            for (key, def) in registry.entries() {
                if seen.contains(key.as_str()) {
                    continue;
                }
                seen.insert(key.clone());
                commands.push(json!({
                    "name": key,
                    "description": def.description,
                    "kind": "skill",
                    "argument_hint": null,
                }));
            }
        }
        json!(commands)
    }
}

/// Which notification to push after a list-type ClientCall succeeds.
#[derive(Debug, Clone, Copy)]
enum ListPush {
    Models,
    Threads,
    Commands,
}

// ── ClientCall dispatch (free fn — borrowed inner, no move per call). ────────
async fn handle_call(
    inner: &Arc<AgentServerInner>,
    client_id: &str,
    call: ClientCall,
) -> Result<Value, RpcError> {
    match call {
        ClientCall::Initialize(_) => Err(RpcError::new(-1, "already initialized")
            .with_code(manox_protocol::msg::CODE_GATEWAY_BAD_REQUEST)),
        // ── v2 write calls (§D.2: receipts only, L7). ───────────────────────
        ClientCall::CreateSession {
            cwd,
            project,
            initial_model,
            approval_mode,
            reasoning_effort,
        } => {
            AgentServerInner::create_session_request(
                inner,
                client_id,
                SessionIntent {
                    session_id: None,
                    cwd,
                    project,
                    initial_model,
                    approval_mode,
                    reasoning_effort,
                },
            )
            .await
        }
        ClientCall::Submit {
            session_id,
            text,
            images,
            origin_rpc,
        } => {
            inner
                .submit(client_id, &session_id, text, images, None, origin_rpc)
                .await
        }
        ClientCall::Steer {
            session_id,
            message_id,
            text,
            images,
            origin_rpc,
        } => inner.steer(&session_id, message_id, text, images, origin_rpc),
        // ── v2 journal read calls (§D.2 PageHistory, §E.3 Q face). ─────────
        ClientCall::PageHistory {
            session_id,
            through_seq,
            before_seq,
            max_messages,
        } => {
            // §D.2: "冷读不激活 engine，jsonl 直读" (GW6). The live engine
            // seam answers when it is materialized; otherwise the persisted
            // journal is read straight off disk — a cold session must never
            // answer "journal engine is not materialized" (pre-fix the
            // client's gap-repair and backwards paging both dead-ended on
            // it). A live session with neither an answering engine nor a
            // persisted file (a fresh deferred thread) has an EMPTY journal,
            // not a missing one; a session that is neither live nor
            // persisted stays `session/not-found`.
            let thread = inner.session_thread(&session_id);
            let snapshot = match &thread {
                Some(t) => t.journal_snapshot().await,
                None => None,
            };
            let snapshot = match snapshot {
                Some(data) => data,
                None => match journal_query::cold_snapshot(&session_id).await {
                    Some(data) => data,
                    None if thread.is_some() => manox_agent::engine::JournalSnapshotData {
                        cursor: 0,
                        records: Vec::new(),
                    },
                    None => {
                        return Err(RpcError::new(-1, "unknown session")
                            .with_code(manox_protocol::msg::CODE_SESSION_NOT_FOUND));
                    }
                },
            };
            journal_query::page_history(snapshot, through_seq, before_seq, max_messages)
        }
        // GW3 (§D.4): withdraw a pending adjudication delivery — the server
        // converges it through the existing expire path (fail-closed), never
        // waiting out the 300s call timeout for a client that navigated away.
        ClientCall::CancelDelivery { delivery_id } => Ok(json!({
            "cancelled": inner.cancel_delivery(client_id, &delivery_id),
        })),
        ClientCall::GetConversationInfo { session_id } => {
            let thread = inner.session_thread(&session_id).ok_or_else(|| {
                RpcError::new(-1, "unknown session")
                    .with_code(manox_protocol::msg::CODE_SESSION_NOT_FOUND)
            })?;
            journal_query::conversation_info(&inner.conversation_info_cache, &thread, &session_id)
                .await
        }
        ClientCall::OpenSession { session_id } => open_session(inner, client_id, &session_id).await,
        ClientCall::ListThreads => serde_json::to_value(inner.threads_snapshot()).map_err(|_| {
            RpcError::new(-1, "threads serialization failed")
                .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL)
        }),
        ClientCall::ListModels => serde_json::to_value(inner.models_snapshot()).map_err(|_| {
            RpcError::new(-1, "models serialization failed")
                .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL)
        }),
        ClientCall::ListCommands => Ok(inner.commands_snapshot()),
        // T10 (§D.6): the v1 query surface is retired — usage rides the
        // journal (Q face `GetConversationInfo`), the model and every header
        // chip field ride the projection baseline/deltas (§E). The variants
        // stay in the enum for the dual-protocol window; answering with an
        // explicit error is the removal signal.
        ClientCall::GetUsage { .. }
        | ClientCall::GetCurrentModel { .. }
        | ClientCall::ThreadInfo { .. } => Err(RpcError::new(
            -1,
            "v1 query surface removed (T10): use the \
                 follow stream, projections, and GetConversationInfo",
        )
        .with_code(manox_protocol::msg::CODE_GATEWAY_BAD_REQUEST)),
        // GW7: an explicit stable code, not a bare -1 — clients that
        // declared terminal support must be able to distinguish "feature
        // not built yet" from a generic failure (§D.7 code set, ratified
        // with the msg.rs constant + spec revision).
        ClientCall::TerminalAttach { .. } | ClientCall::TerminalSnapshot { .. } => {
            Err(RpcError::new(-1, "terminal support lands in β-3b")
                .with_code(manox_protocol::msg::CODE_FEATURE_UNAVAILABLE))
        }
        ClientCall::ModelChat {
            request_id,
            model,
            messages,
            tools,
        } => {
            // Bare-model completion (the VS Code LanguageModelChat provider):
            // stream deltas back to the CALLING client as request-scoped
            // notes. Ported from the retired actor command engine.
            let registry = manox_agent::provider_glue::global();
            let done = |stop: Option<&str>, error: Option<String>| {
                inner.note_to_client(
                    client_id,
                    manox_protocol::ServerNote::ModelChatDone {
                        request_id: request_id.clone(),
                        stop: stop.map(str::to_string),
                        error,
                    },
                );
            };
            let Some(resolved) = manox_harness::model_ref::resolve_model_ref(&registry, &model)
            else {
                done(None, Some("unknown model".into()));
                return Ok(json!({}));
            };
            match registry.resolve_stream(&resolved) {
                Ok(stream) => {
                    let ctx = crate::model_chat::build_context(&resolved, &messages, &tools);
                    let sink = {
                        let inner = Arc::clone(inner);
                        let owner = client_id.to_string();
                        Arc::new(move |note| inner.note_to_client(&owner, note))
                    };
                    crate::model_chat::start(
                        request_id,
                        stream,
                        ctx,
                        sink,
                        Arc::clone(&inner.model_chats),
                    );
                }
                Err(err) => done(None, Some(err.to_string())),
            }
            Ok(json!({}))
        }
    }
}

async fn open_session(
    inner: &Arc<AgentServerInner>,
    owner: &str,
    session_id: &str,
) -> Result<Value, RpcError> {
    // Phase 1 (fast path): a live session is re-owned without any IO.
    if inner.sessions.lock().contains_key(session_id) {
        return reown_existing(inner, owner, session_id);
    }
    // Phase 2 (U8): the journal-file IO runs OUTSIDE the `sessions` lock —
    // a slow disk must not stall the whole gateway table (pre-fix this ran
    // under the single hold, recorded as known debt in the GW2 batch).
    // Concurrent racers load the SAME `ThreadHandle` (the store's weak
    // upgrade), and only phase 3 decides who inserts.
    let thread = manox_agent::thread_store::global()
        .with_mut(|s| s.load_thread(session_id))
        .ok_or_else(|| {
            RpcError::new(-1, "thread not found")
                .with_code(manox_protocol::msg::CODE_SESSION_NOT_FOUND)
        })?;
    // Phase 3: recheck–spawn–insert under ONE lock hold. The pump is
    // spawned HERE, not in phase 2, so a race still yields exactly one
    // entry and one pump: the loser finds the winner's entry and re-owns
    // it, discarding its own load (the same handle via the weak upgrade —
    // nothing leaks). GW2's structural invariant and GW6's resume
    // singleflight both ride this hold.
    {
        let mut sessions = inner.sessions.lock();
        if sessions.contains_key(session_id) {
            drop(sessions);
            return reown_existing(inner, owner, session_id);
        }
        // GW5: the open-time `set_unread(session_id, false)` store mirror
        // write is gone — unread is client-owned (clients clear their
        // badge locally on focus); the server keeps no read-state.
        let turn_active = Arc::new(AtomicBool::new(false));
        let pending_submits = Arc::new(StdMutex::new(Vec::new()));
        let pump_cancel = tokio_util::sync::CancellationToken::new();
        let pump = spawn_pump(
            Arc::clone(inner),
            session_id.into(),
            thread.clone(),
            turn_active.clone(),
            pending_submits.clone(),
            pump_cancel.clone(),
        );
        sessions.insert(
            session_id.into(),
            ServerSession {
                thread: thread.clone(),
                pump_cancel,
                pump,
                turn_active,
                pending_submits,
            },
        );
        drop(sessions);
        inner.add_owner(session_id, owner);
        inner.route_note(
            session_id,
            ServerNote::SessionCreated {
                session_id: session_id.into(),
            },
        );
        // GW1 dual emit: the Host mirror to the owner set (after
        // `add_owner` so the opening client is in the audience).
        inner.route_host(session_id, session_created_event(session_id, &thread));
        Ok(json!({ "restored": true }))
    }
}

/// The idempotent re-own of a live session: the owner joins and the
/// directed `SessionCreated` note + GW1 Host mirror reach ONLY the new
/// owner (owner-set control, never a broadcast — the existing owners are
/// not disturbed). T10 (§D.6): no v1 snapshot replay here; the client's
/// history comes from the §D.1 follow stream's `Snapshot` frame.
fn reown_existing(
    inner: &Arc<AgentServerInner>,
    owner: &str,
    session_id: &str,
) -> Result<Value, RpcError> {
    let thread = {
        let sessions = inner.sessions.lock();
        let Some(existing) = sessions.get(session_id) else {
            // Gone between a check and this re-own (a concurrent dispose):
            // answer not-found; the caller's retry re-enters the open path.
            return Err(RpcError::new(-1, "thread not found")
                .with_code(manox_protocol::msg::CODE_SESSION_NOT_FOUND));
        };
        existing.thread.clone()
    };
    inner.add_owner(session_id, owner);
    inner.note_to_client(
        owner,
        ServerNote::SessionCreated {
            session_id: session_id.into(),
        },
    );
    inner.host_to_client(owner, session_created_event(session_id, &thread));
    Ok(json!({ "restored": true }))
}
/// GW1 (§D.5): build the `SessionCreated` Host mirror — the wire note
/// carries only the id, the Host event carries the header. Projected from
/// the live thread exactly like the follow stream's snapshot header
/// (`cwd` from the thread, `createdAt` the projection moment — the
/// authoritative header rides the follow Snapshot; this mirror is
/// transitional until C4).
fn session_created_event(
    session_id: &str,
    thread: &ThreadHandle,
) -> manox_protocol::stream::HostEvent {
    let cwd = thread.read(|t| t.cwd().to_string_lossy().into_owned());
    HostEvent::SessionCreated {
        session_id: session_id.to_string(),
        header: manox_protocol::journal::ThreadHeader {
            id: session_id.to_string(),
            cwd,
            parent_session: None,
            metadata: None,
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        },
    }
}

// ── ClientNote dispatch (fire-and-forget). ───────────────────────────────────
async fn handle_note(inner: &Arc<AgentServerInner>, owner: &str, note: ClientNote) {
    match note {
        ClientNote::CreateSession { session_id, cwd } => {
            // Compat entry (§D.3 dual-protocol window): forward to the §D.2
            // request path (no intent fields beyond cwd) and discard the
            // receipt — v1 clients never await it. The explicit
            // `session_id` is passed through so the desktop/webui ids stay
            // stable; the request path is idempotent on a live session.
            let intent = SessionIntent {
                session_id: Some(session_id),
                cwd,
                project: None,
                initial_model: None,
                approval_mode: None,
                reasoning_effort: None,
            };
            let _ = AgentServerInner::create_session_request(inner, owner, intent).await;
        }
        ClientNote::DisposeSession { session_id } => inner.dispose_session(owner, &session_id),
        ClientNote::DetachSession { session_id } => inner.detach_session(owner, &session_id),
        ClientNote::Submit {
            session_id,
            text,
            images,
            client_id,
        } => {
            // Compat entry: forward to the §D.2 receipt path, discard.
            let _ = inner
                .submit(owner, &session_id, text, images, client_id, None)
                .await;
        }
        ClientNote::Steer {
            session_id,
            client_id,
            text,
            images,
        } => {
            // Compat entry: the note's `client_id` is the steer id.
            let _ = inner.steer(&session_id, client_id, text, images, None);
        }
        ClientNote::DropQueued {
            session_id,
            client_id,
        } => inner.drop_queued(&session_id, client_id),
        ClientNote::CancelTurn { session_id } => {
            if let Some(t) = inner.session_thread(&session_id) {
                t.with_mut(|t| t.cancel());
            } else {
                inner.note_error(&session_id, "unknown session");
            }
        }
        ClientNote::SetModel { session_id, id } => inner.set_model(&session_id, &id),
        ClientNote::SetReasoningEffort { session_id, effort } => {
            inner.set_reasoning_effort(&session_id, &effort)
        }
        ClientNote::SetApprovalMode { session_id, mode } => {
            inner.set_approval_mode(&session_id, &mode)
        }
        ClientNote::SetCwd { session_id, cwd } => inner.set_cwd(&session_id, &cwd),
        ClientNote::SetPlanMode {
            session_id,
            enabled,
        } => {
            if let Some(t) = inner.session_thread(&session_id) {
                t.with_mut(|t| t.set_plan_mode(enabled));
            } else {
                inner.note_error(&session_id, "unknown session");
            }
        }
        ClientNote::PlanSeedExecution {
            session_id,
            plan_file,
        } => inner.plan_seed(&session_id, &plan_file),
        ClientNote::Compact {
            session_id,
            instructions,
        } => inner.compact(&session_id, instructions),
        ClientNote::Goal {
            session_id,
            action,
            objective,
            budget,
            max_rounds,
        } => inner.goal(&session_id, &action, objective, budget, max_rounds),
        ClientNote::StopBackgroundTask { task_id, .. } => {
            manox_agent::runtime::handle().spawn(async move {
                let _ = manox_agent::background_task::stop(&task_id).await;
            });
        }
        ClientNote::ArchiveThread {
            session_id,
            archived,
        } => inner.archive_thread(owner, &session_id, archived),
        ClientNote::PinThread { session_id, pinned } => {
            manox_agent::thread_store::global().with_mut(|s| s.pin_thread(&session_id, pinned));
        }
        // GW5: unread is client-owned — the server's single-slot `focused`
        // mirror is removed (it could not express multi-client focus, and
        // the desktop never sent FocusThread, so the session the user was
        // WATCHING still lit unread). The note variant survives the
        // dual-protocol window as a no-op; C4 removes it. Clients clear
        // their own unread badge on focus.
        ClientNote::FocusThread { .. } => {}
        ClientNote::TerminalInput { .. } | ClientNote::TerminalResize { .. } => {
            // β-3b: route to TerminalHandle. GW7: until then, an explicit
            // Error note to the SENDING client — pre-fix the note was
            // silently swallowed, which is data loss for a client that
            // declared terminal support (session_id None: the drop is
            // connection-scoped, not a session fact).
            let message = "terminal input dropped: terminal support lands in β-3b";
            inner.note_to_client(
                owner,
                ServerNote::Error {
                    session_id: None,
                    message: message.into(),
                },
            );
            // GW1 dual emit: the §D.5 Host mirror, directed to the sending
            // connection (the note's audience).
            inner.host_to_client(
                owner,
                HostEvent::Error {
                    message: message.into(),
                },
            );
        }
        ClientNote::AppendUserMessage {
            session_id,
            text,
            images,
        } => inner.append_user_message(&session_id, text, images),
        ClientNote::AppendUiNote {
            session_id,
            kind,
            data,
        } => inner.append_ui_note(&session_id, &kind, data),
        ClientNote::CancelModelChat { request_id } => {
            crate::model_chat::cancel(&inner.model_chats, &request_id)
        }
        ClientNote::Shutdown => {
            // Host-driven teardown rides the connection drop; nothing to do
            // per-note.
        }
    }
}

// ── Per-command handlers (&self methods, no spawning). ────────────────────────

/// The canonical on-disk journal path for a session id
/// (`<config>/sessions/<id>.jsonl`) — the same name creation and the
/// repository scan use, so the GW11 identity probe, the GW6 cold read, and
/// the eventual materialization can never disagree about the file.
pub(crate) fn persisted_session_file(session_id: &str) -> Option<PathBuf> {
    manox_agent::paths::sessions_dir().ok().map(|dir| {
        dir.join(manox_harness::session::repository::session_file_name(
            session_id,
        ))
    })
}

/// The §D.2 `CreateSession` intent: optional explicit id (the compat
/// `ClientNote::CreateSession` always supplies one; the v2 request mints
/// server-side), working directory, project binding, and the initial
/// model / approval mode / reasoning effort the session opens with (the
/// "project/model inheritance" defect regression, §J.7).
struct SessionIntent {
    session_id: Option<String>,
    cwd: Option<String>,
    project: Option<String>,
    initial_model: Option<manox_protocol::ModelRef>,
    approval_mode: Option<String>,
    reasoning_effort: Option<String>,
}

impl AgentServerInner {
    /// §D.2 `CreateSession`: build a live session from the intent and answer
    /// `{session_id}`. The thread opens on the `new_in_project` path when a
    /// project is given (fresh session bound to the project in one step,
    /// no orphaned pre-project file), else `new_fresh`; `initial_model`
    /// resolves through the single convergence point
    /// `resolve_model_ref` (L8) *before* anything is created — an
    /// unresolvable canonical ref answers `model/unresolvable` without a
    /// side effect. Re-opening a live session id is idempotent: the
    /// existing id answers and the live session is left untouched. An id
    /// whose journal file already exists on disk is an existing COLD
    /// session: it restores through the `OpenSession` path (§D.2
    /// idempotency on disk, GW11) and is never re-minted over its file.
    async fn create_session_request(
        inner: &Arc<AgentServerInner>,
        owner: &str,
        intent: SessionIntent,
    ) -> Result<Value, RpcError> {
        // Resolve every intent field that can fail before touching state.
        let model = match intent.initial_model.as_ref() {
            None => None,
            Some(m) => {
                let registry = manox_agent::provider_glue::global();
                match manox_harness::model_ref::resolve_model_ref(&registry, &m.0) {
                    Some(model) => Some(model),
                    None => {
                        return Err(RpcError::new(-1, format!("unknown model: {}", m.0))
                            .with_code(manox_protocol::msg::CODE_MODEL_UNRESOLVABLE));
                    }
                }
            }
        };
        let approval = match intent.approval_mode.as_deref() {
            None => None,
            Some(s) => match serde_json::from_value::<PermissionMode>(Value::String(s.to_string()))
            {
                Ok(mode) => Some(mode),
                Err(_) => {
                    return Err(RpcError::new(-1, format!("unknown approval mode: {s}"))
                        .with_code(manox_protocol::msg::CODE_GATEWAY_BAD_REQUEST));
                }
            },
        };
        let effort = match intent.reasoning_effort.as_deref() {
            None => None,
            Some("high") => Some(ReasoningEffort::High),
            Some("max") => Some(ReasoningEffort::Max),
            Some(other) => {
                return Err(
                    RpcError::new(-1, format!("unknown reasoning effort: {other}"))
                        .with_code(manox_protocol::msg::CODE_GATEWAY_BAD_REQUEST),
                );
            }
        };
        // Idempotent re-open of a live session (§D.2).
        if let Some(existing) = intent.session_id.as_deref()
            && inner.sessions.lock().contains_key(existing)
        {
            inner.add_owner(existing, owner);
            return Ok(json!({ "session_id": existing }));
        }
        // §D.2 idempotency on disk (GW11): an id whose journal file already
        // exists is an existing COLD session — restore it through the
        // OpenSession path instead of minting a fresh session over the id.
        // The pre-fix fall-through reached `new_fresh` ("never restores the
        // previous session"), whose deferred materialization rewrote the
        // existing file wholesale on the first assistant message, erasing
        // the cold session's history. The probe reads the canonical
        // sessions-dir path directly rather than the store's scan-populated
        // map, so it holds even when no list refresh has ever run;
        // `note_session_path` seeds the identity map for the restore's
        // `load_thread`.
        if let Some(existing) = intent.session_id.as_deref()
            && let Some(path) = persisted_session_file(existing)
            && path.exists()
        {
            manox_agent::thread_store::global().with_mut(|s| s.note_session_path(existing, &path));
            open_session(inner, owner, existing).await?;
            return Ok(json!({ "session_id": existing }));
        }
        let session_id = intent
            .session_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let project = intent.project.as_ref().map(PathBuf::from);
        let cwd = intent
            .cwd
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| project.clone().unwrap_or_else(|| inner.cwd.clone()));
        let thread = match &project {
            Some(p) => Thread::new_in_project(ThreadId(session_id.clone()), p.clone()),
            None => Thread::new_fresh(ThreadId(session_id.clone()), cwd),
        };
        // Intent application: model (explicit canonical or the global
        // default), approval mode, reasoning effort.
        let initial = model.or_else(manox_agent::provider_glue::default_model);
        thread.with_mut(|t| {
            if let Some(model) = initial {
                t.set_model(model);
            }
            if let Some(mode) = approval {
                t.set_permission_mode(mode);
            }
            if let Some(effort) = effort {
                t.set_reasoning_effort(effort);
            }
        });
        let turn_active = Arc::new(AtomicBool::new(false));
        let pending_submits = Arc::new(StdMutex::new(Vec::new()));
        let pump_cancel = tokio_util::sync::CancellationToken::new();
        let pump = spawn_pump(
            Arc::clone(inner),
            session_id.clone(),
            thread.clone(),
            turn_active.clone(),
            pending_submits.clone(),
            pump_cancel.clone(),
        );
        // GW2: `insert_session` terminates the pump of any entry this
        // replaces — the live-session check above and this insert are not
        // one atomic step, so a racing create/open must not leave two pumps
        // subscribed to the same thread.
        inner.insert_session(
            session_id.clone(),
            ServerSession {
                thread: thread.clone(),
                pump_cancel,
                pump,
                turn_active,
                pending_submits,
            },
        );
        inner.add_owner(&session_id, owner);
        inner.route_note(
            &session_id,
            ServerNote::SessionCreated {
                session_id: session_id.clone(),
            },
        );
        // GW1 dual emit: the §D.5 Host mirror to the owner set (after
        // `add_owner` so the creating client is in the audience).
        inner.route_host(&session_id, session_created_event(&session_id, &thread));
        // T10 (§D.6): the create-time `PermissionModeChanged` mirror is gone —
        // the mode rides the follow-stream snapshot's `permission_mode`
        // projection (seeded from the live thread) and the
        // `permissionModeChange` journal entry on later changes.
        Ok(json!({ "session_id": session_id }))
    }

    fn dispose_session(&self, owner: &str, session_id: &str) {
        // §D.5 dispose semantics: only the REQUESTING client is told — the
        // session survives for every other owner (broadcasting here made a
        // second client's UI drop a still-live session). Owner-table
        // removal below is per-client regardless.
        if let Some(conn) = self.clients.lock().get(owner).map(|e| e.conn.clone()) {
            conn.send_to_client(FromServer::Notification {
                note: ServerNote::SessionDisposed {
                    session_id: session_id.into(),
                },
            });
            // GW1 dual emit: the §D.5 Host mirror, directed to the same
            // single connection (owner-set control, never a broadcast).
            conn.send_to_client(FromServer::Host {
                host: HostEvent::SessionDisposed {
                    session_id: session_id.into(),
                },
            });
        }
        self.remove_owner(owner, session_id);
        if self.owners(session_id).is_empty() {
            let removed = { self.sessions.lock().remove(session_id) };
            if let Some(session) = removed {
                // GW2: terminate the pump BEFORE the entry goes away — the
                // pre-fix removal only dropped the JoinHandle, which detaches
                // (the pump kept its ThreadHandle and ran forever), so a
                // reopen of the same id spawned a second pump.
                session.stop_pump();
                // Disposal closes every live stream of the session (§D.1
                // `Closed`).
                self.end_streams_for_session(session_id, StreamEndReason::Closed);
                if session.turn_active.load(Ordering::SeqCst) {
                    session.thread.with_mut(|t| t.cancel());
                    manox_agent::thread_store::global().with_mut(|s| s.mark_idle(session_id));
                }
            }
        }
    }

    fn detach_session(&self, owner: &str, session_id: &str) {
        // Detach drops this client's strong reference without cancelling: a
        // turn keeps running for any other owner, and the thread persists for
        // reopen. Only the detaching client is told (it stops being an owner,
        // so route_note would drop the note after the table changes).
        if let Some(conn) = self.clients.lock().get(owner).map(|e| e.conn.clone()) {
            conn.send_to_client(FromServer::Notification {
                note: ServerNote::SessionDisposed {
                    session_id: session_id.into(),
                },
            });
            // GW1 dual emit: the §D.5 Host mirror, directed to the detaching
            // connection only.
            conn.send_to_client(FromServer::Host {
                host: HostEvent::SessionDisposed {
                    session_id: session_id.into(),
                },
            });
        }
        self.remove_owner(owner, session_id);
        if self.owners(session_id).is_empty() {
            // Ownership lost ⇒ live streams close (§D.1 `Closed`).
            self.end_streams_for_session(session_id, StreamEndReason::Closed);
            // GW2 follow-up (deferred reap): a detach while the turn still
            // runs keeps the entry and its pump — the settle bookkeeping
            // (store running/unread/pending flags and the SessionStatus
            // edges) belongs to the pump, and stopping it here would strand
            // the store's `running` flag true with nobody left to clear it.
            // The TurnFinished arm reaps the orphan once it settles.
            let running = self
                .sessions
                .lock()
                .get(session_id)
                .is_some_and(|s| s.turn_active.load(Ordering::SeqCst));
            if !running {
                let removed = { self.sessions.lock().remove(session_id) };
                if let Some(session) = removed {
                    session.stop_pump();
                }
            }
        }
    }

    /// §D.2 `Submit`: performs the submission and answers with the receipt
    /// `{accepted, message_id?}` (L7 — the transcript arrives through the
    /// follow stream). K5: a direct (non-queued, non-slash) submission is
    /// persisted BEFORE the receipt — accepted ⟹ logged — through
    /// `ThreadEngine::persist_user_submission`; a persistence failure
    /// REFUSES the receipt (coded `gateway/internal`). The `origin_rpc`
    /// correlation rides the pinned origin on the entry (receipt-id pairing
    /// from the append point is GW8). The compat `ClientNote::Submit`
    /// forwards here with `origin_rpc = None`.
    async fn submit(
        &self,
        owner: &str,
        session_id: &str,
        text: String,
        images: Vec<ImageAttachment>,
        client_id: Option<String>,
        origin_rpc: Option<String>,
    ) -> Result<Value, RpcError> {
        let receipt = |accepted: bool, message_id: Option<String>| {
            Ok(json!({ "accepted": accepted, "message_id": message_id }))
        };
        let images: Vec<(String, String)> = images
            .into_iter()
            .map(|i| (base64_bytes::encode(&i.data), i.mime_type))
            .collect();
        let slash = if images.is_empty() {
            parse_slash(&text)
        } else {
            None
        };
        // Navigation built-ins take effect immediately even mid-turn.
        if let Some((name, _)) = slash.as_ref()
            && let Some(builtin) = manox_agent::slash_builtins::canonical_builtin(name)
            && matches!(builtin.name, "exit" | "new")
        {
            self.archive_thread(owner, session_id, true);
            return receipt(true, None);
        }
        let Some(session) = self.sessions.lock().get(session_id).map(|s| {
            (
                s.thread.clone(),
                s.turn_active.clone(),
                s.pending_submits.clone(),
            )
        }) else {
            self.note_error(session_id, "unknown session");
            return Err(RpcError::new(-1, "unknown session")
                .with_code(manox_protocol::msg::CODE_SESSION_NOT_FOUND));
        };
        let (thread, turn_active, pending_submits) = session;
        let client_id = client_id.unwrap_or_else(|| owner.to_string());
        if turn_active.load(Ordering::SeqCst) && slash.is_none() {
            let ui = thread.read(|t| MessageUiMetadata {
                model_id: t.model().map(|m| m.id.clone()),
                approval_mode: Some(t.permission_mode().as_i64()),
                ..Default::default()
            });
            pending_submits.lock().unwrap().push(QueuedSubmit {
                client_id,
                text,
                images,
                ui,
                origin: origin_rpc,
            });
            return receipt(true, None);
        }
        // Slash resolution first (its handlers mutate the facade): a slash
        // hit keeps the legacy transcript semantics and never takes the
        // accept-time persist path. The origin pin lands here, as before —
        // a run started by a slash builtin carries it.
        let slashed = thread.with_mut(|t| {
            t.set_pending_turn_origin(origin_rpc.clone());
            if let Some((name, args)) = slash {
                let ui = MessageUiMetadata {
                    model_id: t.model().map(|m| m.id.clone()),
                    approval_mode: Some(t.permission_mode().as_i64()),
                    ..Default::default()
                };
                let slash_ui = MessageUiMetadata {
                    display_text: Some(text.clone()),
                    ..ui
                };
                let builtin_hit = t.run_slash_builtin(&name, &args, Some(slash_ui.clone()));
                let command_hit = manox_agent::command::try_global().is_some()
                    && t.submit_command(&name, &args, Some(slash_ui.clone()));
                let skill_hit = manox_agent::skill::try_global().is_some()
                    && t.submit_skill(&name, &args, Some(slash_ui));
                return builtin_hit || command_hit || skill_hit;
            }
            false
        });
        if slashed {
            return receipt(true, None);
        }
        if text.trim().is_empty() && images.is_empty() {
            return receipt(false, None);
        }
        // K5 (accepted ⟹ logged): persist the user entry BEFORE the
        // receipt. Skipped when residual pending prompts would make
        // run_turn's merged prompt differ from this text — the actor's
        // drain-time persistence then covers the merged entry under the
        // same content-match contract (persist_prompt_user_entry). An
        // unmaterialized engine answers Ok(None) and falls to the same
        // drain path.
        let mut accepted_entry: Option<String> = None;
        if !thread.read(|t| t.has_pending_prompts())
            && let Some(engine) = thread.read(|t| t.engine_handle())
        {
            // The SAME (text, images) run_turn will hand the engine — the
            // middleware skip is an exact content match (K5 contract):
            // text normalized like `to_message_content` (no Text block
            // when blank), images as kernel ContentBlocks.
            let persist_text = if text.trim().is_empty() {
                String::new()
            } else {
                text.clone()
            };
            let blocks: Vec<manox_harness::types::ContentBlock> = images
                .iter()
                .map(
                    |(data, mime_type)| manox_harness::types::ContentBlock::Image {
                        data: data.clone(),
                        mime_type: mime_type.clone(),
                    },
                )
                .collect();
            match engine
                .persist_user_submission(&persist_text, blocks, origin_rpc.clone())
                .await
            {
                Ok(id) => accepted_entry = id,
                Err(err) => {
                    // No durability, no receipt. Un-pin the origin so a
                    // later run does not misattribute this refused submit,
                    // and tell the client why.
                    thread.with_mut(|t| t.set_pending_turn_origin(None));
                    let message = format!("submit persistence failed: {err}");
                    self.note_error(session_id, &message);
                    return Err(RpcError::new(-1, message)
                        .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL));
                }
            }
        }
        // Insert + run. The pin arms the actor's middleware skip so the
        // run's own user MessageEnd records the accepted entry instead of
        // appending a duplicate.
        thread.with_mut(|t| {
            t.set_pending_turn_accepted_entry(accepted_entry);
            let ui = MessageUiMetadata {
                model_id: t.model().map(|m| m.id.clone()),
                approval_mode: Some(t.permission_mode().as_i64()),
                ..Default::default()
            };
            let content = to_message_content(text, images);
            t.insert_user_message_with_content_and_ui_metadata(content, Some(ui));
            t.run_turn();
        });
        let message_id = thread.read(|t| t.last_user_message_id().map(str::to_string));
        receipt(true, message_id)
    }

    /// §D.2 `Steer`: injects the steer and answers with the receipt
    /// `{accepted, message_id?}` (the echo of the call's steer id). The
    /// compat `ClientNote::Steer` forwards here with its `client_id` as
    /// `message_id`.
    fn steer(
        &self,
        session_id: &str,
        message_id: String,
        text: String,
        images: Vec<ImageAttachment>,
        // The steer id IS the echo correlation (the client retires its
        // echo when the steer's own injection settles); no origin pin.
        _origin_rpc: Option<String>,
    ) -> Result<Value, RpcError> {
        let images: Vec<(String, String)> = images
            .into_iter()
            .map(|i| (base64_bytes::encode(&i.data), i.mime_type))
            .collect();
        let Some((thread, pending_submits)) = self
            .sessions
            .lock()
            .get(session_id)
            .map(|s| (s.thread.clone(), s.pending_submits.clone()))
        else {
            self.note_error(session_id, "unknown session");
            return Err(RpcError::new(-1, "unknown session")
                .with_code(manox_protocol::msg::CODE_SESSION_NOT_FOUND));
        };
        // A steer removes its own parked follow-up so the turn-end drain does
        // not resend the same text as a plain follow-up.
        pending_submits
            .lock()
            .unwrap()
            .retain(|q| q.client_id != message_id);
        thread.with_mut(|t| {
            let ui = MessageUiMetadata {
                model_id: t.model().map(|m| m.id.clone()),
                approval_mode: Some(t.permission_mode().as_i64()),
                ..Default::default()
            };
            let content = to_message_content(text, images);
            if t.is_running() {
                t.enqueue_steer(content, Some(ui));
            } else {
                t.insert_user_message_with_content_and_ui_metadata(content, Some(ui));
                t.run_turn();
            }
        });
        // T10 (§D.6): the `SteerPending` note mirror is gone — the steer's
        // durable identity is the `message` journal row (`originRpc` echo
        // retirement for the submitting client; every owner sees the row on
        // the follow stream).
        Ok(json!({
            "accepted": true,
            "message_id": message_id,
        }))
    }

    fn drop_queued(&self, session_id: &str, client_id: String) {
        if let Some(session) = self.sessions.lock().get(session_id) {
            let pending = session.pending_submits.clone();
            pending.lock().unwrap().retain(|q| q.client_id != client_id);
        }
    }

    fn set_model(&self, session_id: &str, id: &str) {
        let Some(thread) = self.session_thread(session_id) else {
            return self.note_error(session_id, "unknown session");
        };
        let registry = manox_agent::provider_glue::global();
        match manox_harness::model_ref::resolve_model_ref(&registry, id) {
            Some(model) => {
                // T10: the v1 `ThreadInfo` republish is gone — the engine
                // journals the change and the P-face delta refreshes chips.
                thread.with_mut(|t| t.set_model(model));
            }
            None => self.note_error(session_id, "unknown model"),
        }
    }

    fn set_reasoning_effort(&self, session_id: &str, effort: &str) {
        let Some(thread) = self.session_thread(session_id) else {
            return self.note_error(session_id, "unknown session");
        };
        let effort = match effort {
            "high" => ReasoningEffort::High,
            "max" => ReasoningEffort::Max,
            _ => {
                return self
                    .note_error(session_id, "set_reasoning_effort requires effort: high|max");
            }
        };
        thread.with_mut(|t| t.set_reasoning_effort(effort));
    }

    fn set_approval_mode(&self, session_id: &str, mode: &str) {
        let Some(thread) = self.session_thread(session_id) else {
            return self.note_error(session_id, "unknown session");
        };
        // Parse strictly: an unparseable mode must not settle on the default
        // (a silent no-op beats a chip bounce-back on a projected mutation).
        let Ok(mode) = serde_json::from_value::<PermissionMode>(Value::String(mode.to_string()))
        else {
            return self.note_error(session_id, &format!("unknown approval mode: {mode}"));
        };
        thread.with_mut(|t| t.set_permission_mode(mode));
    }

    fn set_cwd(&self, session_id: &str, cwd: &str) {
        let Some(thread) = self.session_thread(session_id) else {
            return self.note_error(session_id, "unknown session");
        };
        thread.with_mut(|t| {
            // Two distinct semantics, deliberately split:
            // - Project binding is initial-only: a not-yet-interacted
            //   thread adopts the directory as its project (the
            //   `has_interacted` guard in `set_project` is correct for
            //   binding — a conversation's project never re-binds).
            // - The working-directory switch applies at ANY interaction
            //   state, through the same per-call cwd machinery the model's
            //   tools use: sticky advance + a durable `cwd_change` entry —
            //   never the header cwd.
            t.set_project(cwd.into());
            t.set_cwd(cwd.into());
        });
    }

    fn append_ui_note(&self, session_id: &str, kind: &str, data: Value) {
        let Some(thread) = self.session_thread(session_id) else {
            return self.note_error(session_id, "unknown session");
        };
        let kind = match kind {
            "error" => manox_agent::db::UiNoteKind::Error,
            "notice" => manox_agent::db::UiNoteKind::Notice,
            "plan_review" => manox_agent::db::UiNoteKind::PlanReview,
            _ => return self.note_error(session_id, "unknown ui note kind"),
        };
        thread.with_mut(|t| {
            t.append_ui_note(manox_agent::db::UiNoteRecord { kind, data });
        });
    }

    fn append_user_message(
        &self,
        session_id: &str,
        text: String,
        images: Vec<manox_protocol::ImageAttachment>,
    ) {
        let Some(thread) = self.session_thread(session_id) else {
            return self.note_error(session_id, "unknown session");
        };
        let images: Vec<(String, String)> = images
            .into_iter()
            .map(|i| (base64_bytes::encode(&i.data), i.mime_type))
            .collect();
        thread.with_mut(|t| {
            let ui = manox_agent::MessageUiMetadata {
                model_id: t.model().map(|m| m.id.clone()),
                approval_mode: Some(t.permission_mode().as_i64()),
                ..Default::default()
            };
            let content = to_message_content(text, images);
            t.insert_user_message_with_content_and_ui_metadata(content, Some(ui));
        });
    }

    fn compact(&self, session_id: &str, instructions: Option<String>) {
        let Some(thread) = self.session_thread(session_id) else {
            return self.note_error(session_id, "unknown session");
        };
        thread.with_mut(|t| t.compact(instructions));
    }

    fn plan_seed(&self, session_id: &str, plan_file: &str) {
        let Some(thread) = self.session_thread(session_id) else {
            return self.note_error(session_id, "unknown session");
        };
        let plan_file = plan_file.to_string();
        let lang = thread.read(|t| t.agent_language());
        let seed_text =
            match manox_agent::collaboration_mode::render_plan_mode_approved(lang, &plan_file) {
                Ok(text) => text,
                Err(e) => {
                    thread.handle_notice(BackendNotice::Event(Box::new(ThreadEvent::Error(e))));
                    return;
                }
            };
        thread.with_mut(|t| {
            let ui = MessageUiMetadata {
                model_id: t.model().map(|m| m.id.clone()),
                approval_mode: Some(t.permission_mode().as_i64()),
                author: Some(t.self_author()),
                ..Default::default()
            };
            t.seed_plan_execution(plan_file, seed_text, Some(ui));
        });
    }

    fn goal(
        &self,
        session_id: &str,
        action: &str,
        objective: Option<String>,
        budget: Option<u64>,
        max_rounds: Option<u64>,
    ) {
        let Some(thread) = self.session_thread(session_id) else {
            return self.note_error(session_id, "unknown session");
        };
        let objective = objective.unwrap_or_default();
        let actor = manox_agent::db::GoalActor::User;
        let result = thread.with_mut(|t| match action {
            "create" => t.set_goal(objective),
            "edit" => t.edit_goal(objective, budget, max_rounds, actor),
            "replace" => t.replace_goal(objective, budget, max_rounds, actor),
            "clear" => t.clear_goal(actor),
            "pause" => t.set_goal_status(
                manox_agent::goal::GoalStatus::Paused,
                Some(manox_agent::goal::GoalBlockReason {
                    code: "user-paused".into(),
                    message: "paused by user".into(),
                }),
                actor,
            ),
            "resume" => t.set_goal_status(manox_agent::goal::GoalStatus::Active, None, actor),
            _ => Ok(()),
        });
        if let Err(e) = result {
            self.note_error(session_id, &e.to_string());
        }
    }

    fn archive_thread(&self, owner: &str, session_id: &str, archived: bool) {
        if archived {
            self.dispose_session(owner, session_id);
            manox_agent::thread_store::global().with_mut(|s| s.archive_thread(session_id, true));
        } else {
            manox_agent::thread_store::global().with_mut(|s| s.archive_thread(session_id, false));
        }
    }

    /// GW3 (§D.4): mint the stable delivery identity for one adjudication —
    /// `dlv-{session}-{n}`, n counting the session's deliveries. Per-session
    /// (not per-server) so identical scripts on the two
    /// `dual_path_transport_consistency` transports mint identical ids after
    /// session-id normalization; the session prefix keeps the id unique
    /// gateway-wide (the `pending_deliveries` registry key).
    fn next_delivery_id(&self, session_id: &str) -> String {
        let n = {
            let mut seq = self.delivery_seq.lock();
            let entry = seq.entry(session_id.to_string()).or_insert(0);
            *entry += 1;
            *entry
        };
        format!("dlv-{session_id}-{n}")
    }

    /// GW3 (§D.4): withdraw `client_id`'s pending delivery — flips its
    /// cancel token, which the delivery's reply waiter folds into the
    /// funnel as an expired reply (the waterfall then converges fail-closed
    /// through the existing expire path). `false` when the delivery already
    /// settled, never existed, or targeted other clients only.
    fn cancel_delivery(&self, client_id: &str, delivery_id: &str) -> bool {
        let deliveries = self.pending_deliveries.lock();
        match deliveries
            .get(delivery_id)
            .and_then(|tokens| tokens.get(client_id))
        {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }
}

// ── ServerCall routing (β-3b: Approve / AskUserQuestion / PlanVerdict). ─────
async fn route_call(inner: &Arc<AgentServerInner>, session_id: &str, call: ServerCall) {
    let kind = hook_kind_for(&call);
    // GW3 (§D.4): the gateway is the SINGLE stamping point for delivery
    // identity — translate/pump construct the trio with an empty
    // `delivery_id` (they are pure), and every adjudication passes through
    // here before hitting the wire. Directed capability calls carry none.
    let delivery_id = match &call {
        ServerCall::Approve { .. }
        | ServerCall::PlanVerdict { .. }
        | ServerCall::AskUserQuestion { .. } => Some(inner.next_delivery_id(session_id)),
        _ => None,
    };
    let call = match &delivery_id {
        Some(d) => with_delivery_id(call, d),
        None => call,
    };
    // Per-kind context needed to apply the reply, extracted before `call`
    // moves into the Request envelope.
    let ctx = match &call {
        ServerCall::Approve { auth_id, .. } => ReplyCtx::Approve {
            auth_id: auth_id.clone(),
        },
        ServerCall::AskUserQuestion { auth_id, .. } => ReplyCtx::AskUser {
            auth_id: auth_id.clone(),
        },
        ServerCall::PlanVerdict { plan_file, .. } => ReplyCtx::PlanVerdict {
            plan_file: plan_file.clone(),
        },
        _ => ReplyCtx::Other, // β-3b-ii: BrowserOp/ClipboardRead/OpenExternal (capability seam).
    };
    // §D.4: adjudication kinds (Approve / AskUserQuestion / PlanVerdict)
    // fan out to EVERY owner that declared the capability — all must answer
    // next to proceed, any rejection (or per-delivery timeout) settles
    // fail-closed (see [`crate::waterfall`]). Capability calls
    // (BrowserOp/...) stay single-target.
    let adjudication = matches!(
        ctx,
        ReplyCtx::Approve { .. } | ReplyCtx::AskUser { .. } | ReplyCtx::PlanVerdict { .. }
    );

    // Register a waiter per eligible owner under the clients lock (brief —
    // register is synchronous).
    let targets = {
        let owners = inner.owners(session_id);
        let clients = inner.clients.lock();
        owners
            .iter()
            .filter(|cid| clients.get(*cid).is_some_and(|e| e.hello.can(kind)))
            .filter_map(|cid| {
                let entry = clients.get(cid).expect("just checked");
                // Deterministic MsgId per kind so a client without bridge
                // state can correlate its Reply: Approve/AskUser echo the
                // auth_id the card carries; PlanVerdict uses the session id
                // (one pending review per session); capability calls mint a
                // fresh opaque id.
                let id = match &ctx {
                    ReplyCtx::Approve { auth_id } | ReplyCtx::AskUser { auth_id } => {
                        MsgId::new(auth_id.clone())
                    }
                    ReplyCtx::PlanVerdict { .. } => MsgId::new(session_id.to_string()),
                    ReplyCtx::Other => inner.next_call_id(),
                };
                // GW2: `register` refuses a duplicate MsgId (the first
                // waiter stays live). A duplicate here means the same
                // adjudication is being routed twice to one peer — a routing
                // bug (double pump / duplicated owner row). Fail closed for
                // this target: skip it; if every target is skipped the
                // empty-targets path below denies/expires the call.
                match entry.peer.register(id.clone()) {
                    Some(rx) => Some((cid.clone(), entry.conn.clone(), rx, id)),
                    None => {
                        tracing::error!(
                            session = %session_id,
                            client = %cid,
                            msg_id = %id.0,
                            "duplicate ServerCall registration for the same MsgId \
                             (double-routed adjudication); skipping target fail-closed"
                        );
                        None
                    }
                }
            })
            .collect::<Vec<_>>()
    };
    if targets.is_empty() {
        fail_closed(inner, session_id, &ctx);
        return;
    }

    if adjudication {
        route_waterfall(
            inner,
            session_id,
            ctx,
            call,
            targets,
            delivery_id.expect("stamped above for exactly the adjudication kinds"),
        )
        .await;
        return;
    }

    let (conn, rx, id) = {
        let (_, conn, rx, id) = targets.into_iter().next().expect("non-empty checked");
        (conn, rx, id)
    };
    conn.send_to_client(FromServer::Request { id, call });
    let outcome = match tokio::time::timeout(CALL_TIMEOUT, rx.recv()).await {
        Ok(Ok(o)) => o,
        _ => Err(RpcError::new(-1, "capability call timed out or cancelled")
            .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL)),
    };
    if outcome.is_err() {
        // Plan §5.2: a timed-out / errored call must surface the reason,
        // mirroring the no-owner fail-closed path.
        inner.note_error(session_id, "capability call timed out or cancelled");
    }
    apply_reply(inner, session_id, ctx, outcome, None);
}

/// §D.4 fan-out/fan-in: deliver the adjudication Request to every target,
/// funnel their replies (each bounded by [`CALL_TIMEOUT`]) into a
/// [`crate::waterfall::Waterfall`], and apply the SETTLING reply's payload
/// (the first rejection, or the final next). Recipients that never answered
/// by settlement are owed a cancel in a future wire addition; until then
/// the `pending_auth` projection is the truth clients reconcile against.
/// One adjudication delivery: (client id, connection, reply receiver,
/// deterministic MsgId).
type AdjudicationTarget = (
    String,
    Arc<dyn RpcConnection>,
    async_channel::Receiver<Result<Value, RpcError>>,
    MsgId,
);

/// GW3: unregister a delivery when its waterfall settles — and cancel the
/// tokens of any recipient that never answered, so their reply-waiter tasks
/// exit promptly instead of parking on the 300s timeout. Drop-based (the
/// [`PumpExitGuard`] pattern): a pump aborted mid-waterfall still unregisters.
struct DeliveryGuard<'a> {
    inner: &'a AgentServerInner,
    delivery_id: String,
}

impl Drop for DeliveryGuard<'_> {
    fn drop(&mut self) {
        let mut deliveries = self.inner.pending_deliveries.lock();
        if let Some(tokens) = deliveries.remove(&self.delivery_id) {
            for token in tokens.values() {
                token.cancel();
            }
        }
    }
}

/// GW3: rebuild one of the three adjudication variants with the gateway-
/// minted `delivery_id` (the single stamping point — see `route_call`);
/// capability calls pass through untouched (they carry no delivery identity).
fn with_delivery_id(call: ServerCall, delivery_id: &str) -> ServerCall {
    match call {
        ServerCall::Approve {
            session_id,
            auth_id,
            tool_name,
            summary,
            input,
            ..
        } => ServerCall::Approve {
            delivery_id: delivery_id.to_string(),
            session_id,
            auth_id,
            tool_name,
            summary,
            input,
        },
        ServerCall::PlanVerdict {
            session_id,
            plan_file,
            title,
            content,
            ..
        } => ServerCall::PlanVerdict {
            delivery_id: delivery_id.to_string(),
            session_id,
            plan_file,
            title,
            content,
        },
        ServerCall::AskUserQuestion {
            session_id,
            auth_id,
            input,
            ..
        } => ServerCall::AskUserQuestion {
            delivery_id: delivery_id.to_string(),
            session_id,
            auth_id,
            input,
        },
        other => other,
    }
}

async fn route_waterfall(
    inner: &Arc<AgentServerInner>,
    session_id: &str,
    ctx: ReplyCtx,
    call: ServerCall,
    targets: Vec<AdjudicationTarget>,
    delivery_id: String,
) {
    // (client id, delivery expired, reply outcome): `expired` separates a
    // delivery that timed out / closed / was WITHDRAWN (GW3
    // `CancelDelivery`) from an explicit client rejection so the GW9
    // PlanVerdict convergence can name the cause.
    let (funnel_tx, mut funnel_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, bool, Result<Value, RpcError>)>();
    let mut waterfall = crate::waterfall::Waterfall::new(session_id.to_string(), {
        let mut ids = targets
            .iter()
            .map(|(cid, ..)| cid.clone())
            .collect::<Vec<_>>();
        ids.sort();
        ids
    });
    // GW3: one cancel token per recipient, registered under the delivery id
    // for the fan-out window. A `CancelDelivery` from a recipient flips its
    // token; the waiter below folds that into the funnel as an expired
    // reply, converging the waterfall fail-closed through the SAME path a
    // timeout takes (no parallel cancellation semantics).
    let tokens: HashMap<String, tokio_util::sync::CancellationToken> = targets
        .iter()
        .map(|(cid, ..)| (cid.clone(), tokio_util::sync::CancellationToken::new()))
        .collect();
    inner
        .pending_deliveries
        .lock()
        .insert(delivery_id.clone(), tokens.clone());
    let _delivery_guard = DeliveryGuard {
        inner,
        delivery_id: delivery_id.clone(),
    };
    for (cid, conn, rx, id) in targets {
        conn.send_to_client(FromServer::Request {
            id,
            call: call.clone(),
        });
        let tx = funnel_tx.clone();
        let token = tokens
            .get(&cid)
            .expect("every target registered a token")
            .clone();
        manox_agent::runtime::handle().spawn(async move {
            let (expired, outcome) = tokio::select! {
                _ = token.cancelled() => (
                    true,
                    Err(RpcError::new(-1, "delivery withdrawn by client (cancelDelivery)").with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL)),
                ),
                replied = tokio::time::timeout(CALL_TIMEOUT, rx.recv()) => match replied {
                    Ok(Ok(o)) => (false, o),
                    _ => (true, Err(RpcError::new(-1, "adjudication reply timed out").with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL))),
                },
            };
            let _ = tx.send((cid, expired, outcome));
        });
    }
    drop(funnel_tx);
    let mut settled: Option<Result<Value, RpcError>> = None;
    // The settling delivery when it settled the waterfall AGAINST the call:
    // (client id, expired).
    let mut settled_by: Option<(String, bool)> = None;
    while let Some((cid, expired, outcome)) = funnel_rx.recv().await {
        let next = outcome.is_ok();
        if waterfall.reply(&cid, next).is_some() {
            if !next {
                settled_by = Some((cid, expired));
            }
            settled = Some(outcome);
            break;
        }
    }
    let outcome = settled.unwrap_or_else(|| {
        Err(
            RpcError::new(-1, "adjudication unsettled (all deliveries expired)")
                .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL),
        )
    });
    // GW9: a PlanVerdict that settles against the call converges through
    // `apply_plan_verdict`'s fail-closed arm, which sends the kind-specific
    // Error note (naming who rejected / expired) — the generic note below is
    // skipped for it to keep exactly one Error per rejection.
    let verdict_failure = (outcome.is_err() && matches!(ctx, ReplyCtx::PlanVerdict { .. })).then(
        || match &settled_by {
            Some((cid, true)) => format!("plan verdict expired: no reply from {cid}"),
            Some((cid, false)) => format!("plan verdict rejected by {cid}"),
            None => "plan verdict unsettled: every delivery expired".to_string(),
        },
    );
    if outcome.is_err() && verdict_failure.is_none() {
        inner.note_error(session_id, "adjudication rejected or timed out");
    }
    apply_reply(inner, session_id, ctx, outcome, verdict_failure);
}

/// Per-`ServerCall` context carried out of the lock to apply the reply.
enum ReplyCtx {
    Approve { auth_id: String },
    AskUser { auth_id: String },
    PlanVerdict { plan_file: String },
    Other,
}

fn fail_closed(inner: &Arc<AgentServerInner>, session_id: &str, ctx: &ReplyCtx) {
    match ctx {
        ReplyCtx::Approve { auth_id } => {
            respond_auth_fail_closed(inner, session_id, auth_id.clone())
        }
        ReplyCtx::AskUser { auth_id } => {
            respond_ask_fail_closed(inner, session_id, auth_id.clone())
        }
        // GW9: an unreviewable plan is a fail-closed rejection like any
        // other — converge the pending-review state instead of leaving the
        // session parked forever (the bare Error note was the pre-fix
        // behavior; it cleared nothing).
        ReplyCtx::PlanVerdict { .. } => converge_plan_rejected(
            inner,
            session_id,
            "no client can review this plan".to_string(),
        ),
        ReplyCtx::Other => {}
    }
}

fn apply_reply(
    inner: &Arc<AgentServerInner>,
    session_id: &str,
    ctx: ReplyCtx,
    outcome: Result<Value, RpcError>,
    verdict_failure: Option<String>,
) {
    match ctx {
        ReplyCtx::Approve { auth_id } => apply_approve_reply(inner, session_id, auth_id, outcome),
        ReplyCtx::AskUser { auth_id } => apply_ask_reply(inner, session_id, auth_id, outcome),
        ReplyCtx::PlanVerdict { plan_file } => {
            apply_plan_verdict(inner, session_id, plan_file, outcome, verdict_failure)
        }
        ReplyCtx::Other => {}
    }
}

fn respond_auth_fail_closed(inner: &Arc<AgentServerInner>, session_id: &str, auth_id: String) {
    if let Some(thread) = inner.session_thread(session_id) {
        thread.with_mut(|t| {
            t.respond_authorization(
                &auth_id,
                manox_agent::permission::ToolAuthorizationResponse::Decision(
                    manox_agent::permission::PermissionDecision::Deny,
                ),
            )
        });
    }
    inner.note_error(session_id, "no client can answer this approval");
}

fn apply_approve_reply(
    inner: &Arc<AgentServerInner>,
    session_id: &str,
    auth_id: String,
    outcome: Result<Value, RpcError>,
) {
    let allow = match outcome {
        Ok(v) => v.get("allow").and_then(Value::as_bool).unwrap_or(false),
        Err(_) => false,
    };
    let response = if allow {
        manox_agent::permission::ToolAuthorizationResponse::Decision(
            manox_agent::permission::PermissionDecision::AllowOnce,
        )
    } else {
        manox_agent::permission::ToolAuthorizationResponse::Decision(
            manox_agent::permission::PermissionDecision::Deny,
        )
    };
    if let Some(thread) = inner.session_thread(session_id) {
        thread.with_mut(|t| t.respond_authorization(&auth_id, response));
    }
}

fn apply_ask_reply(
    inner: &Arc<AgentServerInner>,
    session_id: &str,
    auth_id: String,
    outcome: Result<Value, RpcError>,
) {
    let response = match outcome {
        Ok(v) => manox_agent::permission::ToolAuthorizationResponse::AskUserQuestion {
            answers: v
                .get("answers")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|p| {
                            let q = p.get(0).and_then(Value::as_str)?.to_string();
                            let a = p.get(1).and_then(Value::as_str).unwrap_or("").to_string();
                            Some((q, a))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            response: v.get("response").and_then(Value::as_str).map(String::from),
        },
        Err(_) => manox_agent::permission::ToolAuthorizationResponse::AskUserQuestion {
            answers: Vec::new(),
            response: None,
        },
    };
    if let Some(thread) = inner.session_thread(session_id) {
        thread.with_mut(|t| t.respond_authorization(&auth_id, response));
    }
}

fn respond_ask_fail_closed(inner: &Arc<AgentServerInner>, session_id: &str, auth_id: String) {
    if let Some(thread) = inner.session_thread(session_id) {
        thread.with_mut(|t| {
            t.respond_authorization(
                &auth_id,
                manox_agent::permission::ToolAuthorizationResponse::AskUserQuestion {
                    answers: Vec::new(),
                    response: None,
                },
            )
        });
    }
    inner.note_error(session_id, "no client can answer this question");
}

fn apply_plan_verdict(
    inner: &Arc<AgentServerInner>,
    session_id: &str,
    plan_file: String,
    outcome: Result<Value, RpcError>,
    verdict_failure: Option<String>,
) {
    let choice = match outcome {
        Ok(v) => v
            .get("choice")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        // GW9: a rejected / expired verdict CONVERGES — the pre-fix early
        // return cleared nothing, so `plan_review_pending` (kernel) and
        // `pending_plan` (store) stayed set forever, the engine stayed
        // parked, and the session was permanently "plan pending review"
        // (late replies had nowhere to land). Fail-closed semantics are
        // kept: the plan does NOT execute.
        Err(_) => {
            converge_plan_rejected(
                inner,
                session_id,
                verdict_failure.unwrap_or_else(|| "plan verdict rejected or expired".to_string()),
            );
            return;
        }
    };
    let Some(thread) = inner.session_thread(session_id) else {
        return;
    };
    // Consume the pending-review flag on every verdict; refine leaves plan
    // mode on (the user can re-edit) without seeding execution.
    if choice == "refine" {
        thread.with_mut(|t| t.set_plan_review_pending(false));
        return;
    }
    let compact = choice == "execute_compact";
    let lang = thread.read(|t| t.agent_language());
    let seed_text =
        match manox_agent::collaboration_mode::render_plan_mode_approved(lang, &plan_file) {
            Ok(text) => text,
            Err(e) => {
                thread.handle_notice(BackendNotice::Event(Box::new(ThreadEvent::Error(e))));
                return;
            }
        };
    let compact_instructions = compact
        .then(|| manox_agent::collaboration_mode::plan_compact_instructions(lang, &plan_file));
    thread.with_mut(|t| {
        t.set_plan_review_pending(false);
        let ui = MessageUiMetadata {
            model_id: t.model().map(|m| m.id.clone()),
            approval_mode: Some(t.permission_mode().as_i64()),
            author: Some(t.self_author()),
            ..Default::default()
        };
        t.approve_plan(compact, compact_instructions, seed_text, Some(ui));
    });
}

/// Converge a PlanVerdict that will never be answered — rejected, expired,
/// or unreviewable (GW9). Fail-closed: the plan does NOT execute; every
/// pending-review plane is cleared so the session stays operable instead of
/// parking forever, and the parked turn is cancelled so its `TurnFinished`
/// settles normally through the pump. `message` names the cause (who
/// rejected / which delivery expired) and rides an Error note to the owners.
///
/// Extracted as a free function so the timeout path (a 300s `CALL_TIMEOUT`
/// wait, unreachable inside a unit test) is testable by direct call.
fn converge_plan_rejected(inner: &Arc<AgentServerInner>, session_id: &str, message: String) {
    if let Some(thread) = inner.session_thread(session_id) {
        thread.with_mut(|t| {
            // Kernel flag: no stale review card re-surfaces on restart.
            t.set_plan_review_pending(false);
            // Cancel the parked turn so TurnFinished arrives and the pump's
            // settlement path runs (running=false, queued-submit drain).
            t.cancel();
        });
    }
    // Store flag: the sidebar badge / ListThreads snapshot clears.
    manox_agent::thread_store::global().with_mut(|s| s.mark_pending_plan(session_id, false));
    // §D.5 status delta: every connection's pending_plan mirror clears.
    inner.broadcast_host(host_status(session_id, |f| {
        f.pending_plan = Some(false);
    }));
    // K3: the decision entry lands with the journal work.
    inner.note_error(session_id, &message);
}

/// Route a capability `ServerCall` (BrowserOp/ClipboardRead/OpenExternal) to the
/// owning ∩ capable client and return its Reply outcome. Unlike `route_call`,
/// the reply is returned to the kernel (the engine's capability call awaits
/// it), not applied internally — there is no engine-side auth/verdict state to
/// mutate.
async fn route_capability_call(
    inner: &Arc<AgentServerInner>,
    session_id: &str,
    call: ServerCall,
) -> Result<Value, RpcError> {
    let kind = hook_kind_for(&call);
    let id = inner.next_call_id();
    let target = {
        let owners = inner.owners(session_id);
        let clients = inner.clients.lock();
        owners
            .iter()
            .find(|cid| clients.get(*cid).is_some_and(|e| e.hello.can(kind)))
            .and_then(|cid| {
                let entry = clients.get(cid).expect("just checked");
                // GW2: a fresh `call-N` id cannot collide unless a previous
                // waiter for it is still registered; refuse the delivery
                // fail-closed rather than clobbering the earlier waiter.
                match entry.peer.register(id.clone()) {
                    Some(rx) => Some((entry.conn.clone(), rx)),
                    None => {
                        tracing::error!(
                            session = %session_id,
                            client = %cid,
                            msg_id = %id.0,
                            "duplicate capability-call registration for the same MsgId; \
                             failing closed"
                        );
                        None
                    }
                }
            })
    };
    let Some((conn, rx)) = target else {
        return Err(
            RpcError::new(-1, "no client can answer this capability call")
                .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL),
        );
    };
    conn.send_to_client(FromServer::Request { id, call });
    match tokio::time::timeout(CALL_TIMEOUT, rx.recv()).await {
        Ok(Ok(o)) => o,
        _ => Err(RpcError::new(-1, "capability call timed out or cancelled")
            .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL)),
    }
}

/// The AgentServer's `CapabilityClient` impl: the kernel's `browser_op` is
/// routed as a `ServerCall::BrowserOp` to the owning ∩ BrowserOp-capable
/// client; the reply (a serialized `BrowserReply`) is returned to the engine.
/// Registered as the provider in γ/δ (replacing the gpui BrowserHost); tested
/// in-process here.
pub struct AgentServerCapabilityClient(Arc<AgentServerInner>);

impl AgentServerCapabilityClient {
    /// Wrap an `AgentServer` so the kernel's `browser_op` routes to its clients.
    pub fn new(server: &AgentServer) -> Self {
        Self(server.0.clone())
    }
}
impl manox_agent::capability::CapabilityClient for AgentServerCapabilityClient {
    fn browser_op(
        &self,
        op: manox_agent::thread_engine::BrowserOp,
    ) -> futures::future::BoxFuture<'static, Result<manox_agent::thread_engine::BrowserReply, String>>
    {
        let inner = self.0.clone();
        Box::pin(async move {
            let session_id = manox_agent::capability::CURRENT_SESSION
                .try_with(|c| c.clone())
                .ok()
                .flatten()
                .ok_or_else(|| "no session context for browser op".to_string())?;
            let call = ServerCall::BrowserOp {
                session_id: session_id.clone(),
                op: serde_json::to_value(&op).map_err(|e| e.to_string())?,
            };
            let outcome = route_capability_call(&inner, &session_id, call).await;
            match outcome {
                Ok(v) => serde_json::from_value::<manox_agent::thread_engine::BrowserReply>(v)
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e.message),
            }
        })
    }
}

/// The settable subset of a `SessionStatus` delta (§D.5).
#[derive(Default)]
struct SessionStatusDelta {
    running: Option<bool>,
    errored: Option<bool>,
    unread: Option<bool>,
    pending_auth: Option<bool>,
    pending_plan: Option<bool>,
    background_work: Option<bool>,
}

/// Build a `SessionStatus` delta (§D.5): only the fields the closure sets
/// travel; clients merge monotonically (unread only rises until focus,
/// errored edge-set, running latest-wins).
fn host_status(session_id: &str, set: impl FnOnce(&mut SessionStatusDelta)) -> HostEvent {
    let mut d = SessionStatusDelta::default();
    set(&mut d);
    HostEvent::SessionStatus {
        session_id: session_id.to_string(),
        running: d.running,
        errored: d.errored,
        unread: d.unread,
        pending_auth: d.pending_auth,
        pending_plan: d.pending_plan,
        background_work: d.background_work,
    }
}

// ── Event pump. ─────────────────────────────────────────────────────────────
fn spawn_pump(
    inner: Arc<AgentServerInner>,
    session_id: String,
    thread: ThreadHandle,
    turn_active: Arc<AtomicBool>,
    pending_submits: Arc<StdMutex<Vec<QueuedSubmit>>>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    // Subscribe synchronously so the receiver is registered before any
    // broadcast (a subscribe inside the task can lose events fired before
    // the task is first polled).
    let rx = thread.subscribe();
    // GW2 observability: the live pump count is spawned minus finished; the
    // exit guard bumps `finished` even when the task is aborted (the abort
    // drops the future, running the guard's Drop).
    inner.pumps_spawned.fetch_add(1, Ordering::SeqCst);
    manox_agent::runtime::handle().spawn(async move {
        let _exit_guard = PumpExitGuard(Arc::clone(&inner));
        loop {
            // GW2: the pump is terminable — `ServerSession::stop_pump`
            // cancels this token (waking a pump parked in `recv`) and the
            // callers additionally abort the JoinHandle (covering a pump
            // parked inside a long `route_call` await below, which never
            // re-enters this select). Pre-fix the loop was a bare
            // `while let Ok(ev) = rx.recv().await`: the pump's own
            // `ThreadHandle` clone kept the unbounded subscription open
            // forever, so a "disposed" session's pump leaked process-wide
            // and a reopen spawned a SECOND pump on the same thread — every
            // `ToolCallAuthorization` was then routed twice, and the
            // duplicate `RpcPeer::register` auto-denied the approval (GW2).
            let ev = tokio::select! {
                _ = cancel.cancelled() => break,
                received = rx.recv() => match received {
                    Ok(ev) => ev,
                    // Every sender dropped: the thread is gone.
                    Err(_) => break,
                },
            };
            // Bookkeeping that mirrors the legacy host pump: thread-store list
            // flags and the queued-follow-up drain. T10 (§D.6): no v1 notes
            // are emitted here — translate only carries adjudication calls.
            match &*ev {
                ThreadEvent::TurnStarted => {
                    turn_active.store(true, Ordering::SeqCst);
                    let id = session_id.clone();
                    manox_agent::thread_store::global().with_mut(|s| {
                        s.mark_running(&id);
                        s.set_errored(&id, false);
                    });
                    inner.broadcast_host(host_status(&session_id, |f| {
                        f.running = Some(true);
                        f.errored = Some(false);
                    }));
                }
                ThreadEvent::TurnFinished {
                    cancelled, failed, ..
                } => {
                    turn_active.store(false, Ordering::SeqCst);
                    let id = session_id.clone();
                    // GW5: no store-side unread mirror write — unread is
                    // client-owned. (Pre-fix this arm wrote
                    // `set_unread(id, true)` when the server's single-slot
                    // `focused` mirror named another session; the slot is
                    // gone and clients derive unread from the delta below.)
                    manox_agent::thread_store::global().with_mut(|s| {
                        s.mark_idle(&id);
                        s.mark_pending_auth(&id, false);
                        s.mark_pending_plan(&id, false);
                        if !*failed {
                            s.set_errored(&id, false);
                        }
                    });
                    inner.broadcast_host(host_status(&session_id, |f| {
                        f.running = Some(false);
                        f.pending_auth = Some(false);
                        f.pending_plan = Some(false);
                        // GW5: the settle edge ALWAYS raises unread — the
                        // server cannot know which of N clients is looking
                        // (the single-slot focus mirror pretended it could,
                        // and the desktop never even sent FocusThread).
                        // Clients clear their own badge locally on focus.
                        f.unread = Some(true);
                    }));
                    if !*cancelled {
                        let drained = pending_submits
                            .lock()
                            .unwrap()
                            .drain(..)
                            .collect::<Vec<_>>();
                        let drained_any = !drained.is_empty();
                        let mut batch_origin: Option<String> = None;
                        if drained_any {
                            thread.with_mut(|t| {
                                for q in drained {
                                    if q.origin.is_some() {
                                        batch_origin = q.origin.clone();
                                    }
                                    let content = to_message_content(q.text, q.images);
                                    t.insert_user_message_with_content_and_ui_metadata(
                                        content,
                                        Some(q.ui),
                                    );
                                }
                            });
                        }
                        thread.with_mut(|t| {
                            if drained_any || t.has_pending_prompts() {
                                t.set_pending_turn_origin(batch_origin);
                                t.run_turn();
                            }
                        });
                    }
                    // Deferred reap (GW2 follow-up): an orphaned session
                    // (last owner detached or disconnected mid-turn) kept
                    // its entry and pump through this settle so the
                    // bookkeeping above could converge the store flags;
                    // reap it now unless the drain just started a follow-up
                    // turn (`is_running` is the facade's synchronous truth —
                    // run_turn sets it, the settle path cleared it before
                    // this event was pushed). Dropping the entry runs
                    // ServerSession::Drop → stop_pump; this loop exits on
                    // the cancelled token at its next select.
                    if inner.owners(&session_id).is_empty() && !thread.read(|t| t.is_running()) {
                        inner.sessions.lock().remove(&session_id);
                    }
                }
                ThreadEvent::ToolCallAuthorization { .. } => {
                    let id = session_id.clone();
                    manox_agent::thread_store::global()
                        .with_mut(|s| s.mark_pending_auth(&id, true));
                    inner.broadcast_host(host_status(&session_id, |f| {
                        f.pending_auth = Some(true);
                    }));
                }
                ThreadEvent::Error(_) => {
                    let id = session_id.clone();
                    manox_agent::thread_store::global().with_mut(|s| {
                        s.set_errored(&id, true);
                        s.mark_pending_plan(&id, false);
                        s.mark_background_work(&id, false);
                    });
                    inner.broadcast_host(host_status(&session_id, |f| {
                        f.errored = Some(true);
                        f.running = Some(false);
                    }));
                }
                ThreadEvent::PlanReady { plan_file, title } => {
                    let id = session_id.clone();
                    manox_agent::thread_store::global()
                        .with_mut(|s| s.mark_pending_plan(&id, true));
                    thread.with_mut(|t| t.set_plan_review_pending(true));
                    // §D.5: the pending_plan TRUE edge broadcasts like the
                    // pending_auth one — without it client mirrors only ever
                    // see the false edge (GW1 delivery finding) and a list
                    // badge cannot rise until the next explicit ListThreads.
                    inner.broadcast_host(host_status(&session_id, |f| {
                        f.pending_plan = Some(true);
                    }));
                    // β-3b: initiate PlanVerdict (carries the plan body) and
                    // skip translate's bare PlanReady note — the call is the
                    // actionable review card; the bare note would duplicate.
                    // GW3: `delivery_id` is stamped at the single routing
                    // point (`route_call`), never at construction.
                    route_call(
                        &inner,
                        &session_id,
                        ServerCall::PlanVerdict {
                            delivery_id: String::new(),
                            session_id: session_id.clone(),
                            plan_file: plan_file.clone(),
                            title: title.clone(),
                            content: std::fs::read_to_string(plan_file).ok(),
                        },
                    )
                    .await;
                    continue;
                }
                ThreadEvent::BackgroundTaskUpdated { .. } => {
                    let id = session_id.clone();
                    // Computed OUTSIDE the store write lock (it takes the
                    // background-task registry lock — nesting it inside was a
                    // U8-class lock-order hazard) and broadcast: §D.5 lists
                    // background work as a SessionStatus delta, and client
                    // mirrors previously only learned it at the next
                    // ListThreads.
                    let active = manox_agent::background_task::thread_has_running_tasks(&id);
                    manox_agent::thread_store::global()
                        .with_mut(|s| s.mark_background_work(&id, active));
                    inner.broadcast_host(host_status(&session_id, |f| {
                        f.background_work = Some(active);
                    }));
                }
                _ => {}
            }
            match translate(&ev, &session_id) {
                Translated::Note(note) => inner.route_note(&session_id, note),
                Translated::Call(call) => route_call(&inner, &session_id, call).await,
                Translated::Skip => {}
            }
        }
    })
}

/// Map a `ServerCall` to the `HookKind` its answerer must declare.
fn hook_kind_for(call: &ServerCall) -> HookKind {
    match call {
        ServerCall::Approve { .. } => HookKind::Approve,
        ServerCall::PlanVerdict { .. } => HookKind::PlanVerdict,
        ServerCall::AskUserQuestion { .. } => HookKind::AskUserQuestion,
        ServerCall::BrowserOp { .. } => HookKind::BrowserOp,
        ServerCall::ClipboardRead { .. } => HookKind::ClipboardRead,
        ServerCall::OpenExternal { .. } => HookKind::OpenExternal,
    }
}

/// Build kernel `MessageContent` from a submit/steer payload: text plus
/// base64-encoded image blocks.
fn to_message_content(text: String, images: Vec<(String, String)>) -> Vec<MessageContent> {
    let mut content = Vec::new();
    if !text.trim().is_empty() {
        content.push(MessageContent::Text(text));
    }
    content
        .into_iter()
        .chain(
            images
                .into_iter()
                .map(|(data, mime_type)| MessageContent::Image { data, mime_type }),
        )
        .collect()
}

/// Split a `/name args` invocation; an empty name is not a slash turn.
fn parse_slash(text: &str) -> Option<(String, String)> {
    let body = text.trim_start().strip_prefix('/')?;
    let (name, args) = body.split_once(char::is_whitespace).unwrap_or((body, ""));
    let name = name.trim();
    (!name.is_empty()).then(|| (name.to_string(), args.trim_start().to_string()))
}

fn deduped_models(models: Vec<manox_harness::types::Model>) -> Vec<manox_harness::types::Model> {
    let mut seen = std::collections::HashSet::new();
    models
        .into_iter()
        .filter(|m| seen.insert((m.provider.clone(), m.id.clone())))
        .collect()
}

fn model_to_wire(model: &manox_harness::types::Model) -> ModelInfo {
    ModelInfo {
        id: model.id.clone(),
        name: manox_agent::provider_glue::display_name(model),
        provider: model.provider.clone(),
        provider_name: Some(manox_agent::provider_glue::display_provider_name(model)),
        api: model.api.clone(),
        context_window: model.context_window as u32,
        max_tokens: Some(model.max_tokens as u32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // Reuse the session module's serialized test scaffolding so this suite
    // never races the process-wide runtime / thread-store / HOME globals.
    use crate::test_support::{hermetic_home, init_globals, lock_globals};
    use manox_protocol::in_process_pair;

    /// K5 probe record: one `run_with_origin` call — (prompt, origin,
    /// accepted entry).
    type OriginRun = (String, Option<String>, Option<String>);

    /// A scripted engine: records runs/steers/authorizations and lets a test
    /// inject `BackendNotice`s to drive the pump.
    struct FakeEngine {
        runs: StdMutex<Vec<String>>,
        steer_calls: StdMutex<Vec<String>>,
        cwds: StdMutex<Vec<PathBuf>>,
        /// Model ids the server pushed through `ThreadEngine::set_model`
        /// (T10: the v1 ThreadInfo mirror is gone — the engine-side wiring
        /// is what the server half of a model switch can be held to; the
        /// real engine journals it and the P face publishes the delta).
        model_switches: StdMutex<Vec<String>>,
        notices: tokio::sync::mpsc::UnboundedSender<BackendNotice>,
        auth_responses: StdMutex<Vec<(String, manox_agent::permission::ToolAuthorizationResponse)>>,
        pending_auth: StdMutex<Vec<(String, manox_agent::permission::PendingAuthMeta)>>,
        /// GW9 probe: every `set_plan_review_pending` the facade forwards,
        /// in order. The trait default is a silent no-op, so without this
        /// recorder the kernel-side pending-review flag is unobservable in
        /// gateway tests (the real engine persists it to a sidecar).
        plan_review_flags: StdMutex<Vec<bool>>,
        /// GW9 probe: every `approve_plan` seed text — a rejected/expired
        /// verdict must never execute the plan, and this is the execution
        /// observable (the trait default is a silent no-op).
        plan_approvals: StdMutex<Vec<String>>,
        /// Journal read-seam override (§C.3): tests append `JournalEvent`s
        /// through this sender and seed the snapshot read directly, so
        /// follow streams / PageHistory / the fold are exercised without a
        /// live PiEngine actor.
        journal_tx: tokio::sync::broadcast::Sender<manox_agent::engine::JournalFeed>,
        journal_data: StdMutex<manox_agent::engine::JournalSnapshotData>,
        /// K5 probes: every `persist_user_submission` call — (text, image
        /// count, origin) in order — and the scripted reply (`None` =
        /// Ok(None) drain fallback; `Err(())` = storage failure). The trait
        /// default would silently answer Ok(None), hiding the accept-time
        /// persist contract from gateway tests.
        persist_calls: StdMutex<Vec<(String, usize, Option<String>)>>,
        persist_reply: StdMutex<Option<Result<Option<String>, ()>>>,
        /// K5 probe: run_with_origin records — (prompt, origin, accepted
        /// entry) — the pin-arming observable.
        origin_runs: StdMutex<Vec<OriginRun>>,
        /// GW6: when false, `journal_snapshot` answers with a oneshot whose
        /// sender is dropped without a reply — the seam's "engine not
        /// materialized" state (`ThreadHandle::journal_snapshot` folds the
        /// dropped reply to `None`) that the PageHistory cold-disk path must
        /// survive.
        journal_available: AtomicBool,
    }

    impl FakeEngine {
        fn new() -> (
            Arc<Self>,
            tokio::sync::mpsc::UnboundedReceiver<BackendNotice>,
        ) {
            let (notices, events) = tokio::sync::mpsc::unbounded_channel();
            (
                Arc::new(Self {
                    runs: StdMutex::new(Vec::new()),
                    steer_calls: StdMutex::new(Vec::new()),
                    cwds: StdMutex::new(Vec::new()),
                    model_switches: StdMutex::new(Vec::new()),
                    notices,
                    auth_responses: StdMutex::new(Vec::new()),
                    pending_auth: StdMutex::new(Vec::new()),
                    plan_review_flags: StdMutex::new(Vec::new()),
                    plan_approvals: StdMutex::new(Vec::new()),
                    journal_tx: tokio::sync::broadcast::channel(64).0,
                    journal_data: StdMutex::new(manox_agent::engine::JournalSnapshotData {
                        cursor: 0,
                        records: Vec::new(),
                    }),
                    persist_calls: StdMutex::new(Vec::new()),
                    persist_reply: StdMutex::new(None),
                    origin_runs: StdMutex::new(Vec::new()),
                    journal_available: AtomicBool::new(true),
                }),
                events,
            )
        }

        /// GW6: flip the §C.3 read seam to "engine not materialized" — the
        /// oneshot's sender drops without answering, exactly like an engine
        /// actor that never assembled.
        fn set_journal_unavailable(&self) {
            self.journal_available.store(false, Ordering::SeqCst);
        }

        /// Script the accept-time persistence reply (K5 gateway tests).
        fn set_persist_reply(&self, reply: Option<Result<Option<String>, ()>>) {
            *self.persist_reply.lock().unwrap() = reply;
        }

        /// Replace the scripted whole-chain read (§C.3): the cursor and the
        /// dense records the follow snapshot / page reads answer with.
        fn set_journal(&self, cursor: u64, records: Vec<JournalRecord>) {
            *self.journal_data.lock().unwrap() =
                manox_agent::engine::JournalSnapshotData { cursor, records };
        }

        /// Append one live journal event onto the thread feed.
        fn push_journal(&self, seq: u64, entry: Arc<SessionTreeEntry>) {
            let _ = self
                .journal_tx
                .send(manox_agent::engine::JournalFeed::Event(JournalEvent {
                    seq,
                    entry,
                }));
        }
    }

    impl manox_agent::thread_engine::ThreadEngine for FakeEngine {
        fn is_running(&self) -> bool {
            false
        }
        fn history(&self) -> Vec<manox_agent::db::HistoryEntry> {
            Vec::new()
        }
        fn request_token_usage(&self) -> HashMap<String, manox_agent::TokenUsage> {
            HashMap::new()
        }
        fn model(&self) -> Option<manox_harness::types::Model> {
            None
        }
        fn run(&self, prompt: String, _: Vec<manox_harness::types::ContentBlock>) {
            self.runs.lock().unwrap().push(prompt);
        }
        fn run_with_origin(
            &self,
            prompt: String,
            images: Vec<manox_harness::types::ContentBlock>,
            origin: Option<String>,
            accepted_entry: Option<String>,
        ) {
            self.origin_runs
                .lock()
                .unwrap()
                .push((prompt.clone(), origin, accepted_entry));
            self.run(prompt, images);
        }
        fn persist_user_submission(
            &self,
            text: &str,
            images: Vec<manox_harness::types::ContentBlock>,
            origin: Option<String>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Option<String>, anyhow::Error>> + Send>,
        > {
            self.persist_calls
                .lock()
                .unwrap()
                .push((text.to_string(), images.len(), origin));
            let reply = self.persist_reply.lock().unwrap().clone();
            Box::pin(async move {
                match reply {
                    None => Ok(None),
                    Some(Ok(id)) => Ok(id),
                    Some(Err(())) => Err(anyhow::Error::msg("persist failure (test)")),
                }
            })
        }
        fn steer(&self, text: String, _: Vec<manox_harness::types::ContentBlock>) -> String {
            self.steer_calls.lock().unwrap().push(text);
            String::new()
        }
        fn cancel_steer(&self, _: &str) -> bool {
            false
        }
        fn abort(&self) {}
        fn set_model(&self, model: manox_harness::types::Model) {
            self.model_switches.lock().unwrap().push(model.id);
        }
        fn set_thinking_level(&self, _: Option<String>) {}
        fn set_plan_review_pending(&self, pending: bool) {
            self.plan_review_flags.lock().unwrap().push(pending);
        }
        fn approve_plan(&self, _compact: bool, _instructions: Option<String>, seed_text: String) {
            self.plan_approvals.lock().unwrap().push(seed_text);
        }
        fn open_session(&self, _: PathBuf) {}
        fn new_session(&self, _: PathBuf, _: Option<PathBuf>) {}
        fn set_cwd(&self, path: std::path::PathBuf) {
            self.cwds.lock().unwrap().push(path);
        }

        fn active_session_path(&self) -> Option<PathBuf> {
            None
        }
        fn subscribe_journal_feed(
            &self,
        ) -> tokio::sync::broadcast::Receiver<manox_agent::engine::JournalFeed> {
            self.journal_tx.subscribe()
        }
        fn journal_snapshot(
            &self,
        ) -> tokio::sync::oneshot::Receiver<manox_agent::engine::JournalSnapshotData> {
            let (tx, rx) = tokio::sync::oneshot::channel();
            if self.journal_available.load(Ordering::SeqCst) {
                let _ = tx.send(self.journal_data.lock().unwrap().clone());
            }
            // GW6 unavailable: `tx` drops without a reply — the seam answers
            // `Err`, which `ThreadHandle::journal_snapshot` folds to `None`.
            rx
        }
        fn session_list(&self) -> Vec<manox_agent::ThreadSummary> {
            Vec::new()
        }
        fn pending_auth_entries(&self) -> Vec<(String, manox_agent::permission::PendingAuthMeta)> {
            self.pending_auth.lock().unwrap().clone()
        }
        fn respond_tool_authorization(
            &self,
            id: &str,
            response: manox_agent::permission::ToolAuthorizationResponse,
        ) {
            self.auth_responses
                .lock()
                .unwrap()
                .push((id.to_string(), response));
        }
    }

    /// A connected client harness: the client end of an in-process pair.
    struct Client {
        conn: manox_protocol::InProcessConnection,
    }

    impl Client {
        fn send(&self, msg: FromClient) {
            self.conn.send_to_server(msg);
        }
        fn recv(&self) -> FromServer {
            // 30s (not 10s): on slow CI runners the agent-runtime task that
            // answers a call can spawn noticeably later than the test thread
            // sends it, and a too-tight deadline flakes the test.
            self.recv_timeout(Duration::from_secs(30))
        }
        fn recv_timeout(&self, timeout: Duration) -> FromServer {
            // Poll the async channel from the test thread (the dispatch/pump
            // tasks run on the agent runtime); try_recv + sleep avoids
            // blocking forever on a misrouted message.
            let rx = self.conn.server_rx();
            let deadline = std::time::Instant::now() + timeout;
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
        /// Per-connection FIFO sync point: round-trip a benign read call so
        /// every note sent earlier on this connection has been processed by
        /// the same dispatch loop before the caller asserts on state. (T10:
        /// the deleted v1 `ThreadInfo` query used to provide this rendezvous
        /// implicitly.) Intervening frames are drained — callers must settle
        /// only between assertions that do not consume wire frames.
        fn settle(&self) {
            self.send(FromClient::Request {
                id: MsgId::new("settle"),
                call: ClientCall::ListThreads,
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                let m = self.recv();
                if matches!(&m, FromServer::Response { id, .. } if id.0 == "settle") {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "settle response never arrived (last frame: {m:?})"
                );
            }
        }
    }

    fn harness(caps: Vec<HookKind>) -> (AgentServer, Client) {
        manox_agent::thread_store::init();
        let server = AgentServer::new(PathBuf::from("/"));
        let (client_conn, server_conn) = in_process_pair();
        server.accept(Arc::new(server_conn));
        let client = Client { conn: client_conn };
        // Handshake.
        let id = MsgId::new("init");
        client.send(FromClient::Request {
            id,
            call: ClientCall::Initialize(Initialize {
                client_id: "test".into(),
                capabilities: caps,
                sessions: vec![],
                protocol_epoch: PROTOCOL_EPOCH,
            }),
        });
        let resp = client.recv();
        assert!(matches!(resp, FromServer::Response { .. }), "expected ack");
        let ready = client.recv();
        assert!(matches!(
            ready,
            FromServer::Notification {
                note: ServerNote::Ready
            }
        ));
        // GW1/C1 dual emit: the handshake also carries the §D.5 Host mirror
        // with the accepted protocol epoch — every test through this harness
        // pins it, and the queue stays clean for the drain loops downstream.
        let host_ready = client.recv();
        assert!(
            matches!(
                host_ready,
                FromServer::Host {
                    host: HostEvent::Ready {
                        epoch: PROTOCOL_EPOCH
                    }
                }
            ),
            "expected the Host Ready epoch echo (GW1/C1), got {host_ready:?}"
        );
        (server, client)
    }

    /// Create a session through the compat note. GW11: an id whose journal
    /// file already exists in the (process-shared hermetic) sessions dir
    /// RESTORES from disk instead of starting fresh — tests that seed real
    /// files must use unique ids or remove them in teardown.
    fn create(_server: &AgentServer, client: &Client, id: &str) {
        client.send(FromClient::Notification {
            note: ClientNote::CreateSession {
                session_id: id.into(),
                cwd: Some("/".into()),
            },
        });
        loop {
            match client.recv() {
                FromServer::Notification {
                    note: ServerNote::SessionCreated { session_id },
                } if session_id == id => break,
                _ => {}
            }
        }
    }

    fn seed_session_file(dir: &std::path::Path, id: &str, cwd: &str) {
        std::fs::write(
            dir.join(format!("{id}.jsonl")),
            format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-05-28T07:13:46.608Z\",\"cwd\":\"{cwd}\"}}\n"),
        )
        .unwrap();
    }

    // ── Journal-vocabulary builders for the scripted stream tests (§C.2). ──
    use manox_harness::session::SessionTreeEntry;
    use manox_harness::session::jsonl::{JournalEvent, JournalRecord};

    /// A fixed envelope timestamp: scripted stream tests compare the two
    /// transports byte-identically, so no wall-clock may leak into a record.
    fn fixed_ts() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2020-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn jentry(
        id: &str,
        parent: Option<&str>,
        entry: fn(String, Option<String>) -> SessionTreeEntry,
    ) -> Arc<SessionTreeEntry> {
        Arc::new(entry(id.into(), parent.map(str::to_string)))
    }

    fn ent_turn_start(id: String, parent_id: Option<String>) -> SessionTreeEntry {
        SessionTreeEntry::TurnStart {
            id,
            parent_id,
            timestamp: fixed_ts(),
        }
    }
    fn ent_stop(id: String, parent_id: Option<String>) -> SessionTreeEntry {
        SessionTreeEntry::Stop {
            reason: Some("dual-path probe".into()),
            id,
            parent_id,
            timestamp: fixed_ts(),
        }
    }
    fn ent_agent_text_delta(id: String, parent_id: Option<String>) -> SessionTreeEntry {
        SessionTreeEntry::AgentTextDelta {
            delta: "tok".into(),
            id,
            parent_id,
            timestamp: fixed_ts(),
        }
    }
    fn ent_tool_call(id: String, parent_id: Option<String>) -> SessionTreeEntry {
        SessionTreeEntry::ToolCall {
            call_id: "tc-1".into(),
            name: "Bash".into(),
            title: "run ls".into(),
            status: "running".into(),
            input: Some(json!({"command": "ls"})),
            id,
            parent_id,
            timestamp: fixed_ts(),
        }
    }
    fn ent_tool_result(id: String, parent_id: Option<String>) -> SessionTreeEntry {
        SessionTreeEntry::ToolResult {
            call_id: "tc-1".into(),
            output: "file".into(),
            is_error: false,
            id,
            parent_id,
            timestamp: fixed_ts(),
        }
    }
    fn ent_model_change(id: String, parent_id: Option<String>) -> SessionTreeEntry {
        SessionTreeEntry::ModelChange {
            provider: "test-prov".into(),
            model_id: "m-1".into(),
            id,
            parent_id,
            timestamp: fixed_ts(),
        }
    }
    fn ent_permission_mode_change(id: String, parent_id: Option<String>) -> SessionTreeEntry {
        SessionTreeEntry::PermissionModeChange {
            mode: "workspace-write".into(),
            id,
            parent_id,
            timestamp: fixed_ts(),
        }
    }
    fn ent_title(id: String, parent_id: Option<String>) -> SessionTreeEntry {
        SessionTreeEntry::Title {
            title: "streamed title".into(),
            id,
            parent_id,
            timestamp: fixed_ts(),
        }
    }
    fn ent_goal(id: String, parent_id: Option<String>) -> SessionTreeEntry {
        SessionTreeEntry::Goal {
            goal: Some(json!({"objective": "ship T4"})),
            id,
            parent_id,
            timestamp: fixed_ts(),
        }
    }
    fn ent_ui_note(id: String, parent_id: Option<String>) -> SessionTreeEntry {
        SessionTreeEntry::UiNote {
            note: json!({"kind": "error", "data": {"text": "oops"}}),
            id,
            parent_id,
            timestamp: fixed_ts(),
        }
    }
    fn ent_error_event(id: String, parent_id: Option<String>) -> SessionTreeEntry {
        SessionTreeEntry::ErrorEvent {
            message: "provider exploded".into(),
            id,
            parent_id,
            timestamp: fixed_ts(),
        }
    }
    fn ent_turn_finish(id: String, parent_id: Option<String>) -> SessionTreeEntry {
        SessionTreeEntry::TurnFinish {
            cancelled: false,
            failed: false,
            stranded_steer_ids: Vec::new(),
            id,
            parent_id,
            timestamp: fixed_ts(),
        }
    }

    fn open_follow(client: &Client, stream: &str, session: &str) {
        client.send(FromClient::StreamOpen {
            stream_id: StreamId::new(stream),
            stream_kind: StreamKind::FollowSession {
                session_id: session.into(),
                max_messages: None,
            },
        });
    }

    /// Drain messages until one matches `check`, panicking after a deadline.
    fn expect<F>(client: &Client, check: F)
    where
        F: Fn(&FromServer) -> bool,
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let msg = client.recv();
            if check(&msg) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "expected message never arrived"
            );
        }
    }

    /// §D.5 / T10: the v2 turn-edge signal. Drain until a
    /// `Host{SessionStatus}` delta for `session_id` whose set fields pass
    /// `check`. Host frames are broadcast to every connection and may
    /// interleave with other traffic, so the match is a drain loop — the
    /// closure only ever sees frames for this session.
    fn expect_host_status<F>(client: &Client, session_id: &str, check: F)
    where
        F: Fn(Option<bool>, Option<bool>, Option<bool>, Option<bool>) -> bool,
    {
        expect(client, |m| {
            matches!(
                m,
                FromServer::Host {
                    host: HostEvent::SessionStatus {
                        session_id: sid,
                        running,
                        errored,
                        unread,
                        pending_auth,
                        ..
                    }
                } if sid == session_id && check(*running, *errored, *unread, *pending_auth)
            )
        });
    }

    /// Drain until the follow stream `stream_id` delivers its opening
    /// `Snapshot` frame (anything ahead of it is other-traffic noise and is
    /// skipped — the §F.1 rule 1 pin lives in
    /// `open_stream_emits_snapshot_then_gap_free_entries`).
    fn snapshot_for(client: &Client, stream_id: &str) -> manox_protocol::stream::SessionSnapshot {
        loop {
            match client.recv() {
                FromServer::StreamItem {
                    stream_id: sid,
                    frame: manox_protocol::StreamFrame::Snapshot(s),
                } if sid.0 == stream_id => return s,
                FromServer::StreamItem { stream_id: sid, .. } => {
                    // A different stream's traffic may interleave (throwaway
                    // probe streams); the §F.1 rule 1 pin (FIRST frame of the
                    // TARGET stream is the Snapshot) is kept exactly.
                    assert_ne!(
                        sid.0, stream_id,
                        "first {stream_id} frame must be the Snapshot"
                    );
                    continue;
                }
                // A cancelled probe stream's terminal frame is noise.
                FromServer::StreamEnd { .. } => continue,
                FromServer::Notification { .. } => continue,
                // GW1 dual-emit mirrors (Host SessionCreated etc.) are noise
                // for the stream-shape pins.
                FromServer::Host { .. } => continue,
                other => panic!("expected {stream_id} Snapshot, got {other:?}"),
            }
        }
    }

    /// Drain until a `Projections` delta for `session_id` stamped exactly
    /// `as_of_seq` arrives; returns the frame (its `values` carry the
    /// changed keys, §E.1).
    fn drain_until_projection(
        client: &Client,
        session_id: &str,
        as_of_seq: u64,
    ) -> manox_protocol::stream::ProjectionsFrame {
        loop {
            match client.recv() {
                FromServer::StreamItem {
                    frame: manox_protocol::StreamFrame::Projections(frame),
                    ..
                } if frame.session_id == session_id && frame.as_of_seq == as_of_seq => {
                    return frame;
                }
                FromServer::StreamItem { .. } => continue,
                FromServer::StreamEnd { .. } => continue,
                FromServer::Notification { .. } => continue,
                // GW1 dual-emit mirrors are noise for the P-face pin.
                FromServer::Host { .. } => continue,
                other => panic!("expected Projections frame, got {other:?}"),
            }
        }
    }

    /// The thread's projection baseline over the real v2 surface (the §E
    /// successor of the v1 `ClientCall::ThreadInfo` query): open a throwaway
    /// follow stream, take its snapshot baseline, cancel the stream. With a
    /// scripted empty journal the baseline is exactly the server-side seed of
    /// the live thread state; folded records would only add journal-driven
    /// changes on top.
    fn projection_baseline_of(
        client: &Client,
        session_id: &str,
        stream_id: &str,
    ) -> serde_json::Value {
        open_follow(client, stream_id, session_id);
        let snap = snapshot_for(client, stream_id);
        client.send(FromClient::StreamCancel {
            stream_id: StreamId::new(stream_id),
        });
        serde_json::to_value(snap.projections).unwrap()
    }

    /// Direct header truth (the kernel state the deleted v1 `ThreadInfo`
    /// payload mirrored): read `plan_mode` off the live thread.
    fn plan_mode_of(server: &AgentServer, session_id: &str) -> bool {
        server
            .0
            .session_thread(session_id)
            .expect("session thread present")
            .read(|t| t.plan_mode())
    }

    /// Await the one-shot provider-registration background build so a
    /// `register_test_model` below cannot be clobbered by its snapshot swap
    /// (the swap lands exactly once per process).
    fn await_provider_registry() {
        manox_agent::runtime::handle().block_on(manox_agent::provider_glue::wait_ready());
    }

    /// Register an Anthropic-shaped endpoint exposing model `id` into the
    /// process-wide provider registry. Append-only: the registry exposes no
    /// deregister/reload hook, so later tests in this binary see these models
    /// (and their first-sorted default) — keep registrations to tests that
    /// assert model values, and never rely on the registry being empty.
    fn register_test_model(id: &str) {
        use manox_harness::provider_registry::{
            Api, Cost, InputModality, ProviderConfig, ProviderModelConfig,
        };
        manox_agent::provider_glue::global()
            .register_provider(
                &format!("test-{id}"),
                ProviderConfig {
                    name: Some("Test".into()),
                    base_url: Some("https://test.example".into()),
                    api_key: Some("k".into()),
                    api: Some(Api::AnthropicMessages),
                    headers: None,
                    auth_header: false,
                    models: vec![ProviderModelConfig {
                        id: id.into(),
                        name: id.into(),
                        reasoning: false,
                        input: vec![InputModality::Text],
                        context_window: 1000,
                        max_tokens: 100,
                        cost: Cost::default(),
                        api: None,
                        base_url: None,
                        metadata: HashMap::new(),
                    }],
                },
            )
            .unwrap();
    }

    #[test]
    fn handshake_registers_client_and_sends_ready() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (_server, _client) = harness(vec![]);
    }

    /// A connection whose `send_to_client` parks on the first Host frame
    /// (signalling entry, then waiting for a test-controlled release) and
    /// delegates everything else to an ordinary in-process pair. It stands
    /// in for a stalled WS peer whose bounded carrier blocks in
    /// `send_blocking`.
    struct GatedConn {
        inner: manox_protocol::InProcessConnection,
        /// Signalled when a Host frame enters `send_to_client` and parks.
        entered: StdMutex<std::sync::mpsc::Sender<()>>,
        /// The parked send waits here until the test flips it.
        release: Arc<(StdMutex<bool>, std::sync::Condvar)>,
    }

    impl RpcConnection for GatedConn {
        fn send_to_client(&self, msg: FromServer) {
            if matches!(msg, FromServer::Host { .. }) {
                let _ = self.entered.lock().unwrap().send(());
                let (lock, cvar) = &*self.release;
                let mut released = lock.lock().unwrap();
                while !*released {
                    released = cvar.wait(released).unwrap();
                }
            }
            self.inner.send_to_client(msg);
        }
        fn send_to_server(&self, msg: FromClient) {
            self.inner.send_to_server(msg);
        }
        fn client_rx(&self) -> async_channel::Receiver<FromClient> {
            self.inner.client_rx()
        }
        fn server_rx(&self) -> async_channel::Receiver<FromServer> {
            self.inner.server_rx()
        }
        fn disconnect(&self) {
            self.inner.disconnect();
        }
    }

    /// GW4 regression: a server→client send may block on a bounded carrier
    /// (a stalled WS peer), and it must never do so while holding the shared
    /// `clients` lock — otherwise one slow client freezes every pump's host
    /// broadcast, Reply dispatch, and route_call registration gateway-wide.
    /// The gated connection parks deterministically inside `send_to_client`;
    /// the test asserts the lock stays acquirable and a healthy client's
    /// targeted traffic keeps flowing while the broadcast thread is parked.
    #[test]
    fn stalled_host_broadcast_never_holds_the_clients_lock() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let server = AgentServer::new(PathBuf::from("/"));

        // Client "gated": the handshake passes (non-Host frames delegate),
        // then the first Host frame parks inside send_to_client.
        let (gated_client_conn, gated_server_conn) = in_process_pair();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel::<()>();
        let release = Arc::new((StdMutex::new(false), std::sync::Condvar::new()));
        server.accept(Arc::new(GatedConn {
            inner: gated_server_conn,
            entered: StdMutex::new(entered_tx),
            release: release.clone(),
        }));
        let gated = Client {
            conn: gated_client_conn,
        };
        gated.send(FromClient::Request {
            id: MsgId::new("init-gated"),
            call: ClientCall::Initialize(Initialize {
                client_id: "gated".into(),
                capabilities: vec![],
                sessions: vec![],
                protocol_epoch: PROTOCOL_EPOCH,
            }),
        });
        assert!(matches!(gated.recv(), FromServer::Response { .. }));
        assert!(matches!(
            gated.recv(),
            FromServer::Notification {
                note: ServerNote::Ready
            }
        ));

        // Client "healthy": an ordinary in-process pair on the same server.
        let (healthy_client_conn, healthy_server_conn) = in_process_pair();
        server.accept(Arc::new(healthy_server_conn));
        let healthy = Client {
            conn: healthy_client_conn,
        };
        healthy.send(FromClient::Request {
            id: MsgId::new("init-healthy"),
            call: ClientCall::Initialize(Initialize {
                client_id: "healthy".into(),
                capabilities: vec![],
                sessions: vec![],
                protocol_epoch: PROTOCOL_EPOCH,
            }),
        });
        assert!(matches!(healthy.recv(), FromServer::Response { .. }));
        assert!(matches!(
            healthy.recv(),
            FromServer::Notification {
                note: ServerNote::Ready
            }
        ));

        // Broadcast from a side thread; it parks inside the gated client's
        // send. The clients-map iteration order decides whether the healthy
        // client is served before or after the park — both are legal, and
        // both clients must hold the frame once the gate releases.
        let inner = server.0.clone();
        let broadcaster = std::thread::spawn(move || {
            inner.broadcast_host(HostEvent::SessionStatus {
                session_id: "gw4".into(),
                running: Some(true),
                errored: None,
                unread: None,
                pending_auth: None,
                pending_plan: None,
                background_work: None,
            });
        });
        entered_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("gated send never entered");

        // The regression assertion: while a send is parked, the shared lock
        // must stay acquirable. Before the fix, broadcast_host held
        // `clients` across send_to_client, so this timed out — and every
        // pump, Reply dispatch, and route_call froze with it.
        assert!(
            server
                .0
                .clients
                .try_lock_for(Duration::from_millis(500))
                .is_some(),
            "clients lock held across a stalled send_to_client (GW4)"
        );

        // Targeted traffic to the healthy client is dispatched (and lands in
        // its carrier) while the gated one is still parked — reaching the
        // release below at all proves note_to_client did not block on the
        // contended lock.
        server.0.note_to_client(
            "healthy",
            ServerNote::Error {
                session_id: None,
                message: "gw4-ping".into(),
            },
        );

        // Release the gate; the broadcast completes for every client.
        {
            let (lock, cvar) = &*release;
            *lock.lock().unwrap() = true;
            cvar.notify_all();
        }
        broadcaster.join().expect("broadcaster panicked");

        // The gated client sees the Host frame after the release.
        expect_host_status(&gated, "gw4", |running, _, _, _| running == Some(true));

        // The healthy client holds both the targeted ping and the broadcast
        // frame; arrival order depends on the map iteration and the park
        // point, so collect until both are seen.
        let mut saw_ping = false;
        let mut saw_host = false;
        let rx = healthy.conn.server_rx();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !(saw_ping && saw_host) {
            match rx.try_recv() {
                Ok(FromServer::Notification {
                    note: ServerNote::Error { message, .. },
                }) if message == "gw4-ping" => saw_ping = true,
                Ok(FromServer::Host {
                    host:
                        HostEvent::SessionStatus {
                            session_id,
                            running,
                            ..
                        },
                }) if session_id == "gw4" && running == Some(true) => saw_host = true,
                Ok(_) => {}
                Err(_) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "healthy client never saw ping+host: ping={saw_ping} host={saw_host}"
                    );
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }

        gated.conn.disconnect();
        healthy.conn.disconnect();
        drop(gated);
        drop(healthy);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW11 regression: CreateSession bearing an id whose journal file
    /// already exists on disk (a cold session — the compat-note path the
    /// desktop landing takes) must restore the persisted session, never
    /// mint a fresh one over the id. Before the fix the request fell
    /// through to `new_fresh` ("never restores the previous session"): a
    /// deferred session with no path at all, whose first assistant message
    /// rewrote the existing file wholesale, erasing the cold history. The
    /// gateway-level observable is the live thread's engine binding —
    /// `open_existing` seeds the active session path at spawn, independent
    /// of the engine actor's asynchronous assembly (which needs provider
    /// configuration this suite does not own).
    #[test]
    fn create_session_with_a_cold_persisted_id_restores_history() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);

        // Keep any engine-actor registry writes (order-dependent: a prior
        // test's model registration lets the actor survive startup) inside
        // this test's own file instead of the shared hermetic registry.
        let registry_tmp = std::env::temp_dir().join(format!(
            "gw11-cold-restore-registry-{}.json",
            std::process::id()
        ));
        manox_agent::thread_registry::set_registry_path_for_test(Some(registry_tmp.clone()));

        // Seed a persisted v4 session under the hermetic sessions dir:
        // header plus a dense two-entry chain, exactly what a real cold
        // session looks like. The header cwd is a directory that exists —
        // a restored actor re-pins its tool cwd to it. No list refresh
        // runs: the restore must work from the authoritative on-disk probe
        // alone.
        let sessions = manox_agent::paths::sessions_dir().unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        let cwd = sessions.parent().unwrap().to_string_lossy().into_owned();
        let path = sessions.join("cold-1.jsonl");
        let contents = format!(
            r#"{{"type":"session","version":4,"id":"cold-1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"{cwd}"}}
{{"type":"message","id":"m1","parentId":null,"seq":0,"timestamp":"2026-05-28T07:14:00.000Z","message":{{"role":"user","content":[{{"type":"text","text":"one"}}],"timestamp":1779952440000}}}}
{{"type":"message","id":"m2","parentId":"m1","seq":1,"timestamp":"2026-05-28T07:14:10.000Z","message":{{"role":"user","content":[{{"type":"text","text":"two"}}],"timestamp":1779952450000}}}}
"#
        );
        std::fs::write(&path, &contents).unwrap();

        // The compat create note with an explicit id (the desktop landing
        // path, `ClientNote::CreateSession`).
        client.send(FromClient::Notification {
            note: ClientNote::CreateSession {
                session_id: "cold-1".into(),
                cwd: Some(cwd),
            },
        });
        loop {
            match client.recv() {
                FromServer::Notification {
                    note: ServerNote::SessionCreated { session_id },
                } if session_id == "cold-1" => break,
                _ => {}
            }
        }

        // Restore identity: the live thread is bound to the persisted
        // journal file. A fresh-minted session carries no active path
        // (deferred until its first assistant message).
        let thread = server
            .0
            .session_thread("cold-1")
            .expect("the cold id became a live session");
        let active = thread.read(|t| t.active_session_path());
        assert_eq!(
            active.as_deref(),
            Some(path.as_path()),
            "the restored session is bound to the persisted journal file"
        );

        // The restore reads the journal; it never rewrites it.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);

        // Teardown: drop the live session, remove the seed file so later
        // tests' directory scans never see it, and restore the shared
        // registry path.
        server.0.dispose_session("test", "cold-1");
        let _ = std::fs::remove_file(&path);
        manox_agent::thread_registry::set_registry_path_for_test(None);
        let _ = std::fs::remove_file(&registry_tmp);
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn submit_streams_turn_started_then_finished() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("s1", engine.clone(), events);
        client.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "s1".into(),
                text: "hello".into(),
                images: vec![],
                client_id: None,
            },
        });
        // v2 turn edges (§D.5): the pump's `SessionStatus` host deltas replace
        // the doomed TurnStarted/TurnFinished notes — running rises to true on
        // the turn, falls back to false on settle.
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(true));
        engine
            .notices
            .send(BackendNotice::Settled {
                cancelled: false,
                failed: false,
                steered: Vec::new(),
                stranded: Vec::new(),
            })
            .unwrap();
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(false));
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn open_session_replays_thread_history() {
        // T10 (§D.6): reopen no longer pushes the v1 history mirror — the
        // authoritative replay is the follow stream's opening `Snapshot`,
        // whose content rides the §C.2 journal wire vocabulary end to end.
        let _g = lock_globals();
        hermetic_home();
        let sessions = manox_agent::paths::manox_config_dir()
            .expect("config dir")
            .join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        seed_session_file(&sessions, "s1", "/proj");
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        client.send(FromClient::Request {
            id: MsgId::new("open"),
            call: ClientCall::OpenSession {
                session_id: "s1".into(),
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionCreated { session_id } } if session_id == "s1"),
        );
        // GW1: the Host SessionCreated mirror rides between the note and the
        // Response — drain to the ack instead of a bare recv.
        expect(
            &client,
            |m| matches!(m, FromServer::Response { id, outcome: Ok(v) } if id.0 == "open" && v["restored"] == true),
        );
        // The v2 replay: attach the scripted read seam and open the stream.
        let (engine, events) = FakeEngine::new();
        engine.set_journal(
            1,
            vec![
                JournalRecord {
                    seq: 0,
                    entry: (*jentry("e-0", None, ent_turn_start)).clone(),
                },
                JournalRecord {
                    seq: 1,
                    entry: (*jentry("e-1", Some("e-0"), ent_turn_finish)).clone(),
                },
            ],
        );
        server.set_session_engine_for_test("s1", engine.clone(), events);
        open_follow(&client, "st-1", "s1");
        let snap = snapshot_for(&client, "st-1");
        assert_eq!(snap.session_id, "s1");
        assert_eq!(snap.cursor, 1);
        assert_eq!(snap.records.len(), 2);
        assert_eq!(snap.records[0].seq, 0);
        assert_eq!(snap.records[1].seq, 1);
        assert!(!snap.has_more);
        // The thread metadata that rode the deleted `ThreadInfo` note is
        // baseline-side: the header carries the reopened cwd, the projection
        // baseline declares the full §E surface.
        assert_eq!(snap.header.cwd, "/proj");
        assert_eq!(snap.header.id, "s1");
        let mut want: Vec<&str> = manox_protocol::surface::PROJECTION_KEYS.to_vec();
        want.sort_unstable();
        let mut got: Vec<&str> = snap.projections.keys().map(String::as_str).collect();
        got.sort_unstable();
        assert_eq!(got, want, "the replay carries the declared surface");
        assert_eq!(
            snap.projections["cwd"],
            serde_json::Value::String("/proj".into())
        );
        drop(client);
        drop(server);
        // GW11 hygiene: create() with an id whose journal file exists
        // on disk now RESTORES it; remove this test's seed so the shared
        // hermetic sessions dir never hijacks a later test's create("s1").
        let _ = std::fs::remove_file(sessions.join("s1.jsonl"));
        manox_agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn approve_call_round_trips_and_unparks() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![HookKind::Approve]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("s1", engine.clone(), events);
        client.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "s1".into(),
                text: "do work".into(),
                images: vec![],
                client_id: None,
            },
        });
        // Turn edge as a v2 host delta (§D.5), not the doomed TurnStarted note.
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(true));
        engine
            .notices
            .send(BackendNotice::Event(Box::new(
                ThreadEvent::ToolCallAuthorization {
                    id: "a1".into(),
                    tool_name: "Bash".into(),
                    summary: "run ls".into(),
                    input: json!({}),
                },
            )))
            .unwrap();
        // The server issues a ServerCall::Approve the client must answer.
        let call_id = loop {
            match client.recv() {
                FromServer::Request {
                    id,
                    call: ServerCall::Approve { auth_id, .. },
                } if auth_id == "a1" => break id,
                _ => {}
            }
        };
        client.send(FromClient::Reply {
            id: call_id,
            outcome: Ok(json!({"allow": true})),
        });
        // route_call applies the Reply asynchronously; poll for AllowOnce
        // rather than racing the pump.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let got = engine.auth_responses.lock().unwrap().iter().any(|(_, r)| {
                matches!(
                    r,
                    manox_agent::permission::ToolAuthorizationResponse::Decision(
                        manox_agent::permission::PermissionDecision::AllowOnce
                    )
                )
            });
            if got {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "engine never received AllowOnce"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        engine
            .notices
            .send(BackendNotice::Settled {
                cancelled: false,
                failed: false,
                steered: Vec::new(),
                stranded: Vec::new(),
            })
            .unwrap();
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(false));
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn approve_with_no_capable_owner_fails_closed() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        // Client declares no capabilities.
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("s1", engine.clone(), events);
        client.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "s1".into(),
                text: "do work".into(),
                images: vec![],
                client_id: None,
            },
        });
        // Turn edge as a v2 host delta (§D.5), not the doomed TurnStarted note.
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(true));
        engine
            .notices
            .send(BackendNotice::Event(Box::new(
                ThreadEvent::ToolCallAuthorization {
                    id: "a1".into(),
                    tool_name: "Bash".into(),
                    summary: "run ls".into(),
                    input: json!({}),
                },
            )))
            .unwrap();
        // Fail-closed: the engine gets Deny and the client sees an Error.
        let mut saw_error = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let FromServer::Notification {
                note: ServerNote::Error { .. },
            } = client.recv_timeout(Duration::from_secs(2))
            {
                saw_error = true;
                break;
            }
        }
        assert!(saw_error, "expected a fail-closed Error note");
        assert!(
            engine
                .auth_responses
                .lock()
                .unwrap()
                .iter()
                .any(|(_, r)| matches!(
                    r,
                    manox_agent::permission::ToolAuthorizationResponse::Decision(
                        manox_agent::permission::PermissionDecision::Deny
                    )
                ))
        );
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn set_model_and_thread_info() {
        // T10 (§D.6): the `ThreadInfo` mirror is gone. Chip-relevant thread
        // metadata rides the projection surface: the snapshot baseline seeds
        // from live thread state, an engine-journaled mutation republishes
        // unprompted through the P-face delta (§E.1).
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        await_provider_registry();
        // Two resolvable models so the SetModel step is a real switch, not a
        // no-op. `alpha-model` sorts first, so it is the default model
        // `create_session` picks at spawn time (empty hermetic HOME otherwise
        // has no default at all); the SetModel target is `beta-model`.
        register_test_model("alpha-model");
        register_test_model("beta-model");
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        // A no-op engine so the SetCwd project binding never materializes a
        // real pi engine actor (same pattern as the submit tests).
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("s1", engine.clone(), events);
        // The baseline carries the declared surface with the create-time
        // state populated (the former 22-field `ThreadInfo` payload's
        // projection successor).
        open_follow(&client, "st-1", "s1");
        let snap = snapshot_for(&client, "st-1");
        let mut want: Vec<&str> = manox_protocol::surface::PROJECTION_KEYS.to_vec();
        want.sort_unstable();
        let mut got: Vec<&str> = snap.projections.keys().map(String::as_str).collect();
        got.sort_unstable();
        assert_eq!(got, want, "snapshot baseline IS the declared surface");
        let p = &snap.projections;
        assert_eq!(p["cwd"], json!("/"));
        assert_eq!(p["permission_mode"], json!("workspace_write"));
        assert_eq!(p["self_author"], json!("lead"));
        assert_eq!(p["running"], json!(false));
        assert_eq!(p["plan_mode"], json!(false));
        assert_eq!(p["has_interacted"], json!(false));
        // `create_session` seeds the default: the hermetic HOME has no settings
        // `default_model` reference, so `default_model()` resolves to the
        // first-sorted registered model — `alpha-model`. The typed model pair
        // (provider+modelId) replaces the payload's `model_id`/`model` fields.
        assert_eq!(
            p["model"]["modelId"],
            json!("alpha-model"),
            "create-time default model should be the first-sorted registered model"
        );
        assert!(
            p["model"]["provider"].is_string(),
            "chips read the typed model pair, not just an id"
        );

        // Composer-chip regression: a model switch reaches the engine (which
        // journals it), and the journal write republishes the `model` key
        // unprompted — no query, no mirror note. The scripted journal entry
        // stands in for the real engine's durable write (same fold path as
        // `open_stream_emits_snapshot_then_gap_free_entries`).
        client.send(FromClient::Notification {
            note: ClientNote::SetModel {
                session_id: "s1".into(),
                id: "beta-model".into(),
            },
        });
        // FIFO sync: the note has been dispatched before we read the engine.
        client.settle();
        assert_eq!(
            engine.model_switches.lock().unwrap().as_slice(),
            ["beta-model".to_string()],
            "SetModel must forward the resolved model to the engine"
        );
        engine.push_journal(1, jentry("e-1", Some("e-0"), ent_model_change));
        let frame = drain_until_projection(&client, "s1", 1);
        assert_eq!(
            frame.values["model"],
            json!({ "provider": "test-prov", "modelId": "m-1" }),
            "a journaled model change must republish the projection unprompted"
        );
        // A freshly opened stream's baseline re-seeds from the live thread:
        // it observes the switch the server applied.
        let baseline = projection_baseline_of(&client, "s1", "st-2");
        assert_eq!(baseline["model"]["modelId"], json!("beta-model"));
        client.send(FromClient::Notification {
            note: ClientNote::SetCwd {
                session_id: "s1".into(),
                cwd: "/proj".into(),
            },
        });
        // Not-yet-interacted: SetCwd binds project + header cwd; the baseline
        // of a fresh stream observes both.
        let baseline = projection_baseline_of(&client, "s1", "st-3");
        assert_eq!(baseline["cwd"], json!("/proj"));
        assert_eq!(baseline["project"], json!("/proj"));
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// A conversation's project never re-binds: once the thread has interacted,
    /// `SetCwd` moves only the engine's working directory, leaving the bound
    /// project and the header cwd untouched — and the projection baseline
    /// still reports the untouched header fields (T10: the v1 `ThreadInfo`
    /// republish this test drained to is gone; §E baseline is the successor).
    #[test]
    fn set_cwd_after_interaction_moves_engine_not_project() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("s1", engine.clone(), events);

        // Mark the thread as having interacted with a real submit turn. Stop
        // at `TurnStarted` without settling: `Settled` re-reads the transcript
        // through `engine.history()` — empty on the fake — which would wipe
        // the user message and the interaction state the guard depends on.
        client.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "s1".into(),
                text: "hello".into(),
                images: vec![],
                client_id: None,
            },
        });
        // v2 turn edge (§D.5): stop at the running=true host delta without
        // settling, exactly as the old TurnStarted-note gate did — `Settled`
        // would wipe the interaction state through the fake's empty history().
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(true));

        // Now switch the working directory. The project header must stay at
        // the create-time cwd; only the engine's cwd advances.
        client.send(FromClient::Notification {
            note: ClientNote::SetCwd {
                session_id: "s1".into(),
                cwd: "/moved".into(),
            },
        });
        let baseline = projection_baseline_of(&client, "s1", "st-probe");
        assert_eq!(
            baseline["project"],
            serde_json::Value::Null,
            "an interacted thread's project never re-binds via SetCwd"
        );
        assert_eq!(baseline["has_interacted"], json!(true));
        assert_eq!(
            baseline["cwd"],
            json!("/"),
            "header cwd is untouched by SetCwd"
        );
        assert!(
            engine
                .cwds
                .lock()
                .unwrap()
                .iter()
                .any(|p| p == std::path::Path::new("/moved")),
            "the working-directory switch must reach the engine"
        );
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn detach_keeps_turn_alive() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("s1", engine.clone(), events);
        client.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "s1".into(),
                text: "hello".into(),
                images: vec![],
                client_id: None,
            },
        });
        // v2 turn edge (§D.5) replaces the TurnStarted note gate.
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(true));
        // Detach drops ownership without cancelling; the engine keeps its run.
        client.send(FromClient::Notification {
            note: ClientNote::DetachSession {
                session_id: "s1".into(),
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionDisposed { session_id } } if session_id == "s1"),
        );
        // The engine recorded exactly one run (no cancel re-run).
        assert_eq!(engine.runs.lock().unwrap().len(), 1);
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }
    #[test]
    fn ask_user_question_round_trips() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![HookKind::AskUserQuestion]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("s1", engine.clone(), events);
        client.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "s1".into(),
                text: "ask me".into(),
                images: vec![],
                client_id: None,
            },
        });
        // v2 turn edge (§D.5) replaces the TurnStarted note gate.
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(true));
        engine
            .notices
            .send(BackendNotice::Event(Box::new(
                ThreadEvent::ToolCallAuthorization {
                    id: "q1".into(),
                    tool_name: manox_agent::tools::ASK_USER_QUESTION.to_string(),
                    summary: "pick a color".into(),
                    input: json!({"question": "color?"}),
                },
            )))
            .unwrap();
        // An AskUser authorization routes as ServerCall::AskUserQuestion, not Approve.
        let call_id = loop {
            match client.recv() {
                FromServer::Request {
                    id,
                    call: ServerCall::AskUserQuestion { auth_id, .. },
                } if auth_id == "q1" => break id,
                _ => {}
            }
        };
        client.send(FromClient::Reply {
            id: call_id,
            outcome: Ok(json!({"answers": [["color", "blue"]], "response": null})),
        });
        // The engine received the structured answers (not a bare Deny).
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let got = engine.auth_responses.lock().unwrap().iter().any(|(id, r)| {
                id == "q1"
                    && matches!(
                        r,
                        manox_agent::permission::ToolAuthorizationResponse::AskUserQuestion { .. }
                    )
            });
            if got {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "engine never received AskUserQuestion answers"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        engine
            .notices
            .send(BackendNotice::Settled {
                cancelled: false,
                failed: false,
                steered: Vec::new(),
                stranded: Vec::new(),
            })
            .unwrap();
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(false));
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn plan_verdict_round_trips_and_seeds_execution() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![HookKind::PlanVerdict]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("s1", engine.clone(), events);
        client.send(FromClient::Notification {
            note: ClientNote::SetPlanMode {
                session_id: "s1".into(),
                enabled: true,
            },
        });
        client.settle(); // FIFO: the SetPlanMode note has been dispatched.
        // Before the verdict, plan_mode is on (confirms SetPlanMode applied).
        // T10: the v1 `ThreadInfo` query is gone — the header truth the
        // deleted payload mirrored is the thread itself; the P-face fold of
        // `PlanModeChange` is pinned in `projections`.
        assert!(plan_mode_of(&server, "s1"));
        let plan_file =
            std::env::temp_dir().join(format!("manox-beta3b-plan-{}.md", std::process::id()));
        std::fs::write(&plan_file, "# Plan\n\n1. Step one\n").unwrap();
        engine
            .notices
            .send(BackendNotice::Event(Box::new(ThreadEvent::PlanReady {
                plan_file: plan_file.to_string_lossy().into_owned(),
                title: "Test plan".into(),
            })))
            .unwrap();
        // PlanReady initiates ServerCall::PlanVerdict carrying the plan body.
        let call_id = loop {
            match client.recv() {
                FromServer::Request {
                    id,
                    call:
                        ServerCall::PlanVerdict {
                            plan_file: pf,
                            content,
                            ..
                        },
                } if pf == plan_file.to_string_lossy() => {
                    assert!(content.is_some(), "PlanVerdict must carry the plan body");
                    break id;
                }
                _ => {}
            }
        };
        client.send(FromClient::Reply {
            id: call_id,
            outcome: Ok(json!({"choice": "execute_keep"})),
        });
        // execute_keep → approve_plan → plan_mode flips off (async: route_call
        // applies the reply on the pump task; poll rather than race it).
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if !plan_mode_of(&server, "s1") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "plan_mode never flipped off after execute_keep"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(&plan_file);
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }
    #[test]
    fn browser_op_routes_to_client_and_returns_reply() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        manox_agent::capability::drop_provider_for_test();
        let (server, client) = harness(vec![HookKind::BrowserOp]);
        manox_agent::capability::set_provider(Arc::new(AgentServerCapabilityClient::new(&server)));
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("s1", engine.clone(), events);
        client.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "s1".into(),
                text: "browse".into(),
                images: vec![],
                client_id: None,
            },
        });
        // v2 turn edge (§D.5) replaces the TurnStarted note gate.
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(true));
        // Inject a BrowserRequest; the AgentServer's impl routes it to the client.
        let (tx, rx) = async_channel::bounded(1);
        engine
            .notices
            .send(BackendNotice::BrowserRequest {
                op: manox_agent::thread_engine::BrowserOp::Open {
                    url: "https://example.com".into(),
                },
                responder: tx,
            })
            .unwrap();
        // The client receives ServerCall::BrowserOp for session s1.
        let call_id = loop {
            match client.recv() {
                FromServer::Request {
                    id,
                    call: ServerCall::BrowserOp { session_id, .. },
                } if session_id == "s1" => break id,
                _ => {}
            }
        };
        // Reply with a BrowserReply::TabId(1).
        client.send(FromClient::Reply {
            id: call_id,
            outcome: Ok(
                serde_json::to_value(manox_agent::thread_engine::BrowserReply::TabId(1)).unwrap(),
            ),
        });
        // The engine's responder got the BrowserReply.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(reply) = rx.try_recv() {
                assert!(reply.is_ok(), "browser op should succeed, not fail-closed");
                assert!(matches!(
                    reply.unwrap(),
                    manox_agent::thread_engine::BrowserReply::TabId(_)
                ));
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "browser op reply never arrived"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(client);
        drop(server);
        manox_agent::capability::drop_provider_for_test();
        manox_agent::thread_store::drop_global_for_test();
    }

    /// §D.1 atomicity (T10 successor of the v1 open-time snapshot race): the
    /// follow task subscribes to the journal feed BEFORE the snapshot read,
    /// so an entry that lands in between still forwards as exactly one live
    /// `Entry` frame with no duplicate inside the snapshot (§F.1 rule 2).
    /// An entry landing after the snapshot read must likewise never appear
    /// in the snapshot — the §E fold sees it once.
    #[test]
    fn open_session_snapshot_subscribe_is_atomic() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        // Seed the whole-chain read: one dense record (cursor = 0).
        engine.set_journal(
            0,
            vec![JournalRecord {
                seq: 0,
                entry: (*jentry("e-0", None, ent_turn_start)).clone(),
            }],
        );
        server.set_session_engine_for_test("s1", engine.clone(), events);
        open_follow(&client, "st-1", "s1");
        let snap = snapshot_for(&client, "st-1");
        assert_eq!(snap.cursor, 0);
        assert_eq!(snap.records.len(), 1);

        // Inject the live edge right after the snapshot read: it must arrive
        // exactly once as an Entry frame, never inside the snapshot.
        engine.push_journal(1, jentry("e-1", Some("e-0"), ent_turn_finish));
        let mut entry_frames = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                std::time::Instant::now() < deadline,
                "live entry after the snapshot read never forwarded"
            );
            match client.recv() {
                FromServer::StreamItem {
                    stream_id,
                    frame: manox_protocol::StreamFrame::Entry { seq, .. },
                } => {
                    assert_eq!(stream_id.0, "st-1");
                    assert_eq!(seq, 1, "gap-free continuation of the cursor");
                    entry_frames += 1;
                }
                // The P-face delta for the turn-finish edge and any v1
                // compat traffic are expected noise for this pin.
                FromServer::StreamItem { .. } => continue,
                FromServer::Notification { .. } => continue,
                other => panic!("expected the live Entry frame, got {other:?}"),
            }
            // Settle window: nothing further may deliver the same edge.
            let settle = std::time::Instant::now() + Duration::from_millis(300);
            loop {
                match client.conn.server_rx().try_recv() {
                    Ok(FromServer::StreamItem {
                        frame: manox_protocol::StreamFrame::Entry { seq: 1, .. },
                        ..
                    }) => {
                        panic!("duplicate live delivery of the same entry");
                    }
                    Ok(_) => continue,
                    Err(_) if std::time::Instant::now() < settle => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            break;
        }
        assert_eq!(entry_frames, 1);
        // The snapshot the client already holds never contained the edge —
        // the read is the scripted chain, whose tail (seq 0) precedes it.
        assert_eq!(snap.records.len(), 1);
        assert_eq!(snap.records[0].seq, 0);
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    use manox_protocol::transport::{BACKPRESSURE_CAPACITY, BackpressurePolicy, RpcConnection};

    /// A serde-loopback connection: every message crosses the wire as JSON —
    /// the serialization shape the napi/webui transports use. Round-trips
    /// `FromServer`/`FromClient` through `serde_json` inside the send calls
    /// and applies the same backpressure semantics as the in-process pair.
    struct SerdeLoopbackConn {
        c2s_tx: async_channel::Sender<FromClient>,
        c2s_rx: async_channel::Receiver<FromClient>,
        s2c_tx: async_channel::Sender<FromServer>,
        s2c_rx: async_channel::Receiver<FromServer>,
    }

    fn serde_pair() -> (SerdeLoopbackConn, SerdeLoopbackConn) {
        let (c2s_tx, c2s_rx) = async_channel::bounded(BACKPRESSURE_CAPACITY);
        let (s2c_tx, s2c_rx) = async_channel::bounded(BACKPRESSURE_CAPACITY);
        let client = SerdeLoopbackConn {
            c2s_tx: c2s_tx.clone(),
            c2s_rx: c2s_rx.clone(),
            s2c_tx: s2c_tx.clone(),
            s2c_rx: s2c_rx.clone(),
        };
        let server = SerdeLoopbackConn {
            c2s_tx,
            c2s_rx,
            s2c_tx,
            s2c_rx,
        };
        (client, server)
    }

    impl RpcConnection for SerdeLoopbackConn {
        fn send_to_client(&self, msg: FromServer) {
            let wire = serde_json::to_string(&msg).expect("FromServer serializes");
            let msg: FromServer = serde_json::from_str(&wire).expect("FromServer deserializes");
            let drop = matches!(
                &msg,
                FromServer::Notification { note }
                    if note.backpressure_policy() == BackpressurePolicy::Drop
            );
            if drop {
                let _ = self.s2c_tx.try_send(msg);
            } else {
                let _ = self.s2c_tx.send_blocking(msg);
            }
        }
        fn send_to_server(&self, msg: FromClient) {
            let wire = serde_json::to_string(&msg).expect("FromClient serializes");
            let msg: FromClient = serde_json::from_str(&wire).expect("FromClient deserializes");
            let _ = self.c2s_tx.send_blocking(msg);
        }
        fn client_rx(&self) -> async_channel::Receiver<FromClient> {
            self.c2s_rx.clone()
        }
        fn server_rx(&self) -> async_channel::Receiver<FromServer> {
            self.s2c_rx.clone()
        }
        fn disconnect(&self) {
            self.c2s_tx.close();
            self.s2c_tx.close();
        }
    }

    /// Test-side handle mirroring the in-process `Client` helper.
    struct SerdeClient {
        conn: SerdeLoopbackConn,
    }

    impl SerdeClient {
        fn send(&self, msg: FromClient) {
            self.conn.send_to_server(msg);
        }
        fn recv_timeout(&self, timeout: Duration) -> FromServer {
            let rx = self.conn.server_rx();
            let deadline = std::time::Instant::now() + timeout;
            loop {
                match rx.try_recv() {
                    Ok(m) => return m,
                    Err(_) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => panic!("timed out waiting for a serde-path message"),
                }
            }
        }
    }

    /// ε-1: the SAME client script driven through the in-process pair and
    /// through the serde loopback must produce identical `FromServer`
    /// sequences. This is the single-protocol-surface contract in executable
    /// form: no transport may reinterpret a message.
    ///
    /// Determinism: both sessions run the same FakeEngine script with fixed
    /// ids (`a1`, `call-N` counters start at 0 per fresh server); Drop-policy
    /// streaming notes are filtered (their loss is policy, not content); the
    /// two session ids are normalized to one placeholder before comparing.
    #[test]
    fn dual_path_transport_consistency() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();

        // ── Path 1: in-process pair ──
        let (server, client_ip) = harness(vec![HookKind::Approve]);
        let (engine_ip, events_ip) = FakeEngine::new();
        create(&server, &client_ip, "sess-inproc");
        server.set_session_engine_for_test("sess-inproc", engine_ip.clone(), events_ip);

        // ── Path 2: serde loopback ──
        let (client_sl_conn, server_sl) = serde_pair();
        server.accept(std::sync::Arc::new(server_sl));
        let client_sl = SerdeClient {
            conn: client_sl_conn,
        };
        client_sl.send(FromClient::Request {
            id: MsgId::new("init"),
            call: ClientCall::Initialize(Initialize {
                client_id: "serde-test".into(),
                capabilities: vec![HookKind::Approve],
                sessions: vec![],
                protocol_epoch: PROTOCOL_EPOCH,
            }),
        });
        assert!(matches!(
            client_sl.recv_timeout(Duration::from_secs(10)),
            FromServer::Response { .. }
        ));
        assert!(matches!(
            client_sl.recv_timeout(Duration::from_secs(10)),
            FromServer::Notification {
                note: ServerNote::Ready
            }
        ));
        // GW1/C1: the Host Ready epoch echo is part of the handshake on both
        // paths — drain it here exactly like `harness()` does for the
        // in-process path, so the multiset comparison below sees the same
        // handshake prefix on both.
        assert!(matches!(
            client_sl.recv_timeout(Duration::from_secs(10)),
            FromServer::Host {
                host: HostEvent::Ready {
                    epoch: PROTOCOL_EPOCH
                }
            }
        ));
        let (engine_sl, events_sl) = FakeEngine::new();
        client_sl.send(FromClient::Notification {
            note: ClientNote::CreateSession {
                session_id: "sess-serde".into(),
                cwd: Some("/".into()),
            },
        });
        loop {
            match client_sl.recv_timeout(Duration::from_secs(10)) {
                FromServer::Notification {
                    note: ServerNote::SessionCreated { session_id },
                } if session_id == "sess-serde" => break,
                _ => {}
            }
        }
        server.set_session_engine_for_test("sess-serde", engine_sl.clone(), events_sl);

        // ── The same script on both sessions ──
        let notes = |sid: &str| {
            vec![ClientNote::Submit {
                session_id: sid.into(),
                text: "do work".into(),
                images: vec![],
                client_id: None,
            }]
        };
        for note in notes("sess-inproc") {
            client_ip.send(FromClient::Notification { note });
        }
        for note in notes("sess-serde") {
            client_sl.send(FromClient::Notification { note });
        }

        // Sequence the script: the v2 turn edge (§D.5 `SessionStatus`
        // running=true) must land on BOTH paths before the auth notice is
        // injected — otherwise the dispatch task's turn start and the pump
        // task's Approve interleave non-deterministically (two concurrent
        // server-side sources, not a transport difference). The gate frame
        // itself is pushed into the collected sequence, so the host-frame
        // multiset comparison below still verifies transport identity of the
        // edge signal.
        let is_turn_edge = |m: &FromServer| {
            matches!(
                m,
                FromServer::Host {
                    host: HostEvent::SessionStatus {
                        running: Some(true),
                        ..
                    }
                }
            )
        };
        let mut seq_ip: Vec<FromServer> = Vec::new();
        loop {
            let m = client_ip.recv();
            let hit = is_turn_edge(&m);
            seq_ip.push(m);
            if hit {
                break;
            }
        }
        let mut seq_sl: Vec<FromServer> = Vec::new();
        loop {
            let m = client_sl.recv_timeout(Duration::from_secs(10));
            let hit = is_turn_edge(&m);
            seq_sl.push(m);
            if hit {
                break;
            }
        }

        // Engine-side script: one authorization round-trip per session,
        // injected only after both paths settled the turn edge.
        for engine in [&engine_ip, &engine_sl] {
            engine
                .notices
                .send(BackendNotice::Event(Box::new(
                    ThreadEvent::ToolCallAuthorization {
                        id: "a1".into(),
                        tool_name: "Bash".into(),
                        summary: "run ls".into(),
                        input: json!({}),
                    },
                )))
                .unwrap();
        }

        // Collect until each path has seen the Approve request; reply, then
        // collect the remaining tail.

        let call_ip = loop {
            let m = client_ip.recv();
            if let FromServer::Request {
                id,
                call: ServerCall::Approve { auth_id, .. },
            } = &m
                && auth_id == "a1"
            {
                seq_ip.push(m.clone());
                break id.clone();
            }
            seq_ip.push(m);
        };
        let call_sl = loop {
            let m = client_sl.recv_timeout(Duration::from_secs(10));
            if let FromServer::Request {
                id,
                call: ServerCall::Approve { auth_id, .. },
            } = &m
                && auth_id == "a1"
            {
                seq_sl.push(m.clone());
                break id.clone();
            }
            seq_sl.push(m);
        };
        for (id, serde_path) in [(&call_ip, false), (&call_sl, true)] {
            let reply = FromClient::Reply {
                id: id.clone(),
                outcome: Ok(json!({"allow": true})),
            };
            if serde_path {
                client_sl.send(reply);
            } else {
                client_ip.send(reply);
            }
        }

        // Drain both until each has answered a final `ListThreads` request
        // (bounded settle through the same dispatch queue, no sleeps beyond
        // the recv polling). T10: this was the v1 `ThreadInfo` query — any
        // order-guaranteed read call serves the rendezvous; the comparison
        // here is transport identity, not the payload's content.
        for send_ip in [true, false] {
            let req = FromClient::Request {
                id: MsgId::new("settle"),
                call: ClientCall::ListThreads,
            };
            if send_ip {
                client_ip.send(req);
            } else {
                client_sl.send(req);
            }
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut got_ip_info = false;
        let mut got_sl_info = false;
        while std::time::Instant::now() < deadline && !(got_ip_info && got_sl_info) {
            if !got_ip_info && let Ok(m) = client_ip.conn.server_rx().try_recv() {
                got_ip_info = matches!(m, FromServer::Response { .. });
                seq_ip.push(m);
            }
            if !got_sl_info && let Ok(m) = client_sl.conn.server_rx().try_recv() {
                got_sl_info = matches!(m, FromServer::Response { .. });
                seq_sl.push(m);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            got_ip_info && got_sl_info,
            "both paths answered ListThreads"
        );

        // ── §D.1 stream round (T4 extension) ──
        //
        // The stream half reuses each path's scripted engine: seed an
        // identical journal, open a follow stream, await the snapshot, push
        // one live entry, await it, then cancel. Transport-identical frames
        // are the assertion (the §C.3 read seam itself is exercised through
        // the engine's `journal_snapshot`/`subscribe_journal_feed`, which
        // `open_stream_emits_snapshot_then_gap_free_entries` pins end-to-end
        // against a live PiEngine file).
        let seed_journal = |engine: &FakeEngine| {
            engine.set_journal(
                1,
                vec![
                    JournalRecord {
                        seq: 0,
                        entry: (*jentry("e-0", None, ent_turn_start)).clone(),
                    },
                    JournalRecord {
                        seq: 1,
                        entry: (*jentry("e-1", Some("e-0"), ent_turn_finish)).clone(),
                    },
                ],
            );
        };
        seed_journal(&engine_ip);
        seed_journal(&engine_sl);

        open_follow(&client_ip, "stream-1", "sess-inproc");
        client_sl.send(FromClient::StreamOpen {
            stream_id: StreamId::new("stream-1"),
            stream_kind: StreamKind::FollowSession {
                session_id: "sess-serde".into(),
                max_messages: None,
            },
        });
        // Snapshot-first (§F.1 rule 1): collect until the opening frame on
        // each path (anything ahead of it is shared noise, pushed verbatim).
        for (path_ip, seq) in [(true, &mut seq_ip), (false, &mut seq_sl)] {
            let is_snapshot = |m: &FromServer| {
                matches!(
                    m,
                    FromServer::StreamItem {
                        frame: manox_protocol::StreamFrame::Snapshot(_),
                        ..
                    }
                )
            };
            let mut arrived = false;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !arrived {
                let m = if path_ip {
                    client_ip.recv_timeout(Duration::from_secs(10))
                } else {
                    client_sl.recv_timeout(Duration::from_secs(10))
                };
                arrived = is_snapshot(&m);
                seq.push(m);
                assert!(std::time::Instant::now() < deadline, "snapshot frame lost");
            }
        }

        // One live append forwarded as an Entry frame (seq 2, after the
        // cursor at 1).
        for engine in [&engine_ip, &engine_sl] {
            engine.push_journal(2, jentry("e-2", Some("e-1"), ent_stop));
        }
        for (path_ip, seq) in [(true, &mut seq_ip), (false, &mut seq_sl)] {
            let is_entry = |m: &FromServer| {
                matches!(
                    m,
                    FromServer::StreamItem {
                        frame: manox_protocol::StreamFrame::Entry { seq: 2, .. },
                        ..
                    }
                )
            };
            let mut arrived = false;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !arrived {
                let m = if path_ip {
                    client_ip.recv_timeout(Duration::from_secs(10))
                } else {
                    client_sl.recv_timeout(Duration::from_secs(10))
                };
                arrived = is_entry(&m);
                seq.push(m);
                assert!(std::time::Instant::now() < deadline, "entry frame lost");
            }
        }

        // Cancel both streams; the terminal StreamEnd rides the same
        // transport and must match too.
        client_ip.send(FromClient::StreamCancel {
            stream_id: StreamId::new("stream-1"),
        });
        client_sl.send(FromClient::StreamCancel {
            stream_id: StreamId::new("stream-1"),
        });
        for (path_ip, seq) in [(true, &mut seq_ip), (false, &mut seq_sl)] {
            let mut ended = false;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !ended {
                let m = if path_ip {
                    client_ip.recv_timeout(Duration::from_secs(10))
                } else {
                    client_sl.recv_timeout(Duration::from_secs(10))
                };
                ended = matches!(m, FromServer::StreamEnd { .. });
                seq.push(m);
                assert!(
                    std::time::Instant::now() < deadline,
                    "stream end frame lost"
                );
            }
        }

        // ── Normalize + compare ──
        let normalize = |msgs: Vec<FromServer>| -> Vec<serde_json::Value> {
            /// Stream frames carry wall-clock stamps (file timestamps from
            /// the two independent seeds); scrub them to one value — the
            /// transport-identity claim is about the frame protocol, not the
            /// seed moments.
            fn scrub(v: &mut serde_json::Value) {
                match v {
                    Value::Object(map) => {
                        for (k, child) in map.iter_mut() {
                            if k == "timestamp" || k == "createdAt" {
                                *child = Value::String("TS".into());
                            } else {
                                scrub(child);
                            }
                        }
                    }
                    Value::Array(items) => items.iter_mut().for_each(scrub),
                    other => {
                        let _ = other;
                    }
                }
            }
            msgs.into_iter()
                .filter(|m| {
                    !matches!(
                        m,
                        FromServer::Notification { note }
                            if note.backpressure_policy() == BackpressurePolicy::Drop
                    )
                })
                .map(|m| {
                    let mut v = serde_json::to_value(&m).expect("serializable");
                    scrub(&mut v);
                    let s = v.to_string();
                    let s = s
                        .replace("sess-inproc", "SESS")
                        .replace("sess-serde", "SESS");
                    serde_json::from_str(&s).expect("re-parses")
                })
                .collect()
        };
        let (nip, nsl) = (normalize(seq_ip), normalize(seq_sl));
        // Host frames (§D.5) originate on the pump task, whose interleaving
        // with serve-loop frames is scheduler-dependent — equivalence is
        // exact-order for everything else + multiset for host frames.
        let split = |seq: Vec<serde_json::Value>| {
            let mut hosts: Vec<serde_json::Value> = Vec::new();
            let rest: Vec<serde_json::Value> = seq
                .into_iter()
                .filter(|v| {
                    let is_host = v.get("kind").and_then(|k| k.as_str()) == Some("host");
                    if is_host {
                        hosts.push(v.clone());
                    }
                    !is_host
                })
                .collect();
            hosts.sort_by_key(|h| h.to_string());
            (rest, hosts)
        };
        let (rest_ip, hosts_ip) = split(nip);
        let (rest_sl, hosts_sl) = split(nsl);
        assert_eq!(
            rest_ip, rest_sl,
            "in-process and serde paths must produce identical FromServer sequences (non-host frames)"
        );
        assert_eq!(
            hosts_ip, hosts_sl,
            "host frames must be identical as a multiset across transports"
        );

        drop(client_ip);
        drop(client_sl);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// ε-2b: multi-client routing — two clients, two sessions, one server.
    /// Session-scoped notes reach every owner; §D.5 host deltas reach every
    /// connection (global broadcast). The spec is the observed behavior.
    #[test]
    fn multi_client_broadcast_and_dispose_semantics() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client_a) = harness(vec![]);
        let (client_b_conn, server_b_conn) = in_process_pair();
        server.accept(std::sync::Arc::new(server_b_conn));
        let client_b = Client {
            conn: client_b_conn,
        };
        client_b.send(FromClient::Request {
            id: MsgId::new("init-b"),
            call: ClientCall::Initialize(Initialize {
                client_id: "test-b".into(),
                capabilities: vec![],
                sessions: vec![],
                protocol_epoch: PROTOCOL_EPOCH,
            }),
        });
        assert!(matches!(client_b.recv(), FromServer::Response { .. }));
        assert!(matches!(
            client_b.recv(),
            FromServer::Notification {
                note: ServerNote::Ready
            }
        ));

        // Each client owns its own session; both creations land.
        create(&server, &client_a, "sa");
        create(&server, &client_b, "sb");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("sa", engine.clone(), events);

        // A v2 session-scoped domain signal for sa must reach EVERY client:
        // `SessionStatus` host deltas are broadcast globally (§D.5), not
        // owner-routed like `route_note` — the replaceable-note domain moved
        // onto this global lane in T10, so the ownership routing proof rides
        // it now. Injecting `TurnStarted` (a doomed-note event in v1) is also
        // a pin that the pump emits NO session-scoped Notification for it.
        engine
            .notices
            .send(BackendNotice::Event(Box::new(ThreadEvent::TurnStarted)))
            .unwrap();
        // b is a non-owner: it must still get sa's host frame (broadcast),
        // while every owner-scoped frame it might hold (Ready, its own
        // create acks, ...) may drain past.
        expect(&client_b, |m| {
            matches!(
                m,
                FromServer::Host {
                    host: HostEvent::SessionStatus {
                        session_id,
                        running: Some(true),
                        ..
                    }
                } if session_id == "sa"
            )
        });
        // Drain everything still queued on both clients and classify.
        let drain = |c: &Client| {
            let mut v = Vec::new();
            while let Ok(m) = c.conn.server_rx().try_recv() {
                v.push(m);
            }
            v
        };
        let a_pending = drain(&client_a);
        let b_pending = drain(&client_b);
        // Spec: (1) neither owner sees a domain-note Notification — translate
        // no longer mirrors any session-domain note for the turn edge (the
        // turn arms are gone from the enum post-T10; `Error` survives as the
        // server-originated channel, which translate must not use either);
        // (2) every session-scoped host frame a or b holds belongs to sa —
        // only sa's engine is wired, so no foreign session can leak.
        let doomed = |m: &FromServer| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::Error { .. }
                }
            )
        };
        let foreign = |m: &FromServer| {
            matches!(
                m,
                FromServer::Host {
                    host: HostEvent::SessionStatus { session_id, .. }
                } if session_id != "sa"
            )
        };
        assert!(
            !a_pending.iter().any(doomed) && !b_pending.iter().any(doomed),
            "translate must emit no session-domain notes (v1 mirrors removed): a={a_pending:?} b={b_pending:?}"
        );
        assert!(
            !a_pending.iter().any(foreign) && !b_pending.iter().any(foreign),
            "no foreign-session host frames may reach either client: a={a_pending:?} b={b_pending:?}"
        );

        // Dispose: each client detaches its own session; the other is
        // unaffected and the server keeps serving.
        client_a.send(FromClient::Notification {
            note: ClientNote::DisposeSession {
                session_id: "sa".into(),
            },
        });
        // Liveness proof for b after a's dispose, independent of the
        // process-global provider registry and of any session state: a plain
        // read call must get a Response (server alive, connection served),
        // never a dropped connection. T10: was `GetCurrentModel` — the v1
        // per-session query is gone; the transport liveness proof does not
        // need one. (The earlier "submit -> Error note" proof was
        // order-fragile: it relied on sb's engine bailing with no default
        // model, which a prior model-registering test defeats.)
        client_b.send(FromClient::Request {
            id: MsgId::new("b-alive"),
            call: ClientCall::ListThreads,
        });
        expect(&client_b, |m| {
            matches!(m, FromServer::Response { outcome: Ok(_), .. })
        });

        drop(client_a);
        drop(client_b);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn reinitialize_same_client_id_reseats_and_reopen_loads() {
        let _g = lock_globals();
        hermetic_home();
        let sessions = manox_agent::paths::manox_config_dir()
            .expect("config dir")
            .join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        seed_session_file(&sessions, "s1", "/proj");
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        // First open of s1: must succeed (ack + SessionCreated; T10: the v1
        // snapshot push is gone — history replays via the follow stream).
        client.send(FromClient::Request {
            id: MsgId::new("open-1"),
            call: ClientCall::OpenSession {
                session_id: "s1".into(),
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionCreated { session_id } } if session_id == "s1"),
        );
        expect(
            &client,
            |m| matches!(m, FromServer::Response { id, outcome: Ok(_), .. } if id.0 == "open-1"),
        );
        // Simulate reconnect: a second connection with the same client_id.
        let (client_reconn_conn, server_reconn_conn) = in_process_pair();
        server.accept(std::sync::Arc::new(server_reconn_conn));
        let client_reconn = Client {
            conn: client_reconn_conn,
        };
        client_reconn.send(FromClient::Request {
            id: MsgId::new("init-reconn"),
            call: ClientCall::Initialize(Initialize {
                client_id: "test".into(),
                capabilities: vec![],
                sessions: vec![],
                protocol_epoch: PROTOCOL_EPOCH,
            }),
        });
        // Must NOT be rejected — must get ack + Ready.
        let resp = client_reconn.recv();
        assert!(
            matches!(resp, FromServer::Response { outcome: Ok(_), .. }),
            "reconnect must not be rejected: {resp:?}"
        );
        let ready = client_reconn.recv();
        assert!(
            matches!(
                ready,
                FromServer::Notification {
                    note: ServerNote::Ready
                }
            ),
            "reconnect must receive Ready: {ready:?}"
        );
        // C1/GW1: a re-seat handshake echoes the epoch on the Host lane too.
        let host_ready = client_reconn.recv();
        assert!(
            matches!(
                host_ready,
                FromServer::Host {
                    host: HostEvent::Ready {
                        epoch: PROTOCOL_EPOCH
                    }
                }
            ),
            "reconnect must receive the Host Ready epoch echo: {host_ready:?}"
        );
        // Reopen s1 on the new connection: must load the session again —
        // directed ack, and the v2 replay lane (follow stream) answers the
        // re-seated owner.
        client_reconn.send(FromClient::Request {
            id: MsgId::new("open-reconn"),
            call: ClientCall::OpenSession {
                session_id: "s1".into(),
            },
        });
        expect(
            &client_reconn,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionCreated { session_id } } if session_id == "s1"),
        );
        expect(
            &client_reconn,
            |m| matches!(m, FromServer::Response { id, outcome: Ok(_), .. } if id.0 == "open-reconn"),
        );
        let (engine, events) = FakeEngine::new();
        engine.set_journal(
            0,
            vec![JournalRecord {
                seq: 0,
                entry: (*jentry("e-0", None, ent_turn_start)).clone(),
            }],
        );
        server.set_session_engine_for_test("s1", engine.clone(), events);
        open_follow(&client_reconn, "st-reopen", "s1");
        let snap = snapshot_for(&client_reconn, "st-reopen");
        assert_eq!(snap.session_id, "s1");
        assert_eq!(snap.records.len(), 1);
        drop(client);
        drop(client_reconn);
        drop(server);
        // GW11 hygiene: create() with an id whose journal file exists
        // on disk now RESTORES it; remove this test's seed so the shared
        // hermetic sessions dir never hijacks a later test's create("s1").
        let _ = std::fs::remove_file(sessions.join("s1.jsonl"));
        manox_agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn add_owner_is_idempotent_no_duplicate_notes() {
        // A fresh session (no disk restore, so no racing engine drain), then a
        // second idempotent `OpenSession` from the same client. If `add_owner`
        // pushed a duplicate owner entry, the turn's event would be routed to
        // the same connection twice. `expect` returns on the FIRST match and
        // then asserts nothing else is queued — proving single delivery.
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("s1", engine.clone(), events);
        // Second open from the same client — the idempotent reopen path calls
        // `add_owner` again; it must not add a duplicate owner.
        client.send(FromClient::Request {
            id: MsgId::new("open-2"),
            call: ClientCall::OpenSession {
                session_id: "s1".into(),
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionCreated { session_id } } if session_id == "s1"),
        );
        // T10: the reopen pushes no v1 snapshot — only the directed
        // `SessionCreated` plus the ack above. The channel needs no drain.
        expect(
            &client,
            |m| matches!(m, FromServer::Response { id, outcome: Ok(_), .. } if id.0 == "open-2"),
        );
        // Drive one turn: exactly one turn edge (v2: `SessionStatus`
        // running=true host delta, §D.5) must reach the client.
        client.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "s1".into(),
                text: "idempotency probe".into(),
                images: vec![],
                client_id: None,
            },
        });
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(true));
        // Nothing further may be queued: a duplicate owner would have delivered
        // a second turn edge here.
        std::thread::sleep(Duration::from_millis(100));
        let mut extras = Vec::new();
        while let Ok(extra) = client.conn.server_rx().try_recv() {
            extras.push(extra);
        }
        assert!(
            extras.is_empty(),
            "duplicate delivery after idempotent reopen: {extras:?}"
        );
        engine
            .notices
            .send(BackendNotice::Settled {
                cancelled: false,
                failed: false,
                steered: Vec::new(),
                stranded: Vec::new(),
            })
            .unwrap();
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(false));
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    // ── T-D regression: one connection multiplexing many sessions; the
    //    Detach/Open semantics that make idle-switch leak-free. ─────────────

    /// One client owns two sessions on the shared connection; both
    /// `SessionCreated` arrive on it and the server lists the client as the
    /// sole owner of each — the ownership table the multiplexer demuxes on.
    #[test]
    fn single_connection_multiplexes_multiple_sessions() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);
        create(&server, &client, "sa");
        create(&server, &client, "sb");
        assert_eq!(server.0.owners("sa"), vec!["test".to_string()]);
        assert_eq!(server.0.owners("sb"), vec!["test".to_string()]);
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// `DetachSession` releases the server-side owner without killing the
    /// client; the detaching client is told `SessionDisposed` and `owners()`
    /// is empty afterwards — the pre-multiplex idle-switch leak is gone.
    #[test]
    fn detach_session_releases_owner_no_leak() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        assert_eq!(server.0.owners("s1"), vec!["test".to_string()]);
        client.send(FromClient::Notification {
            note: ClientNote::DetachSession {
                session_id: "s1".into(),
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionDisposed { session_id } } if session_id == "s1"),
        );
        assert!(
            server.0.owners("s1").is_empty(),
            "DetachSession must release the owner"
        );
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW2 follow-up regression: detaching the last owner WHILE a turn runs
    /// defers the pump stop — the settle bookkeeping (store flags + the
    /// SessionStatus edges) belongs to the pump, and killing it at detach
    /// stranded the store's `running` flag with nobody left to clear it.
    /// The TurnFinished arm reaps the orphan: the entry leaves the table,
    /// the pump terminates, and the settle edge still broadcasts.
    #[test]
    fn detach_while_running_defers_reap_until_settle() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("s1", engine.clone(), events);
        // The turn starts: the pump marks it active and broadcasts.
        engine
            .notices
            .send(BackendNotice::Event(Box::new(ThreadEvent::TurnStarted)))
            .unwrap();
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(true));

        // The last owner detaches mid-turn.
        client.send(FromClient::Notification {
            note: ClientNote::DetachSession {
                session_id: "s1".into(),
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionDisposed { session_id } } if session_id == "s1"),
        );
        assert!(
            server.0.owners("s1").is_empty(),
            "DetachSession must release the owner"
        );
        // Deferred: the entry and its pump survive the detach while the
        // turn is in flight.
        assert!(
            server.0.session_thread("s1").is_some(),
            "a detached-but-running session keeps its entry until settle"
        );
        expect_live_pumps(&server, 1, "detach mid-turn defers the pump stop");

        // Settle: the pump runs the bookkeeping, broadcasts the edge, and
        // reaps the orphan.
        engine
            .notices
            .send(BackendNotice::Settled {
                cancelled: false,
                failed: false,
                steered: Vec::new(),
                stranded: Vec::new(),
            })
            .unwrap();
        expect_host_status(&client, "s1", |running, _, _, _| running == Some(false));
        expect_live_pumps(&server, 0, "the orphan is reaped at settle");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while server.0.session_thread("s1").is_some() {
            assert!(
                std::time::Instant::now() < deadline,
                "the settled orphan's entry never left the table"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// Reopening a detached session is idempotent: the persisted thread
    /// survives detach (only the in-memory owner is dropped), so a later
    /// `OpenSession` re-adds the owner and the v2 replay lane — the follow
    /// stream's opening `Snapshot` — delivers the history (T10: replaced the
    /// `ThreadHistory { restored: true }` push).
    #[test]
    fn detach_then_reopen_replays_history() {
        let _g = lock_globals();
        hermetic_home();
        let sessions = manox_agent::paths::manox_config_dir()
            .expect("config dir")
            .join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        seed_session_file(&sessions, "s1", "/proj");
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        client.send(FromClient::Request {
            id: MsgId::new("open"),
            call: ClientCall::OpenSession {
                session_id: "s1".into(),
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionCreated { session_id } } if session_id == "s1"),
        );
        // Detach drops the owner; the disk file survives.
        client.send(FromClient::Notification {
            note: ClientNote::DetachSession {
                session_id: "s1".into(),
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionDisposed { session_id } } if session_id == "s1"),
        );
        assert!(server.0.owners("s1").is_empty());
        // Reopen: idempotent load from disk → re-added owner + ack.
        client.send(FromClient::Request {
            id: MsgId::new("reopen"),
            call: ClientCall::OpenSession {
                session_id: "s1".into(),
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionCreated { session_id } } if session_id == "s1"),
        );
        expect(
            &client,
            |m| matches!(m, FromServer::Response { id, outcome: Ok(_), .. } if id.0 == "reopen"),
        );
        assert_eq!(server.0.owners("s1"), vec!["test".to_string()]);
        // The v2 replay: the reopened session's history arrives through the
        // follow stream's `Snapshot` (scripted read seam as in
        // `open_session_replays_thread_history`).
        let (engine, events) = FakeEngine::new();
        engine.set_journal(
            1,
            vec![
                JournalRecord {
                    seq: 0,
                    entry: (*jentry("e-0", None, ent_turn_start)).clone(),
                },
                JournalRecord {
                    seq: 1,
                    entry: (*jentry("e-1", Some("e-0"), ent_turn_finish)).clone(),
                },
            ],
        );
        server.set_session_engine_for_test("s1", engine.clone(), events);
        open_follow(&client, "st-reopen", "s1");
        let snap = snapshot_for(&client, "st-reopen");
        assert_eq!(snap.session_id, "s1");
        assert_eq!(snap.cursor, 1);
        assert_eq!(snap.records.len(), 2, "replay carries the whole chain");
        drop(client);
        drop(server);
        // GW11 hygiene: create() with an id whose journal file exists
        // on disk now RESTORES it; remove this test's seed so the shared
        // hermetic sessions dir never hijacks a later test's create("s1").
        let _ = std::fs::remove_file(sessions.join("s1.jsonl"));
        manox_agent::thread_store::drop_global_for_test();
    }

    // ── T4: §D.1 follow streams (§F server side). ─────────────────────────

    /// A snapshot-first open followed by strictly gap-free live entries:
    /// `Snapshot.cursor == journal_cursor` (the seeded chain end), then
    /// Entry frames with consecutive dense seqs starting at cursor+1
    /// (§F.1 rule 1/2 — the client engine's opening contract).
    #[test]
    fn open_stream_emits_snapshot_then_gap_free_entries() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        // Seed the whole-chain read: two dense records, cursor = 1.
        engine.set_journal(
            1,
            vec![
                JournalRecord {
                    seq: 0,
                    entry: (*jentry("e-0", None, ent_turn_start)).clone(),
                },
                JournalRecord {
                    seq: 1,
                    entry: (*jentry("e-1", Some("e-0"), ent_turn_finish)).clone(),
                },
            ],
        );
        server.set_session_engine_for_test("s1", engine.clone(), events);
        open_follow(&client, "st-1", "s1");

        let snapshot = loop {
            match client.recv() {
                FromServer::StreamItem {
                    stream_id,
                    frame: manox_protocol::StreamFrame::Snapshot(s),
                } if stream_id.0 == "st-1" => break s,
                // Leftover v1 push (the compat CreateSession still emits
                // its notes in the dual-protocol window) is skipped; the
                // FIRST STREAM frame must be the snapshot (§F.1 rule 1).
                FromServer::Notification { .. } => continue,
                // GW1 dual-emit Host mirrors are skipped likewise.
                FromServer::Host { .. } => continue,
                other => {
                    panic!("first stream frame must be the Snapshot, got {other:?}");
                }
            }
        };
        assert_eq!(snapshot.session_id, "s1");
        // Snapshot cursor equals the journal cursor: the read is the
        // engine's whole active chain (§C.3), whose tail stamp is the cursor.
        assert_eq!(snapshot.cursor, 1);
        assert_eq!(snapshot.records.len(), 2);
        assert_eq!(snapshot.records[0].seq, 0);
        assert_eq!(snapshot.records[1].seq, 1);
        assert!(!snapshot.has_more);
        // §D.1 T4 scope: empty projection baseline, stamped at the cursor
        // (the registry is T5).
        // T5: the snapshot baseline carries exactly the declared projection
        // surface (§E.2 / L12) — the registry seeded from the thread and
        // folded over the snapshot records.
        let mut want: Vec<&str> = manox_protocol::surface::PROJECTION_KEYS.to_vec();
        want.sort_unstable();
        let mut got: Vec<&str> = snapshot.projections.keys().map(String::as_str).collect();
        got.sort_unstable();
        assert_eq!(
            got, want,
            "snapshot baseline IS the declared projection surface"
        );
        assert_eq!(snapshot.projections_as_of_seq, snapshot.cursor);
        assert_eq!(snapshot.header.id, "s1");

        // Live entries: a varied, dense run of §C.2 rows forwarded as
        // gap-free Entry frames — each maps through `translate::wire_event`
        // and continues the cursor by exactly one (§F.1 rule 2).
        type EntryBuilder = fn(String, Option<String>) -> SessionTreeEntry;
        let live: Vec<(u64, EntryBuilder, &str)> = vec![
            (2, ent_agent_text_delta, "agentTextDelta"),
            (3, ent_tool_call, "toolCall"),
            (4, ent_model_change, "modelChange"),
            (5, ent_permission_mode_change, "permissionModeChange"),
            (6, ent_title, "title"),
            (7, ent_goal, "goal"),
            (8, ent_ui_note, "uiNote"),
            (9, ent_error_event, "error"),
        ];
        for (seq, make, _tag) in &live {
            engine.push_journal(
                *seq,
                jentry(&format!("e-{seq}"), Some(&format!("e-{}", seq - 1)), *make),
            );
        }
        let mut last = snapshot.cursor;
        let mut projection_frames = 0usize;
        for (seq, _make, tag) in &live {
            let (got, event) = loop {
                match client.recv() {
                    FromServer::StreamItem {
                        frame: manox_protocol::StreamFrame::Entry { seq, event, .. },
                        ..
                    } => break (seq, event),
                    // The P face interleaves changed-key frames after the
                    // entries that produced them (§E.1); count them for the
                    // assertion below.
                    FromServer::StreamItem {
                        frame: manox_protocol::StreamFrame::Projections(frame),
                        ..
                    } => {
                        assert!(
                            frame.as_of_seq <= last + 1,
                            "projection stamp stays with the stream cursor"
                        );
                        projection_frames += 1;
                        continue;
                    }
                    // Compat-window v1 push may interleave; skip it.
                    FromServer::Notification { .. } => continue,
                    // GW1 dual-emit Host mirrors may interleave; skip them.
                    FromServer::Host { .. } => continue,
                    other => panic!("expected Entry frames, got {other:?}"),
                }
            };
            // Gap-free: every entry continues the cursor immediately.
            assert_eq!(got, last + 1, "gap-free Entry stream (§F.1 rule 2)");
            assert_eq!(got, *seq);
            assert_eq!(
                serde_json::to_value(&event).unwrap()["type"].as_str(),
                Some(*tag),
                "wire tag for seq {seq}"
            );
            last = got;
        }
        assert_eq!(last, 9, "cursor advanced across the whole live run");
        // The scripted live run contains state changes (turn finish, model
        // change, …) — the P face must have published at least one delta.
        assert!(projection_frames > 0, "P face publishes changed keys");
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// `StreamCancel` terminates the stream with exactly one
    /// `StreamEnd { Cancelled }` and nothing after it (§D.1).
    #[test]
    fn stream_cancel_ends_stream() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        engine.set_journal(
            0,
            vec![JournalRecord {
                seq: 0,
                entry: (*jentry("e-0", None, ent_turn_start)).clone(),
            }],
        );
        server.set_session_engine_for_test("s1", engine.clone(), events);
        open_follow(&client, "st-1", "s1");
        expect(&client, |m| {
            matches!(
                m,
                FromServer::StreamItem {
                    frame: manox_protocol::StreamFrame::Snapshot(_),
                    ..
                }
            )
        });
        client.send(FromClient::StreamCancel {
            stream_id: StreamId::new("st-1"),
        });
        expect(&client, |m| {
            matches!(
                m,
                FromServer::StreamEnd {
                    stream_id,
                    reason: manox_protocol::StreamEndReason::Cancelled,
                } if stream_id.0 == "st-1"
            )
        });
        // Nothing may follow the terminal frame; a late entry or a second
        // StreamEnd would violate the §F.1 contract.
        let rx = client.conn.server_rx();
        let deadline = std::time::Instant::now() + Duration::from_millis(200);
        while std::time::Instant::now() < deadline {
            assert!(
                rx.try_recv().is_err(),
                "traffic after StreamEnd on a cancelled stream"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// One connection, many streams (§D.1): two follow streams on two
    /// sessions interleave without cross-talk — cancel one, the other keeps
    /// delivering.
    #[test]
    fn multi_stream_one_connection() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        create(&server, &client, "s2");
        let (engine1, events1) = FakeEngine::new();
        engine1.set_journal(
            0,
            vec![JournalRecord {
                seq: 0,
                entry: (*jentry("a-0", None, ent_turn_start)).clone(),
            }],
        );
        server.set_session_engine_for_test("s1", engine1.clone(), events1);
        let (engine2, events2) = FakeEngine::new();
        engine2.set_journal(
            0,
            vec![JournalRecord {
                seq: 0,
                entry: (*jentry("b-0", None, ent_title)).clone(),
            }],
        );
        server.set_session_engine_for_test("s2", engine2.clone(), events2);

        open_follow(&client, "st-a", "s1");
        open_follow(&client, "st-b", "s2");
        // Each stream opens with its own snapshot (dense, per session).
        let mut snap_a = false;
        let mut snap_b = false;
        while !(snap_a && snap_b) {
            match client.recv() {
                FromServer::StreamItem {
                    stream_id,
                    frame: manox_protocol::StreamFrame::Snapshot(s),
                } => {
                    if stream_id.0 == "st-a" {
                        assert_eq!(s.session_id, "s1");
                        snap_a = true;
                    } else {
                        assert_eq!(stream_id.0, "st-b");
                        assert_eq!(s.session_id, "s2");
                        snap_b = true;
                    }
                }
                // Compat-window v1 notes are drained; any other frame on
                // an unopened stream would be cross-talk.
                FromServer::Notification { .. } => continue,
                // GW1 dual-emit Host mirrors are drained likewise.
                FromServer::Host { .. } => continue,
                other => panic!("expected snapshots on both streams, got {other:?}"),
            }
        }
        // Live entries route to the right stream only.
        engine1.push_journal(1, jentry("a-1", Some("a-0"), ent_agent_text_delta));
        let got = loop {
            match client.recv() {
                FromServer::StreamItem { stream_id, frame } => break (stream_id, frame),
                FromServer::Notification { .. } => continue,
                FromServer::Host { .. } => continue,
                other => panic!("expected a live entry, got {other:?}"),
            }
        };
        assert_eq!(got.0.0, "st-a", "entries must not cross streams");
        assert!(matches!(
            got.1,
            manox_protocol::StreamFrame::Entry { seq: 1, .. }
        ));
        // Cancel one; the other stays live.
        client.send(FromClient::StreamCancel {
            stream_id: StreamId::new("st-a"),
        });
        expect(&client, |m| {
            matches!(
                m,
                FromServer::StreamEnd { stream_id, reason: manox_protocol::StreamEndReason::Cancelled }
                    if stream_id.0 == "st-a"
            )
        });
        engine2.push_journal(1, jentry("b-1", Some("b-0"), ent_agent_text_delta));
        expect(&client, |m| {
            matches!(
                m,
                FromServer::StreamItem { stream_id, .. } if stream_id.0 == "st-b"
            )
        });
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// Dispose closes every live stream of the session (§D.1 `Closed`):
    /// server-side termination, never a silent stall.
    #[test]
    fn dispose_session_closes_streams() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        engine.set_journal(
            0,
            vec![JournalRecord {
                seq: 0,
                entry: (*jentry("e-0", None, ent_turn_start)).clone(),
            }],
        );
        server.set_session_engine_for_test("s1", engine, events);
        open_follow(&client, "st-1", "s1");
        expect(&client, |m| {
            matches!(
                m,
                FromServer::StreamItem {
                    frame: manox_protocol::StreamFrame::Snapshot(_),
                    ..
                }
            )
        });
        client.send(FromClient::Notification {
            note: ClientNote::DisposeSession {
                session_id: "s1".into(),
            },
        });
        expect(&client, |m| {
            matches!(
                m,
                FromServer::StreamEnd {
                    stream_id,
                    reason: manox_protocol::StreamEndReason::Closed,
                } if stream_id.0 == "st-1"
            )
        });
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    // ── T4: §D.2 PageHistory + §E.3 GetConversationInfo. ──────────────────

    fn request(client: &Client, id: &str, call: ClientCall) -> FromServer {
        client.send(FromClient::Request {
            id: MsgId::new(id),
            call,
        });
        loop {
            let m = client.recv();
            if matches!(m, FromServer::Response { .. }) {
                return m;
            }
        }
    }

    fn response_outcome(m: FromServer) -> Value {
        match m {
            FromServer::Response { outcome: Ok(v), .. } => v,
            FromServer::Response {
                outcome: Err(e), ..
            } => panic!("expected Ok response, got err {e:?}"),
            other => panic!("expected a Response, got {other:?}"),
        }
    }

    /// Cold chain pages round-trip through the §D.2 PageHistory surface:
    /// `{records, has_more, cursor}` over the seeded chain (dense seq,
    /// §F.1-compatible), through the real `translate::wire_entry` mapping.
    #[test]
    fn page_history_cold_read_round_trips() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        engine.set_journal(
            3,
            vec![
                JournalRecord {
                    seq: 0,
                    entry: (*jentry("e-0", None, ent_turn_start)).clone(),
                },
                JournalRecord {
                    seq: 1,
                    entry: (*jentry("e-1", Some("e-0"), ent_agent_text_delta)).clone(),
                },
                JournalRecord {
                    seq: 2,
                    entry: (*jentry("e-2", Some("e-1"), ent_tool_call)).clone(),
                },
                JournalRecord {
                    seq: 3,
                    entry: (*jentry("e-3", Some("e-2"), ent_tool_result)).clone(),
                },
            ],
        );
        server.set_session_engine_for_test("s1", engine.clone(), events);

        // Latest page: the whole chain, dense, oldest-first.
        let v = response_outcome(request(
            &client,
            "ph-1",
            ClientCall::PageHistory {
                session_id: "s1".into(),
                through_seq: -1,
                before_seq: None,
                max_messages: None,
            },
        ));
        assert_eq!(v["cursor"], 3);
        assert_eq!(v["has_more"], false);
        assert_eq!(v["records"].as_array().unwrap().len(), 4);
        for (i, r) in v["records"].as_array().unwrap().iter().enumerate() {
            assert_eq!(r["seq"], i as u64, "dense seq, oldest first");
        }
        assert_eq!(v["records"][0]["type"], "turnStart");
        assert_eq!(v["records"][3]["type"], "toolResult");
        assert_eq!(v["records"][3]["callId"], "tc-1", "C.1 handle rename");

        // Backwards page: strictly before seq 2, capped at 1 message —
        // has_more surfaces the older prefix.
        let v = response_outcome(request(
            &client,
            "ph-2",
            ClientCall::PageHistory {
                session_id: "s1".into(),
                through_seq: -1,
                before_seq: Some(2),
                max_messages: Some(1),
            },
        ));
        let recs = v["records"].as_array().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0]["seq"], 1);
        assert_eq!(v["cursor"], 1);
        assert_eq!(v["has_more"], true, "seq 0 predates the window");

        // The page round-trips the real wire type (§J.5 serde shape).
        let back: Vec<manox_protocol::JournalWireEntry> =
            serde_json::from_value(v["records"].clone()).expect("wire records parse");
        assert_eq!(
            back[0].event,
            manox_protocol::JournalWireEvent::AgentTextDelta { s: "tok".into() }
        );
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// §E.3: turns count `turn_start` rows, messages count `message` rows,
    /// `models[]` aggregates assistant usage by canonical model.
    #[test]
    fn conversation_info_folds_usage() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        let msg =
            |id: String, parent: Option<String>, message: manox_harness::types::AgentMessage| {
                SessionTreeEntry::Message {
                    id,
                    parent_id: parent,
                    timestamp: chrono::Utc::now(),
                    message,
                    origin: None,
                }
            };
        engine.set_journal(
            3,
            vec![
                JournalRecord {
                    seq: 0,
                    entry: (*jentry("c-0", None, ent_turn_start)).clone(),
                },
                JournalRecord {
                    seq: 1,
                    entry: msg(
                        "c-1".into(),
                        Some("c-0".into()),
                        manox_harness::types::AgentMessage::User {
                            content: vec![manox_harness::types::ContentBlock::Text {
                                text: "hi".into(),
                                signature: None,
                            }],
                            timestamp: chrono::Utc::now(),
                        },
                    ),
                },
                JournalRecord {
                    seq: 2,
                    entry: msg(
                        "c-2".into(),
                        Some("c-1".into()),
                        manox_harness::types::AgentMessage::Assistant {
                            content: vec![],
                            model: "m-1".into(),
                            provider: "test-prov".into(),
                            api: "anthropic".into(),
                            response_model: None,
                            response_id: None,
                            diagnostics: None,
                            stop_reason: None,
                            raw_stop_reason: None,
                            usage: Box::new(usage(10, 2, 1, 0, 0)),
                            error_message: None,
                            timestamp: chrono::Utc::now(),
                        },
                    ),
                },
                JournalRecord {
                    seq: 3,
                    entry: (*jentry("c-3", Some("c-2"), ent_turn_start)).clone(),
                },
            ],
        );
        server.set_session_engine_for_test("s1", engine.clone(), events);
        let v = response_outcome(request(
            &client,
            "ci-1",
            ClientCall::GetConversationInfo {
                session_id: "s1".into(),
            },
        ));
        assert_eq!(v["turns"], 2, "turn_start count");
        assert_eq!(v["messages"], 2, "message count");
        let models = v["models"].as_array().unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["provider"], "test-prov");
        // Canonical wire identity (L8).
        assert_eq!(models[0]["model"], "test-prov/m-1");
        assert_eq!(models[0]["input"], 10);
        assert_eq!(models[0]["output"], 2);
        assert_eq!(models[0]["cacheRead"], 1);
        assert_eq!(models[0]["calls"], 1);
        // T4 placeholders: cost is T5, git stays null, token-meter fields
        // null until the registry lands (§E.3 field sourcing).
        assert_eq!(v["cumulativeCost"], 0.0);
        assert!(v["git"].is_null());
        // contextWindow tracks the thread's model (null only when the
        // process-global provider registry happens to be empty); the
        // fold must at least carry the key.
        assert!(v.get("contextWindow").is_some());
        assert!(models[0]["hitRate"].is_null());
        assert!(models[0]["pct"].is_null());

        // §E.3 cache: the same cursor replays the cached fold byte-identical.
        let v2 = response_outcome(request(
            &client,
            "ci-2",
            ClientCall::GetConversationInfo {
                session_id: "s1".into(),
            },
        ));
        assert_eq!(v, v2);
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    fn usage(
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
        reasoning: u64,
    ) -> manox_harness::types::Usage {
        manox_harness::types::Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_write,
            cache_write_1h: None,
            reasoning_tokens: if reasoning > 0 { Some(reasoning) } else { None },
            total_tokens: input + output + cache_read + cache_write,
            cost: None,
        }
    }

    // ── T4: §D.2 request receipts + intent. ───────────────────────────────

    /// `ClientCall::Submit` answers with the §D.2 receipt and the durable
    /// path journals the submission: the engine sees the prompt, and the
    /// receipt carries the `message_id` of the user row. `origin_rpc` is
    /// accepted (receipt unchanged — the kernel origin row is a T5 type
    /// change, see the delivery report gap note).
    #[test]
    fn submit_request_returns_receipt_and_journals_origin() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("s1", engine.clone(), events);
        let v = response_outcome(request(
            &client,
            "sub-1",
            ClientCall::Submit {
                session_id: "s1".into(),
                text: "hello v2".into(),
                images: vec![],
                origin_rpc: Some("rpc-echo-9".into()),
            },
        ));
        assert_eq!(v["accepted"], true);
        // The receipt names the durable user row (message_id present and
        // stable across the journaling path).
        let message_id = v["message_id"].as_str().unwrap().to_string();
        assert!(!message_id.is_empty(), "receipt carries the message id");
        // The submission reached the engine and the transcript journaling
        // path (the kernel user row + the §C.2 message entry share the id).
        assert_eq!(engine.runs.lock().unwrap().as_slice(), ["hello v2"]);
        let v2 = response_outcome(request(
            &client,
            "sub-2",
            ClientCall::Submit {
                session_id: "s1".into(),
                text: "second".into(),
                images: vec![],
                origin_rpc: None,
            },
        ));
        assert_eq!(v2["accepted"], true);
        // Unknown session: §D.7 stable code, not a silent note.
        let m = request(
            &client,
            "sub-3",
            ClientCall::Submit {
                session_id: "nope".into(),
                text: "x".into(),
                images: vec![],
                origin_rpc: None,
            },
        );
        match m {
            FromServer::Response {
                outcome: Err(e), ..
            } => assert_eq!(e.data.unwrap()["code"], "session/not-found"),
            other => panic!("expected not-found err, got {other:?}"),
        }
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// §D.2 CreateSession intent: the session binds to `project` via the
    /// `new_in_project` kernel path (project == cwd origin, no orphaned
    /// pre-project file), `initial_model` resolves through the canonical
    /// `resolve_model_ref` (L8), and `approval_mode` / `reasoning_effort`
    /// land on the thread. An unresolvable model answers
    /// `model/unresolvable` (§D.7).
    #[test]
    fn create_session_with_project_intent() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        await_provider_registry();
        register_test_model("deepseek-chat");
        let (server, client) = harness(vec![]);
        let project = std::env::temp_dir().join("manox-t4-create-intent-proj");
        std::fs::create_dir_all(&project).unwrap();
        let v = response_outcome(request(
            &client,
            "cs-1",
            ClientCall::CreateSession {
                cwd: None,
                project: Some(project.to_string_lossy().into_owned()),
                initial_model: Some(manox_protocol::ModelRef::new(
                    "test-deepseek-chat/deepseek-chat",
                )),
                approval_mode: Some("read-only".into()),
                reasoning_effort: Some("high".into()),
            },
        ));
        let sid = v["session_id"].as_str().expect("session id").to_string();
        let thread = server.0.session_thread(&sid).expect("live session");
        let (proj, model, mode, effort) = thread.read(|t| {
            (
                t.project().map(|p| p.to_path_buf()),
                t.model().map(|m| (m.provider.clone(), m.id.clone())),
                t.permission_mode(),
                t.reasoning_effort().wire_value().to_string(),
            )
        });
        assert_eq!(
            proj.as_deref(),
            Some(project.as_path()),
            "new_in_project binding"
        );
        // Canonical resolution (L8): the wire registration name, not a bare
        // id, selects the model — provider + id both applied.
        assert_eq!(
            model,
            Some(("test-deepseek-chat".into(), "deepseek-chat".into()))
        );
        assert_eq!(mode.wire(), "read-only");
        assert_eq!(effort, "high");

        // Unresolvable initial model: §D.7 code, zero side effects.
        let m = request(
            &client,
            "cs-2",
            ClientCall::CreateSession {
                cwd: None,
                project: None,
                initial_model: Some(manox_protocol::ModelRef::new("prov/no-such-model")),
                approval_mode: None,
                reasoning_effort: None,
            },
        );
        match m {
            FromServer::Response {
                outcome: Err(e), ..
            } => assert_eq!(e.data.unwrap()["code"], "model/unresolvable"),
            other => panic!("expected model/unresolvable, got {other:?}"),
        }
        // A follow stream opens on the freshly created session and answers
        // with a snapshot (the intent path is a normal session in every
        // respect).
        let (engine, events) = FakeEngine::new();
        engine.set_journal(0, Vec::new());
        server.set_session_engine_for_test(&sid, engine, events);
        open_follow(&client, "st-cs", &sid);
        expect(&client, |m| {
            matches!(m, FromServer::StreamItem {
                stream_id, frame: manox_protocol::StreamFrame::Snapshot(_),
            } if stream_id.0 == "st-cs")
        });
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    // ── GW2: pump lifetime / double-pump regressions. ─────────────────────

    /// Handshake a fresh connection with an explicit `sessions` ownership
    /// declaration (the `harness` helper always sends `sessions: []`).
    fn connect_sessions(
        server: &AgentServer,
        client_id: &str,
        caps: Vec<HookKind>,
        sessions: Vec<String>,
    ) -> Client {
        let (client_conn, server_conn) = in_process_pair();
        server.accept(Arc::new(server_conn));
        let client = Client { conn: client_conn };
        client.send(FromClient::Request {
            id: MsgId::new(format!("init-{client_id}")),
            call: ClientCall::Initialize(Initialize {
                client_id: client_id.into(),
                capabilities: caps,
                sessions,
                protocol_epoch: PROTOCOL_EPOCH,
            }),
        });
        let resp = client.recv();
        assert!(
            matches!(resp, FromServer::Response { outcome: Ok(_), .. }),
            "expected ack, got {resp:?}"
        );
        let ready = client.recv();
        assert!(
            matches!(
                ready,
                FromServer::Notification {
                    note: ServerNote::Ready
                }
            ),
            "expected Ready, got {ready:?}"
        );
        // GW1/C1 dual emit: drain the Host Ready epoch echo so the queue is
        // clean for the caller's assertions.
        let host_ready = client.recv();
        assert!(
            matches!(
                host_ready,
                FromServer::Host {
                    host: HostEvent::Ready {
                        epoch: PROTOCOL_EPOCH
                    }
                }
            ),
            "expected the Host Ready epoch echo (GW1/C1), got {host_ready:?}"
        );
        client
    }

    /// Poll `server.0.live_pumps()` to `want` within a deadline (pump exit
    /// runs on the runtime; the abort's future-drop is asynchronous).
    fn expect_live_pumps(server: &AgentServer, want: u64, what: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if server.0.live_pumps() == want {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{what}: live pump count is {}, expected {want}",
                server.0.live_pumps()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Drain this connection for a settle window, counting `ServerCall`
    /// Request frames — the "exactly one delivery" pin for the GW2
    /// double-pump chain (one kernel event must produce one frame).
    fn count_requests_in_settle_window(client: &Client, window: Duration) -> usize {
        let settle = std::time::Instant::now() + window;
        let mut count = 0usize;
        loop {
            match client.conn.server_rx().try_recv() {
                Ok(FromServer::Request { .. }) => count += 1,
                Ok(_) => {}
                Err(_) if std::time::Instant::now() < settle => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return count,
            }
        }
    }

    /// GW2 regression: dispose terminates the pump (pre-fix the JoinHandle
    /// drop only DETACHED it — the pump's own ThreadHandle kept the
    /// unbounded subscription open, so `rx.recv()` never closed and the pump
    /// plus engine actor leaked process-wide). A reopen of the same id must
    /// then run EXACTLY ONE pump: one ToolCallAuthorization produces one
    /// Approve frame, and no fail-closed Deny lands before the user replies
    /// (the double-pump chain routed the same auth twice; the duplicate
    /// MsgId registration killed the first waiter and the waterfall read
    /// that as a rejection — an auto-deny before any answer).
    #[test]
    fn dispose_then_reopen_keeps_exactly_one_pump() {
        let _g = lock_globals();
        hermetic_home();
        let sessions = manox_agent::paths::manox_config_dir()
            .expect("config dir")
            .join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        // GW11 hygiene: a seeded file makes every later create() of this id
        // restore — unique id, removed in teardown.
        seed_session_file(&sessions, "gw2-reopen-1", "/proj");
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![HookKind::Approve]);

        // First open: one pump.
        client.send(FromClient::Request {
            id: MsgId::new("open-1"),
            call: ClientCall::OpenSession {
                session_id: "gw2-reopen-1".into(),
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionCreated { session_id } } if session_id == "gw2-reopen-1"),
        );
        expect(
            &client,
            |m| matches!(m, FromServer::Response { id, outcome: Ok(_), .. } if id.0 == "open-1"),
        );
        expect_live_pumps(&server, 1, "after the first open");

        // Dispose: the pump must terminate (pre-fix it ran forever).
        client.send(FromClient::Notification {
            note: ClientNote::DisposeSession {
                session_id: "gw2-reopen-1".into(),
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionDisposed { session_id } } if session_id == "gw2-reopen-1"),
        );
        expect_live_pumps(&server, 0, "after dispose (GW2: the pump must terminate)");

        // Reopen the same id: exactly one pump again — not one leaked plus
        // one fresh.
        client.send(FromClient::Request {
            id: MsgId::new("open-2"),
            call: ClientCall::OpenSession {
                session_id: "gw2-reopen-1".into(),
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionCreated { session_id } } if session_id == "gw2-reopen-1"),
        );
        expect(
            &client,
            |m| matches!(m, FromServer::Response { id, outcome: Ok(_), .. } if id.0 == "open-2"),
        );
        expect_live_pumps(&server, 1, "after the reopen");

        // Behavioral pin: one authorization → one Approve frame, one Reply
        // settles it, and nothing auto-denies before the answer.
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("gw2-reopen-1", engine.clone(), events);
        client.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "gw2-reopen-1".into(),
                text: "probe".into(),
                images: vec![],
                client_id: None,
            },
        });
        expect_host_status(&client, "gw2-reopen-1", |running, _, _, _| {
            running == Some(true)
        });
        engine
            .notices
            .send(BackendNotice::Event(Box::new(
                ThreadEvent::ToolCallAuthorization {
                    id: "gw2-a1".into(),
                    tool_name: "Bash".into(),
                    summary: "run ls".into(),
                    input: json!({}),
                },
            )))
            .unwrap();
        let call_id = loop {
            match client.recv() {
                FromServer::Request {
                    id,
                    call: ServerCall::Approve { auth_id, .. },
                } if auth_id == "gw2-a1" => break id,
                _ => {}
            }
        };
        // A second (leaked) pump would deliver a duplicate Approve frame
        // here — or, with the duplicate-registration refusal, auto-deny the
        // authorization fail-closed. Neither may happen.
        assert_eq!(
            count_requests_in_settle_window(&client, Duration::from_millis(400)),
            0,
            "one ToolCallAuthorization must produce exactly one Approve frame (GW2 double pump)"
        );
        assert!(
            engine.auth_responses.lock().unwrap().is_empty(),
            "no decision may land before the user replies (GW2 auto-deny chain)"
        );
        client.send(FromClient::Reply {
            id: call_id,
            outcome: Ok(json!({"allow": true})),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let responses = engine.auth_responses.lock().unwrap();
            if responses.len() == 1
                && matches!(
                    responses[0].1,
                    manox_agent::permission::ToolAuthorizationResponse::Decision(
                        manox_agent::permission::PermissionDecision::AllowOnce
                    )
                )
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the single Reply never settled the approval as exactly one AllowOnce: {responses:?}"
            );
            drop(responses);
            std::thread::sleep(Duration::from_millis(10));
        }
        engine
            .notices
            .send(BackendNotice::Settled {
                cancelled: false,
                failed: false,
                steered: Vec::new(),
                stranded: Vec::new(),
            })
            .unwrap();
        expect_host_status(&client, "gw2-reopen-1", |running, _, _, _| {
            running == Some(false)
        });
        drop(client);
        drop(server);
        let _ = std::fs::remove_file(sessions.join("gw2-reopen-1.jsonl"));
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW2 regression: two connections racing `OpenSession` for the same
    /// cold id must converge on ONE sessions entry and ONE pump — pre-fix
    /// the check and the insert were separated by unlocked `load_thread` IO
    /// (TOCTOU), both opens passed the check, and the weak upgrade handed
    /// both the SAME `ThreadHandle`, spawning a second pump.
    #[test]
    fn concurrent_open_session_yields_one_entry_one_pump() {
        let _g = lock_globals();
        hermetic_home();
        let sessions = manox_agent::paths::manox_config_dir()
            .expect("config dir")
            .join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        // GW11 hygiene: unique id, seed removed in teardown.
        seed_session_file(&sessions, "gw2-conc-1", "/proj");
        init_globals();
        manox_agent::thread_store::init();
        let server = AgentServer::new(PathBuf::from("/"));
        let client_a = connect_sessions(&server, "racer-a", vec![HookKind::Approve], vec![]);
        let client_b = connect_sessions(&server, "racer-b", vec![HookKind::Approve], vec![]);

        // Fire both opens back-to-back, then read both answers: the
        // check-load-insert window (if any) is where the race lives.
        for (client, id) in [(&client_a, "race-a"), (&client_b, "race-b")] {
            client.send(FromClient::Request {
                id: MsgId::new(id),
                call: ClientCall::OpenSession {
                    session_id: "gw2-conc-1".into(),
                },
            });
        }
        for (client, id) in [(&client_a, "race-a"), (&client_b, "race-b")] {
            expect(
                client,
                |m| matches!(m, FromServer::Response { id: rid, outcome: Ok(_), .. } if rid.0 == id),
            );
        }

        // One table entry, one live pump, both racers owners.
        assert_eq!(
            server.0.sessions.lock().len(),
            1,
            "the sessions table must hold exactly one entry for the raced id"
        );
        expect_live_pumps(&server, 1, "after the concurrent opens");
        let mut owners = server.0.owners("gw2-conc-1");
        owners.sort();
        assert_eq!(owners, vec!["racer-a".to_string(), "racer-b".to_string()]);

        // No double ServerCall: one authorization fans out to exactly one
        // frame per owner (§D.4), never one per pump.
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("gw2-conc-1", engine.clone(), events);
        client_a.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "gw2-conc-1".into(),
                text: "probe".into(),
                images: vec![],
                client_id: None,
            },
        });
        expect_host_status(&client_a, "gw2-conc-1", |running, _, _, _| {
            running == Some(true)
        });
        expect_host_status(&client_b, "gw2-conc-1", |running, _, _, _| {
            running == Some(true)
        });
        engine
            .notices
            .send(BackendNotice::Event(Box::new(
                ThreadEvent::ToolCallAuthorization {
                    id: "gw2c-a1".into(),
                    tool_name: "Bash".into(),
                    summary: "run ls".into(),
                    input: json!({}),
                },
            )))
            .unwrap();
        for client in [&client_a, &client_b] {
            let _call_id = loop {
                match client.recv() {
                    FromServer::Request {
                        id,
                        call: ServerCall::Approve { auth_id, .. },
                    } if auth_id == "gw2c-a1" => break id,
                    _ => {}
                }
            };
        }
        // A second pump would duplicate the per-owner delivery (or, with the
        // duplicate-registration refusal, auto-deny fail-closed).
        assert_eq!(
            count_requests_in_settle_window(&client_a, Duration::from_millis(400)),
            0,
            "owner A saw a duplicate Approve frame (GW2 double pump)"
        );
        assert_eq!(
            count_requests_in_settle_window(&client_b, Duration::from_millis(400)),
            0,
            "owner B saw a duplicate Approve frame (GW2 double pump)"
        );
        assert!(
            engine.auth_responses.lock().unwrap().is_empty(),
            "no decision may land before the owners reply (GW2 auto-deny chain)"
        );
        // Both owners answer next on their OWN connections (the deterministic
        // MsgId is the auth_id on every delivery) → Allowed → exactly one
        // AllowOnce lands on the engine.
        for client in [&client_a, &client_b] {
            client.send(FromClient::Reply {
                id: MsgId::new("gw2c-a1"),
                outcome: Ok(json!({"allow": true})),
            });
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let responses = engine.auth_responses.lock().unwrap();
            if responses.len() == 1
                && matches!(
                    responses[0].1,
                    manox_agent::permission::ToolAuthorizationResponse::Decision(
                        manox_agent::permission::PermissionDecision::AllowOnce
                    )
                )
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the two-owner waterfall never settled as exactly one AllowOnce: {responses:?}"
            );
            drop(responses);
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(client_a);
        drop(client_b);
        drop(server);
        let _ = std::fs::remove_file(sessions.join("gw2-conc-1.jsonl"));
        manox_agent::thread_store::drop_global_for_test();
    }

    // ── GW9: rejected/expired PlanVerdict convergence. ────────────────────

    /// Drain until a `SessionStatus` delta for `session_id` clears
    /// `pending_plan` (§D.5 — `expect_host_status` pins the other fields).
    fn expect_pending_plan_cleared(client: &Client, session_id: &str) {
        expect(client, |m| {
            matches!(
                m,
                FromServer::Host {
                    host: HostEvent::SessionStatus {
                        session_id: sid,
                        pending_plan: Some(false),
                        ..
                    }
                } if sid == session_id
            )
        });
    }

    /// Poll the store-side `pending_plan` flag to `want` (convergence runs
    /// on the pump task).
    fn expect_store_pending_plan(session_id: &str, want: bool, what: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let got =
                manox_agent::thread_store::global().read(|s| s.pending_plan_contains(session_id));
            if got == want {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{what}: store pending_plan is {got}, expected {want}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// GW9 regression: a REJECTED PlanVerdict must converge — pre-fix the
    /// fail-closed arm was a bare `return` that cleared nothing: the kernel
    /// `plan_review_pending` flag and the store `pending_plan` flag stayed
    /// set forever, no `pending_plan=false` delta was broadcast, and the
    /// session was permanently "plan pending review". The convergence
    /// clears every plane, cancels the parked turn, and names the rejecter
    /// in an Error note. Fail-closed semantics hold: the plan never
    /// executes (no `approve_plan` on the engine).
    #[test]
    fn plan_verdict_rejection_converges_pending_state() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![HookKind::PlanVerdict]);
        create(&server, &client, "gw9-s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("gw9-s1", engine.clone(), events);
        client.send(FromClient::Notification {
            note: ClientNote::SetPlanMode {
                session_id: "gw9-s1".into(),
                enabled: true,
            },
        });
        client.settle();
        assert!(plan_mode_of(&server, "gw9-s1"));

        let plan_file =
            std::env::temp_dir().join(format!("manox-gw9-reject-{}.md", std::process::id()));
        std::fs::write(&plan_file, "# Plan\n\n1. Step one\n").unwrap();
        engine
            .notices
            .send(BackendNotice::Event(Box::new(ThreadEvent::PlanReady {
                plan_file: plan_file.to_string_lossy().into_owned(),
                title: "GW9 plan".into(),
            })))
            .unwrap();
        let mut saw_pending_true = false;
        let call_id = loop {
            match client.recv() {
                FromServer::Request {
                    id,
                    call: ServerCall::PlanVerdict { .. },
                } => break id,
                // §D.5: the pending_plan TRUE edge must broadcast on the
                // way in (GW1 delivery finding) — the pump emits it before
                // routing the verdict call, so it arrives first on this
                // FIFO connection.
                FromServer::Host {
                    host:
                        HostEvent::SessionStatus {
                            session_id,
                            pending_plan: Some(true),
                            ..
                        },
                } if session_id == "gw9-s1" => saw_pending_true = true,
                _ => {}
            }
        };
        assert!(
            saw_pending_true,
            "the pending_plan TRUE edge must broadcast (§D.5)"
        );
        // The pending-review planes are set while the verdict is in flight.
        expect_store_pending_plan("gw9-s1", true, "while the verdict is pending");

        // The reviewer rejects.
        client.send(FromClient::Reply {
            id: call_id,
            outcome: Err(RpcError::new(-1, "user rejected the plan")),
        });

        // Convergence, plane by plane:
        // 1. §D.5 delta: pending_plan clears for every connection.
        expect_pending_plan_cleared(&client, "gw9-s1");
        // 2. Store flag clears (sidebar badge / ListThreads snapshot).
        expect_store_pending_plan("gw9-s1", false, "after the rejection");
        // 3. Kernel facade flag clears (no stale review card on restart).
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let flags = engine.plan_review_flags.lock().unwrap().clone();
            if flags == vec![true, false] {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the facade pending-review flag never cleared: {flags:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // 4. The Error note names the rejecter.
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::Error { session_id: Some(sid), message }
                } if sid == "gw9-s1" && message.contains("rejected by test")
            )
        });
        // 5. Fail-closed: the plan never executes and plan mode stays on
        //    (the user can re-edit; convergence touches no plan_mode).
        assert!(
            engine.plan_approvals.lock().unwrap().is_empty(),
            "a rejected plan must not seed execution"
        );
        assert!(
            plan_mode_of(&server, "gw9-s1"),
            "convergence leaves plan mode on (fail-closed, re-editable)"
        );
        // 6. The session stays operable: a §D.2 read answers normally.
        let v = response_outcome(request(
            &client,
            "gw9-ph",
            ClientCall::PageHistory {
                session_id: "gw9-s1".into(),
                through_seq: -1,
                before_seq: None,
                max_messages: None,
            },
        ));
        assert_eq!(v["cursor"], 0, "PageHistory answers after convergence");

        let _ = std::fs::remove_file(&plan_file);
        let _ = std::fs::remove_file(
            manox_agent::paths::sessions_dir()
                .unwrap()
                .join("gw9-s1.jsonl"),
        );
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW9 regression: an UNREVIEWABLE plan (no owner declared the
    /// PlanVerdict capability) is the fail_closed arm of the same deadlock —
    /// pre-fix it noted an Error and left every pending-review plane set.
    #[test]
    fn plan_verdict_without_reviewer_converges() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        // No PlanVerdict capability: route_call finds no target.
        let (server, client) = harness(vec![]);
        create(&server, &client, "gw9-s2");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("gw9-s2", engine.clone(), events);
        client.send(FromClient::Notification {
            note: ClientNote::SetPlanMode {
                session_id: "gw9-s2".into(),
                enabled: true,
            },
        });
        client.settle();
        engine
            .notices
            .send(BackendNotice::Event(Box::new(ThreadEvent::PlanReady {
                plan_file: "/nonexistent/gw9-plan.md".into(),
                title: "GW9 orphan plan".into(),
            })))
            .unwrap();
        // Convergence order inside `converge_plan_rejected` is wire-ordered:
        // the §D.5 broadcast is sent BEFORE the fail-closed Error note, so
        // expect the Host frame first (an expect drains and discards
        // non-matching frames).
        expect_pending_plan_cleared(&client, "gw9-s2");
        // The fail-closed Error note arrives...
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::Error { session_id: Some(sid), message }
                } if sid == "gw9-s2" && message.contains("no client can review this plan")
            )
        });
        // ...and the pending-review planes converge exactly like a
        // rejection: store flag and facade flag clear.
        expect_store_pending_plan("gw9-s2", false, "after the unreviewable plan");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let flags = engine.plan_review_flags.lock().unwrap().clone();
            if flags == vec![true, false] {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the facade pending-review flag never cleared: {flags:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // Still operable.
        let v = response_outcome(request(
            &client,
            "gw9-ph2",
            ClientCall::PageHistory {
                session_id: "gw9-s2".into(),
                through_seq: -1,
                before_seq: None,
                max_messages: None,
            },
        ));
        assert_eq!(v["cursor"], 0);
        let _ = std::fs::remove_file(
            manox_agent::paths::sessions_dir()
                .unwrap()
                .join("gw9-s2.jsonl"),
        );
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW9 timeout path: `CALL_TIMEOUT` is 300s — unreachable inside a unit
    /// test — so the convergence itself is pinned by direct call with the
    /// expired-delivery message route_waterfall builds for a timed-out
    /// reviewer.
    #[test]
    fn converge_plan_rejected_clears_every_plane_directly() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![HookKind::PlanVerdict]);
        create(&server, &client, "gw9-s3");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("gw9-s3", engine.clone(), events);

        // Set both pending planes as PlanReady would.
        manox_agent::thread_store::global().with_mut(|s| s.mark_pending_plan("gw9-s3", true));
        server
            .0
            .session_thread("gw9-s3")
            .expect("live session")
            .with_mut(|t| t.set_plan_review_pending(true));

        // The expiry convergence, exactly as the timed-out waterfall arm
        // calls it.
        converge_plan_rejected(
            &server.0,
            "gw9-s3",
            "plan verdict expired: no reply from test".to_string(),
        );

        assert_eq!(
            engine.plan_review_flags.lock().unwrap().clone(),
            vec![true, false]
        );
        expect_store_pending_plan("gw9-s3", false, "after the direct convergence");
        expect_pending_plan_cleared(&client, "gw9-s3");
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::Error { session_id: Some(sid), message }
                } if sid == "gw9-s3" && message.contains("expired")
            )
        });
        let _ = std::fs::remove_file(
            manox_agent::paths::sessions_dir()
                .unwrap()
                .join("gw9-s3.jsonl"),
        );
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    // ── GW10: handshake owner registration. ───────────────────────────────

    /// GW10 regression: a re-seat handshake (same client_id reconnecting)
    /// that re-declares a non-empty `sessions` list must not duplicate the
    /// owner rows — pre-fix the handshake pushed unconditionally (no
    /// `add_owner` dedup) and `remove_client`'s generation guard early-
    /// returned without clearing the old rows, so every reconnect copied
    /// the ownership: `owner_conns` fanned every note out twice and
    /// `route_call` registered the same MsgId twice (the GW2 auto-deny
    /// chain). The fix clears the client's rows on handshake and re-adds
    /// through the deduping `add_owner`: the fresh hello is authoritative.
    #[test]
    fn rehandshake_same_client_id_keeps_one_owner_row_per_session() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let server = AgentServer::new(PathBuf::from("/"));

        let _first = connect_sessions(&server, "gw10", vec![], vec!["gw10-s1".into()]);
        assert_eq!(server.0.owners("gw10-s1"), vec!["gw10".to_string()]);

        // Re-seat: the same client_id on a fresh connection, re-declaring
        // the session. The old connection is disconnected by the handshake.
        let second = connect_sessions(&server, "gw10", vec![], vec!["gw10-s1".into()]);
        // The handshake completed synchronously before Ready — no polling.
        assert_eq!(
            server.0.owners("gw10-s1"),
            vec!["gw10".to_string()],
            "a re-seat handshake must keep exactly one owner row (GW10)"
        );

        // One owner row ⇒ one frame per note (pre-fix: the duplicated row
        // delivered every owner-scoped note twice to the same connection).
        server.0.route_note(
            "gw10-s1",
            ServerNote::Error {
                session_id: Some("gw10-s1".into()),
                message: "gw10-probe".into(),
            },
        );
        expect(&second, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::Error { message, .. }
                } if message == "gw10-probe"
            )
        });
        // Settle window: a duplicated owner row would deliver the SAME probe
        // note to this connection a second time.
        let settle = std::time::Instant::now() + Duration::from_millis(300);
        let mut duplicates = 0usize;
        loop {
            match second.conn.server_rx().try_recv() {
                Ok(FromServer::Notification {
                    note: ServerNote::Error { message, .. },
                }) if message == "gw10-probe" => duplicates += 1,
                Ok(_) => {}
                Err(_) if std::time::Instant::now() < settle => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        assert_eq!(
            duplicates, 0,
            "duplicate owner row delivered the probe note twice (GW10)"
        );
        drop(_first);
        drop(second);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    // ── GW7: terminal stubs answer explicitly. ────────────────────────────

    /// GW7 regression: the terminal calls answer with the stable
    /// `feature/unavailable` code (pre-fix a bare -1 with no data.code —
    /// indistinguishable from a generic failure for clients that declared
    /// terminal support). The code is a §D.7 addition candidate; the spec
    /// revision is proposed in the delivery report.
    #[test]
    fn terminal_calls_answer_feature_unavailable() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (_server, client) = harness(vec![]);
        let m = request(
            &client,
            "term-attach",
            ClientCall::TerminalAttach {
                session: "s1".into(),
                cols: 80,
                rows: 24,
            },
        );
        match m {
            FromServer::Response {
                outcome: Err(e), ..
            } => {
                assert_eq!(
                    e.data.as_ref().expect("GW7: the error carries data.code")["code"],
                    "feature/unavailable"
                );
                assert!(e.message.contains("β-3b"));
            }
            other => panic!("expected the terminal error, got {other:?}"),
        }
        let m = request(
            &client,
            "term-snapshot",
            ClientCall::TerminalSnapshot {
                terminal: "t1".into(),
            },
        );
        match m {
            FromServer::Response {
                outcome: Err(e), ..
            } => {
                assert_eq!(e.data.unwrap()["code"], "feature/unavailable");
            }
            other => panic!("expected the terminal error, got {other:?}"),
        }
        drop(client);
        drop(_server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW7 regression: terminal NOTES are no longer silently swallowed —
    /// the sending client gets an explicit `ServerNote::Error` (silent data
    /// loss for a client that declared terminal support).
    #[test]
    fn terminal_notes_answer_with_an_error_note() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (_server, client) = harness(vec![]);
        for note in [
            ClientNote::TerminalInput {
                terminal: "t1".into(),
                bytes: b"ls\n".to_vec(),
            },
            ClientNote::TerminalResize {
                terminal: "t1".into(),
                cols: 100,
                rows: 40,
            },
        ] {
            client.send(FromClient::Notification { note });
            expect(&client, |m| {
                matches!(
                    m,
                    FromServer::Notification {
                        note: ServerNote::Error { session_id: None, message }
                    } if message == "terminal input dropped: terminal support lands in β-3b"
                )
            });
        }
        drop(client);
        drop(_server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// K5 gateway regression: a direct Submit persists BEFORE its receipt
    /// (accepted ⟹ logged). By the time the receipt arrives the persist
    /// call is recorded with the SAME text/images/origin the run carries,
    /// and the run itself carries the accepted entry id — the pin that
    /// arms the middleware's one-shot duplicate skip.
    #[test]
    fn submit_receipt_waits_for_accept_time_persistence() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        engine.set_persist_reply(Some(Ok(Some("entry-42".to_string()))));
        server.set_session_engine_for_test("s1", engine.clone(), events);
        client.send(FromClient::Request {
            id: MsgId::new("sub-1"),
            call: ClientCall::Submit {
                session_id: "s1".into(),
                text: "persist me".into(),
                images: vec![ImageAttachment {
                    data: vec![1, 2, 3],
                    mime_type: "image/png".into(),
                }],
                origin_rpc: Some("rpc-9".into()),
            },
        });
        let outcome = loop {
            if let FromServer::Response { id, outcome } = client.recv() {
                assert_eq!(id.0, "sub-1");
                break outcome;
            }
        };
        assert!(
            outcome.is_ok(),
            "the receipt answers once persistence landed"
        );
        assert_eq!(
            engine.persist_calls.lock().unwrap().clone(),
            vec![("persist me".to_string(), 1, Some("rpc-9".to_string()))],
            "persist precedes the receipt with the run's own (text, images, origin)"
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let runs = engine.origin_runs.lock().unwrap().clone();
            if !runs.is_empty() {
                assert_eq!(runs[0].0, "persist me");
                assert_eq!(runs[0].1, Some("rpc-9".to_string()));
                assert_eq!(
                    runs[0].2,
                    Some("entry-42".to_string()),
                    "the run carries the accepted entry (middleware-skip pin)"
                );
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the run never started"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// K5 gateway regression: when persistence FAILS the Submit must not
    /// receipt as accepted (accepted ⟹ logged) — the caller gets a coded
    /// error and no turn starts.
    #[test]
    fn submit_persistence_failure_refuses_the_receipt() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        let (engine, events) = FakeEngine::new();
        engine.set_persist_reply(Some(Err(())));
        server.set_session_engine_for_test("s1", engine.clone(), events);
        client.send(FromClient::Request {
            id: MsgId::new("sub-2"),
            call: ClientCall::Submit {
                session_id: "s1".into(),
                text: "doomed".into(),
                images: vec![],
                origin_rpc: Some("rpc-x".into()),
            },
        });
        let outcome = loop {
            if let FromServer::Response { id, outcome } = client.recv() {
                assert_eq!(id.0, "sub-2");
                break outcome;
            }
        };
        let err = outcome.expect_err("a failed persist must refuse the receipt");
        assert_eq!(
            err.data.unwrap()["code"],
            manox_protocol::msg::CODE_GATEWAY_INTERNAL
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            engine.runs.lock().unwrap().is_empty(),
            "a refused submit must not start a run"
        );
        assert!(engine.origin_runs.lock().unwrap().is_empty());
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    // ── K.7 remediation wave: C1 / GW1 / GW3 / GW5 / GW6 regressions. ─────

    /// Seed a dense two-entry v4 chain (two user messages) under the hermetic
    /// sessions dir — the on-disk shape the GW6 cold reads answer from.
    /// Returns the file path (callers remove it in teardown, GW11 hygiene).
    fn seed_v4_chain(dir: &std::path::Path, id: &str) -> PathBuf {
        let cwd = dir.parent().unwrap().to_string_lossy().into_owned();
        let path = dir.join(format!("{id}.jsonl"));
        std::fs::write(
            &path,
            format!(
                r#"{{"type":"session","version":4,"id":"{id}","timestamp":"2026-05-28T07:13:46.608Z","cwd":"{cwd}"}}
{{"type":"message","id":"m1","parentId":null,"seq":0,"timestamp":"2026-05-28T07:14:00.000Z","message":{{"role":"user","content":[{{"type":"text","text":"one"}}],"timestamp":1779952440000}}}}
{{"type":"message","id":"m2","parentId":"m1","seq":1,"timestamp":"2026-05-28T07:14:10.000Z","message":{{"role":"user","content":[{{"type":"text","text":"two"}}],"timestamp":1779952450000}}}}
"#
            ),
        )
        .unwrap();
        path
    }

    /// C1 regression: a handshake declaring a protocol epoch the server does
    /// not speak must be refused with the stable `protocol/unsupported-epoch`
    /// code (§D.7) — pre-fix the server accepted ANY `Initialize` (the epoch
    /// field did not exist), so future frame generations could not be
    /// distinguished (L12). The frame rides raw JSON: a pre-epoch server
    /// parses it by ignoring the unknown field, which is exactly the defect.
    #[test]
    fn handshake_rejects_unknown_protocol_epoch() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let server = AgentServer::new(PathBuf::from("/"));
        let (client_conn, server_conn) = serde_pair();
        server.accept(Arc::new(server_conn));
        let client = SerdeClient { conn: client_conn };
        let init: FromClient = serde_json::from_value(json!({
            "kind": "request",
            "id": "init-epoch",
            "call": {
                "method": "initialize",
                "clientId": "epoch-probe",
                "capabilities": [],
                "sessions": [],
                "protocolEpoch": 7,
            }
        }))
        .expect("the epoch-bearing Initialize frame parses");
        client.send(init);
        match client.recv_timeout(Duration::from_secs(10)) {
            FromServer::Response {
                id,
                outcome: Err(e),
            } if id.0 == "init-epoch" => {
                assert_eq!(
                    e.data
                        .as_ref()
                        .expect("C1: the epoch rejection carries data.code")["code"],
                    "protocol/unsupported-epoch"
                );
                assert!(
                    e.message.contains('7'),
                    "the rejection names the offending epoch: {}",
                    e.message
                );
            }
            other => panic!("expected a coded epoch rejection, got {other:?}"),
        }
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// C1 compat pin: a v1 client whose `Initialize` carries no
    /// `protocolEpoch` at all (the serde-default-0 generation) is still
    /// accepted — the epoch gate must not sever the dual-protocol window.
    #[test]
    fn handshake_accepts_v1_client_without_protocol_epoch() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let server = AgentServer::new(PathBuf::from("/"));
        let (client_conn, server_conn) = serde_pair();
        server.accept(Arc::new(server_conn));
        let client = SerdeClient { conn: client_conn };
        let init: FromClient = serde_json::from_value(json!({
            "kind": "request",
            "id": "init-v1",
            "call": {
                "method": "initialize",
                "clientId": "v1-probe",
                "capabilities": [],
                "sessions": [],
            }
        }))
        .expect("the v1 Initialize frame parses");
        client.send(init);
        match client.recv_timeout(Duration::from_secs(10)) {
            FromServer::Response { id, outcome: Ok(_) } if id.0 == "init-v1" => {}
            other => panic!("a v1 handshake (no protocolEpoch) must be accepted, got {other:?}"),
        }
        assert!(
            matches!(
                client.recv_timeout(Duration::from_secs(10)),
                FromServer::Notification {
                    note: ServerNote::Ready
                }
            ),
            "the v1 handshake still yields the Ready note"
        );
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// C1+GW1 regression: the handshake dual-emits — the v1
    /// `ServerNote::Ready` is followed by the §D.5 `HostEvent::Ready` echo
    /// carrying the server's protocol epoch. Pre-fix no Host frame ever
    /// arrived (the third `recv` below timed out).
    #[test]
    fn handshake_ready_double_emits_host_epoch_echo() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let server = AgentServer::new(PathBuf::from("/"));
        let (client_conn, server_conn) = in_process_pair();
        server.accept(Arc::new(server_conn));
        let client = Client { conn: client_conn };
        client.send(FromClient::Request {
            id: MsgId::new("init-echo"),
            call: ClientCall::Initialize(Initialize {
                client_id: "echo-probe".into(),
                capabilities: vec![],
                sessions: vec![],
                protocol_epoch: PROTOCOL_EPOCH,
            }),
        });
        assert!(matches!(client.recv(), FromServer::Response { .. }));
        assert!(matches!(
            client.recv(),
            FromServer::Notification {
                note: ServerNote::Ready
            }
        ));
        // The §D.5 Host mirror with the accepted epoch (C1: `Ready{epoch}`
        // echoes PROTOCOL_EPOCH = 1; pre-fix this recv timed out).
        match client.recv() {
            FromServer::Host {
                host: HostEvent::Ready { epoch },
            } => assert_eq!(epoch, 1, "the Ready host event echoes the epoch"),
            other => panic!("expected the Host Ready epoch echo (C1/GW1), got {other:?}"),
        }
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW1 regression: the list-push channel dual-emits — each of
    /// Models/ThreadsUpdated/Commands arrives BOTH as the v1 note and as its
    /// §D.5 `HostEvent` mirror on the requesting connection (the C4
    /// close-out retires the note arm; until then clients fold both).
    /// Pre-fix no Host mirror was ever produced (`saw_host` stayed false).
    #[test]
    fn list_pushes_double_emit_host_frames() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);

        client.send(FromClient::Request {
            id: MsgId::new("lm"),
            call: ClientCall::ListModels,
        });
        let (mut saw_note, mut saw_host) = (false, false);
        loop {
            match client.recv() {
                FromServer::Response { id, .. } if id.0 == "lm" => break,
                FromServer::Notification {
                    note: ServerNote::Models { .. },
                } => saw_note = true,
                FromServer::Host {
                    host: HostEvent::Models { .. },
                } => saw_host = true,
                _ => {}
            }
        }
        assert!(saw_note, "the v1 Models note still emits (dual window)");
        assert!(saw_host, "GW1: ListModels must dual-emit HostEvent::Models");

        client.send(FromClient::Request {
            id: MsgId::new("lt"),
            call: ClientCall::ListThreads,
        });
        let (mut saw_note, mut saw_host) = (false, false);
        loop {
            match client.recv() {
                FromServer::Response { id, .. } if id.0 == "lt" => break,
                FromServer::Notification {
                    note: ServerNote::ThreadsUpdated { .. },
                } => saw_note = true,
                FromServer::Host {
                    host: HostEvent::ThreadsUpdated { .. },
                } => saw_host = true,
                _ => {}
            }
        }
        assert!(saw_note, "the v1 ThreadsUpdated note still emits");
        assert!(
            saw_host,
            "GW1: ListThreads must dual-emit HostEvent::ThreadsUpdated"
        );

        client.send(FromClient::Request {
            id: MsgId::new("lc"),
            call: ClientCall::ListCommands,
        });
        let (mut saw_note, mut saw_host) = (false, false);
        loop {
            match client.recv() {
                FromServer::Response { id, .. } if id.0 == "lc" => break,
                FromServer::Notification {
                    note: ServerNote::Commands { .. },
                } => saw_note = true,
                FromServer::Host {
                    host: HostEvent::Commands { .. },
                } => saw_host = true,
                _ => {}
            }
        }
        assert!(saw_note, "the v1 Commands note still emits");
        assert!(
            saw_host,
            "GW1: ListCommands must dual-emit HostEvent::Commands"
        );
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW1 regression: `SessionCreated` dual-emits DIRECTED — the Host mirror
    /// reaches the owner connection (with the session header, §D.5) and never
    /// a non-owner (it is owner-set control, not a broadcast; pre-fix the
    /// owner's `expect` below timed out).
    #[test]
    fn session_created_double_emits_directed_host_frame() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let server = AgentServer::new(PathBuf::from("/"));
        let owner = connect_sessions(&server, "gw1-owner", vec![], vec![]);
        let bystander = connect_sessions(&server, "gw1-bystander", vec![], vec![]);

        create(&server, &owner, "gw1-s1");
        // create() drained the v1 note; the Host mirror follows it on the
        // same connection, carrying the header (§D.5 SessionCreated).
        expect(&owner, |m| {
            matches!(
                m,
                FromServer::Host {
                    host: HostEvent::SessionCreated { session_id, header }
                } if session_id == "gw1-s1" && header.id == "gw1-s1"
            )
        });
        // Directed, not broadcast: the bystander's settle window stays free
        // of the owner's SessionCreated mirror.
        let settle = std::time::Instant::now() + Duration::from_millis(300);
        while std::time::Instant::now() < settle {
            if let Ok(m) = bystander.conn.server_rx().try_recv() {
                assert!(
                    !matches!(
                        m,
                        FromServer::Host {
                            host: HostEvent::SessionCreated { .. }
                        }
                    ),
                    "GW1: SessionCreated is owner-directed, never broadcast: {m:?}"
                );
            } else {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        drop(owner);
        drop(bystander);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW1 regression: `SessionDisposed` dual-emits on both directed paths —
    /// dispose (requester only, §D.5) and detach (the detaching client).
    /// Pre-fix the Host mirror `expect`s timed out.
    #[test]
    fn session_disposed_and_detached_double_emit_host_frames() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);

        create(&server, &client, "gw1-d1");
        client.send(FromClient::Notification {
            note: ClientNote::DisposeSession {
                session_id: "gw1-d1".into(),
            },
        });
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::SessionDisposed { session_id }
                } if session_id == "gw1-d1"
            )
        });
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Host {
                    host: HostEvent::SessionDisposed { session_id }
                } if session_id == "gw1-d1"
            )
        });

        create(&server, &client, "gw1-d2");
        client.send(FromClient::Notification {
            note: ClientNote::DetachSession {
                session_id: "gw1-d2".into(),
            },
        });
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::SessionDisposed { session_id }
                } if session_id == "gw1-d2"
            )
        });
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Host {
                    host: HostEvent::SessionDisposed { session_id }
                } if session_id == "gw1-d2"
            )
        });
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW1 regression: a session-scoped `Error` note dual-emits its §D.5
    /// `HostEvent::Error` mirror to the same owner audience, same message
    /// (pre-fix the Host mirror `expect` timed out).
    #[test]
    fn error_notes_double_emit_host_error() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);
        create(&server, &client, "gw1-e1");
        // An unresolvable SetModel target routes `note_error` to the owners.
        client.send(FromClient::Notification {
            note: ClientNote::SetModel {
                session_id: "gw1-e1".into(),
                id: "definitely-not-a-model".into(),
            },
        });
        let mut note_message: Option<String> = None;
        let mut host_message: Option<String> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while note_message.is_none() || host_message.is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "GW1: the Error dual emit never completed (note={note_message:?} host={host_message:?})"
            );
            match client.recv() {
                FromServer::Notification {
                    note:
                        ServerNote::Error {
                            session_id: Some(sid),
                            message,
                        },
                } if sid == "gw1-e1" => note_message = Some(message),
                FromServer::Host {
                    host: HostEvent::Error { message },
                } => host_message = Some(message),
                _ => {}
            }
        }
        assert_eq!(
            note_message, host_message,
            "GW1: the Host Error mirror carries the note's message"
        );
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW5 regression: unread is client-owned — the settle edge ALWAYS raises
    /// `unread:true`, even when a client just reported focus on the session.
    /// Pre-fix the server's single-slot `focused` mirror suppressed the delta
    /// (and the desktop, which never sends FocusThread, lit unread for the
    /// session the user was watching; multi-client focus was inexpressible).
    #[test]
    fn turn_settle_raises_unread_even_after_focus_report() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);
        create(&server, &client, "gw5-s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("gw5-s1", engine.clone(), events);
        // The client reports focus; GW5 makes the server handler a no-op.
        client.send(FromClient::Notification {
            note: ClientNote::FocusThread {
                session_id: Some("gw5-s1".into()),
            },
        });
        client.settle();
        client.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "gw5-s1".into(),
                text: "hi".into(),
                images: vec![],
                client_id: None,
            },
        });
        expect_host_status(&client, "gw5-s1", |running, _, _, _| running == Some(true));
        engine
            .notices
            .send(BackendNotice::Settled {
                cancelled: false,
                failed: false,
                steered: Vec::new(),
                stranded: Vec::new(),
            })
            .unwrap();
        // The settle edge carries unread:true REGARDLESS of the focus report
        // (pre-fix: the focused mirror suppressed this delta and the expect
        // timed out).
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Host {
                    host: HostEvent::SessionStatus {
                        session_id,
                        running: Some(false),
                        unread: Some(true),
                        ..
                    }
                } if session_id == "gw5-s1"
            )
        });
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW5 regression: the server keeps NO unread mirror — `FocusThread` is a
    /// no-op (a legacy store-side mirror write survives it; pre-fix the
    /// handler cleared it) and the §D.2 list response reports `unread:false`
    /// regardless of the store (field deprecated, C4 removes it; pre-fix the
    /// row carried the mirror value `true`).
    #[test]
    fn list_threads_unread_is_always_false_and_focus_is_noop() {
        let _g = lock_globals();
        hermetic_home();
        let sessions = manox_agent::paths::manox_config_dir()
            .expect("config dir")
            .join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        // A cold thread the init scan indexes (the list row's source).
        seed_session_file(&sessions, "gw5-s2", "/proj");
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        // Wait for the asynchronous scan to land the summary row.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if manox_agent::thread_store::global()
                .read(|s| s.summaries().iter().any(|t| t.id == "gw5-s2"))
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the init scan never indexed the seeded thread"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // Simulate a legacy mirror write (the desktop's own store writes
        // during the transition window).
        manox_agent::thread_store::global().with_mut(|s| s.set_unread("gw5-s2", true));

        // FocusThread is a server-side no-op now (GW5): the mirror survives.
        client.send(FromClient::Notification {
            note: ClientNote::FocusThread {
                session_id: Some("gw5-s2".into()),
            },
        });
        client.settle();
        assert!(
            manox_agent::thread_store::global().read(|s| s
                .summaries()
                .iter()
                .any(|t| t.id == "gw5-s2" && t.has_unread)),
            "GW5: FocusThread must no longer clear a store-side unread mirror (handler is a no-op)"
        );

        // The list response never carries the mirror (deprecated field,
        // always false until C4 removes it).
        let v = response_outcome(request(&client, "gw5-lt", ClientCall::ListThreads));
        let row = v
            .as_array()
            .expect("ListThreads answers an array")
            .iter()
            .find(|r| r["id"] == "gw5-s2")
            .expect("the seeded thread has a list row");
        assert_eq!(
            row["unread"],
            json!(false),
            "GW5: list responses carry no server unread mirror (deprecated, C4 removes the field)"
        );
        drop(client);
        drop(server);
        let _ = std::fs::remove_file(sessions.join("gw5-s2.jsonl"));
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW6 regression: PageHistory on an OPENED session whose engine seam
    /// answers "not materialized" must cold-read the persisted jsonl (§D.2
    /// "冷读不激活 engine，jsonl 直读") — pre-fix it answered
    /// `gateway/internal: journal engine is not materialized`.
    #[test]
    fn page_history_cold_reads_disk_for_opened_session_without_engine() {
        let _g = lock_globals();
        hermetic_home();
        let sessions = manox_agent::paths::manox_config_dir()
            .expect("config dir")
            .join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = seed_v4_chain(&sessions, "gw6-cold-1");
        init_globals();
        manox_agent::thread_store::init();
        // Deterministic identity seed (the async init scan may not have
        // landed yet): the open must find the path map entry.
        manox_agent::thread_store::global().with_mut(|s| s.note_session_path("gw6-cold-1", &path));
        let (server, client) = harness(vec![]);
        // Open the cold session (restores from disk).
        client.send(FromClient::Request {
            id: MsgId::new("gw6-open"),
            call: ClientCall::OpenSession {
                session_id: "gw6-cold-1".into(),
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Response { id, outcome: Ok(_), .. } if id.0 == "gw6-open"),
        );
        // Replace the engine with the unavailable-seam fake: the live read
        // answers None, so the page must come from the disk journal.
        let (engine, events) = FakeEngine::new();
        engine.set_journal_unavailable();
        server.set_session_engine_for_test("gw6-cold-1", engine, events);

        let v = response_outcome(request(
            &client,
            "gw6-ph",
            ClientCall::PageHistory {
                session_id: "gw6-cold-1".into(),
                through_seq: -1,
                before_seq: None,
                max_messages: None,
            },
        ));
        assert_eq!(v["cursor"], 1, "the cold chain's tail seq is the cursor");
        let records = v["records"].as_array().expect("records array");
        assert_eq!(records.len(), 2, "both persisted entries cold-read");
        assert_eq!(records[0]["seq"], 0);
        assert_eq!(records[0]["type"], "message");
        assert_eq!(records[1]["seq"], 1);
        assert_eq!(v["has_more"], false);
        drop(client);
        drop(server);
        let _ = std::fs::remove_file(&path);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW6 regression: PageHistory answers for a session NEVER opened on this
    /// server — the cold path reads the persisted journal without activating
    /// anything (§D.2). Pre-fix: `session/not-found` for any id outside the
    /// live sessions table.
    #[test]
    fn page_history_reads_disk_without_live_session() {
        let _g = lock_globals();
        hermetic_home();
        let sessions = manox_agent::paths::manox_config_dir()
            .expect("config dir")
            .join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = seed_v4_chain(&sessions, "gw6-cold-2");
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        // No open/create: the session exists only on disk.
        let v = response_outcome(request(
            &client,
            "gw6-ph2",
            ClientCall::PageHistory {
                session_id: "gw6-cold-2".into(),
                through_seq: -1,
                before_seq: None,
                max_messages: Some(1),
            },
        ));
        let records = v["records"].as_array().expect("records array");
        assert_eq!(records.len(), 1, "max_messages caps the cold page");
        assert_eq!(records[0]["seq"], 1, "the page keeps the tail entry");
        assert_eq!(v["cursor"], 1);
        assert_eq!(v["has_more"], true, "seq 0 predates the window");
        drop(client);
        drop(server);
        let _ = std::fs::remove_file(&path);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW6 pin: a truly unknown id (no live session, no persisted file) still
    /// answers `session/not-found` — the cold read must not mask discovery
    /// errors with an empty page.
    #[test]
    fn page_history_unknown_session_still_answers_not_found() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let (server, client) = harness(vec![]);
        match request(
            &client,
            "gw6-ph3",
            ClientCall::PageHistory {
                session_id: "gw6-does-not-exist".into(),
                through_seq: -1,
                before_seq: None,
                max_messages: None,
            },
        ) {
            FromServer::Response {
                outcome: Err(e), ..
            } => {
                assert_eq!(
                    e.data.as_ref().expect("coded error")["code"],
                    manox_protocol::msg::CODE_SESSION_NOT_FOUND
                );
            }
            other => panic!("expected session/not-found, got {other:?}"),
        }
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW6 resume singleflight: concurrent compat CreateSession notes and an
    /// OpenSession request racing the SAME cold id converge on exactly one
    /// sessions entry and one pump — the argument: the cold-file probe
    /// delegates to `open_session`, whose check-load-insert is one `sessions`
    /// lock hold (GW2), so every racer serializes through it; the first
    /// loads and inserts, the rest take the idempotent re-own branch. (The
    /// fresh-mint path is covered by `insert_session`'s replace-and-stop
    /// semantics.) This test pins the argument.
    #[test]
    fn concurrent_create_and_open_same_cold_id_singleflight() {
        let _g = lock_globals();
        hermetic_home();
        let sessions = manox_agent::paths::manox_config_dir()
            .expect("config dir")
            .join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        // GW11 hygiene: unique id, seed removed in teardown.
        seed_session_file(&sessions, "gw6-race", "/proj");
        init_globals();
        manox_agent::thread_store::init();
        // Deterministic identity seed: every racer's load must find the path
        // map entry regardless of the async scan's timing.
        manox_agent::thread_store::global()
            .with_mut(|s| s.note_session_path("gw6-race", &sessions.join("gw6-race.jsonl")));
        let server = AgentServer::new(PathBuf::from("/"));
        let a = connect_sessions(&server, "sf-a", vec![], vec![]);
        let b = connect_sessions(&server, "sf-b", vec![], vec![]);
        let c = connect_sessions(&server, "sf-c", vec![], vec![]);

        // Fire all three materializations back-to-back: two compat creates
        // (fire-and-forget) and one open request.
        a.send(FromClient::Notification {
            note: ClientNote::CreateSession {
                session_id: "gw6-race".into(),
                cwd: Some("/proj".into()),
            },
        });
        b.send(FromClient::Notification {
            note: ClientNote::CreateSession {
                session_id: "gw6-race".into(),
                cwd: Some("/proj".into()),
            },
        });
        c.send(FromClient::Request {
            id: MsgId::new("sf-open"),
            call: ClientCall::OpenSession {
                session_id: "gw6-race".into(),
            },
        });
        expect(
            &c,
            |m| matches!(m, FromServer::Response { id, outcome: Ok(_), .. } if id.0 == "sf-open"),
        );

        // Every racer converges on the SAME entry: one table row, one pump,
        // all three owners.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let mut owners = server.0.owners("gw6-race");
            owners.sort();
            if owners == vec!["sf-a".to_string(), "sf-b".to_string(), "sf-c".to_string()] {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the racers never converged on one owner set: {owners:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            server.0.sessions.lock().len(),
            1,
            "GW6 singleflight: exactly one sessions entry for the raced id"
        );
        expect_live_pumps(&server, 1, "GW6 singleflight: exactly one pump");
        drop(a);
        drop(b);
        drop(c);
        drop(server);
        let _ = std::fs::remove_file(sessions.join("gw6-race.jsonl"));
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW3 regression: every adjudication delivery carries a stable
    /// `deliveryId` on the wire (§D.4) — the handle a `cancelDelivery` call
    /// references. Inspected through the wire JSON so the pin compiles
    /// against the pre-GW3 enum (which lacked the field — the assertion
    /// below is the red evidence). Also pins uniqueness across deliveries of
    /// one session.
    #[test]
    fn adjudication_requests_carry_stable_delivery_id() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![
            HookKind::Approve,
            HookKind::AskUserQuestion,
            HookKind::PlanVerdict,
        ]);
        create(&server, &client, "gw3-s1");
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("gw3-s1", engine.clone(), events);
        client.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "gw3-s1".into(),
                text: "do work".into(),
                images: vec![],
                client_id: None,
            },
        });
        expect_host_status(&client, "gw3-s1", |running, _, _, _| running == Some(true));

        // One ToolCallAuthorization → Approve with a non-empty deliveryId.
        engine
            .notices
            .send(BackendNotice::Event(Box::new(
                ThreadEvent::ToolCallAuthorization {
                    id: "g3-a1".into(),
                    tool_name: "Bash".into(),
                    summary: "run ls".into(),
                    input: json!({}),
                },
            )))
            .unwrap();
        let (approve_id, approve_dlv) = loop {
            match client.recv() {
                FromServer::Request { id, call } if matches!(&call, ServerCall::Approve { auth_id, .. } if auth_id == "g3-a1") =>
                {
                    let wire = serde_json::to_value(&call).unwrap();
                    break (id, wire["deliveryId"].as_str().unwrap_or("").to_string());
                }
                _ => {}
            }
        };
        assert!(
            !approve_dlv.is_empty(),
            "GW3: the Approve delivery must carry a stable deliveryId"
        );
        assert!(
            approve_dlv.contains("gw3-s1"),
            "GW3: the deliveryId names its session ({approve_dlv})"
        );
        client.send(FromClient::Reply {
            id: approve_id,
            outcome: Ok(json!({"allow": true})),
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if engine
                .auth_responses
                .lock()
                .unwrap()
                .iter()
                .any(|(id, _)| id == "g3-a1")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the Approve reply never settled"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // A second adjudication (AskUser) gets a DIFFERENT deliveryId.
        engine
            .notices
            .send(BackendNotice::Event(Box::new(
                ThreadEvent::ToolCallAuthorization {
                    id: "g3-q1".into(),
                    tool_name: manox_agent::tools::ASK_USER_QUESTION.to_string(),
                    summary: "pick".into(),
                    input: json!({}),
                },
            )))
            .unwrap();
        let (ask_id, ask_dlv) = loop {
            match client.recv() {
                FromServer::Request { id, call } if matches!(&call, ServerCall::AskUserQuestion { auth_id, .. } if auth_id == "g3-q1") =>
                {
                    let wire = serde_json::to_value(&call).unwrap();
                    break (id, wire["deliveryId"].as_str().unwrap_or("").to_string());
                }
                _ => {}
            }
        };
        assert!(
            !ask_dlv.is_empty() && ask_dlv != approve_dlv,
            "GW3: each delivery mints its own stable id ({approve_dlv} vs {ask_dlv})"
        );
        client.send(FromClient::Reply {
            id: ask_id,
            outcome: Ok(json!({"answers": [], "response": null})),
        });

        // PlanVerdict, the third waterfall arm, carries one too.
        client.send(FromClient::Notification {
            note: ClientNote::SetPlanMode {
                session_id: "gw3-s1".into(),
                enabled: true,
            },
        });
        client.settle();
        engine
            .notices
            .send(BackendNotice::Event(Box::new(ThreadEvent::PlanReady {
                plan_file: "/nonexistent/gw3-plan.md".into(),
                title: "GW3 plan".into(),
            })))
            .unwrap();
        let (verdict_id, verdict_dlv) = loop {
            match client.recv() {
                FromServer::Request { id, call }
                    if matches!(&call, ServerCall::PlanVerdict { .. }) =>
                {
                    let wire = serde_json::to_value(&call).unwrap();
                    break (id, wire["deliveryId"].as_str().unwrap_or("").to_string());
                }
                _ => {}
            }
        };
        assert!(
            !verdict_dlv.is_empty() && verdict_dlv != approve_dlv && verdict_dlv != ask_dlv,
            "GW3: PlanVerdict carries its own deliveryId ({verdict_dlv})"
        );
        // Refine: consumes the pending review without executing (keeps the
        // session clean for teardown).
        client.send(FromClient::Reply {
            id: verdict_id,
            outcome: Ok(json!({"choice": "refine"})),
        });
        client.settle();
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// GW3 regression: a client withdraws its pending adjudication delivery —
    /// `cancelDelivery` (raw wire JSON below: the pre-GW3 vocabulary cannot
    /// even express the call, its parse failure is red evidence) settles the
    /// waterfall fail-closed through the EXISTING expire/converge path: the
    /// engine receives Deny, the owners an Error note, and the receipt
    /// reports the withdrawal. A second cancel after settlement reports
    /// `cancelled:false`.
    #[test]
    fn cancel_delivery_converges_pending_adjudication() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        manox_agent::thread_store::init();
        let server = AgentServer::new(PathBuf::from("/"));
        let a = connect_sessions(&server, "gw3-a", vec![HookKind::Approve], vec![]);
        let b = connect_sessions(&server, "gw3-b", vec![HookKind::Approve], vec![]);
        create(&server, &a, "gw3-s2");
        // b joins the owner set (the §D.4 fan-out audience).
        b.send(FromClient::Request {
            id: MsgId::new("gw3-open-b"),
            call: ClientCall::OpenSession {
                session_id: "gw3-s2".into(),
            },
        });
        expect(
            &b,
            |m| matches!(m, FromServer::Response { id, outcome: Ok(_), .. } if id.0 == "gw3-open-b"),
        );
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("gw3-s2", engine.clone(), events);
        a.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "gw3-s2".into(),
                text: "do work".into(),
                images: vec![],
                client_id: None,
            },
        });
        expect_host_status(&a, "gw3-s2", |running, _, _, _| running == Some(true));
        expect_host_status(&b, "gw3-s2", |running, _, _, _| running == Some(true));

        // One authorization fans out to BOTH owners (§D.4).
        engine
            .notices
            .send(BackendNotice::Event(Box::new(
                ThreadEvent::ToolCallAuthorization {
                    id: "g3c-a1".into(),
                    tool_name: "Bash".into(),
                    summary: "run ls".into(),
                    input: json!({}),
                },
            )))
            .unwrap();
        let dlv = loop {
            match a.recv() {
                FromServer::Request { call, .. } if matches!(&call, ServerCall::Approve { auth_id, .. } if auth_id == "g3c-a1") =>
                {
                    break serde_json::to_value(&call).unwrap()["deliveryId"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                }
                _ => {}
            }
        };
        assert!(!dlv.is_empty(), "GW3: the delivery carries its id");
        expect(&b, |m| {
            matches!(
                m,
                FromServer::Request {
                    call: ServerCall::Approve { auth_id, .. },
                    ..
                } if auth_id == "g3c-a1"
            )
        });

        // a withdraws its delivery (e.g. it navigated away from the
        // session). Raw wire JSON: the pre-GW3 vocabulary cannot express
        // this call (unknown method — the parse panic is the red evidence).
        let cancel: FromClient = serde_json::from_value(json!({
            "kind": "request",
            "id": "gw3-cancel-1",
            "call": { "method": "cancelDelivery", "deliveryId": dlv },
        }))
        .expect("GW3: the wire vocabulary expresses delivery cancellation");
        a.send(cancel);
        // The receipt races the convergence's Error note (dispatch task vs
        // pump task): drain to the Response.
        let receipt = loop {
            match a.recv() {
                FromServer::Response { id, outcome } if id.0 == "gw3-cancel-1" => break outcome,
                _ => {}
            }
        };
        match receipt {
            Ok(v) => assert_eq!(v["cancelled"], json!(true), "the withdrawal is receipted"),
            Err(e) => panic!("expected the cancel receipt, got err {e:?}"),
        }

        // The waterfall converged fail-closed through the expire path: the
        // engine receives Deny for the authorization.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let denied = engine.auth_responses.lock().unwrap().iter().any(|(id, r)| {
                id == "g3c-a1"
                    && matches!(
                        r,
                        manox_agent::permission::ToolAuthorizationResponse::Decision(
                            manox_agent::permission::PermissionDecision::Deny
                        )
                    )
            });
            if denied {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "GW3: the cancelled delivery never converged to a fail-closed Deny"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // The owners see the rejection's Error mirror.
        expect(&b, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::Error { session_id: Some(sid), .. }
                } if sid == "gw3-s2"
            )
        });
        // After settlement the delivery is unregistered: b's late cancel
        // reports nothing to withdraw (a receipt, never an error).
        let cancel_b: FromClient = serde_json::from_value(json!({
            "kind": "request",
            "id": "gw3-cancel-2",
            "call": { "method": "cancelDelivery", "deliveryId": dlv },
        }))
        .expect("GW3: the wire vocabulary expresses delivery cancellation");
        b.send(cancel_b);
        let receipt_b = loop {
            match b.recv() {
                FromServer::Response { id, outcome } if id.0 == "gw3-cancel-2" => break outcome,
                _ => {}
            }
        };
        match receipt_b {
            Ok(v) => assert_eq!(
                v["cancelled"],
                json!(false),
                "a settled delivery has nothing to withdraw"
            ),
            Err(e) => panic!("expected the late-cancel receipt, got err {e:?}"),
        }
        drop(a);
        drop(b);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// C2 gate: every gateway error carries a §D.7 stable code. Scans this
    /// crate's production sources for `RpcError::new(` constructor calls
    /// whose matching close paren is not immediately followed by
    /// `.with_code(` — an uncoded error is a contract hole (clients switch
    /// on `data.code`; L11). Paren matching is string-aware enough for this
    /// codebase (message literals keep their parens balanced); a site that
    /// trips the gate falsely should still be restructured to chain
    /// `.with_code` directly.
    #[test]
    fn every_production_rpc_error_carries_a_stable_code() {
        const FILES: &[&str] = &[
            "src/agent_server.rs",
            "src/journal_query.rs",
            "src/follow.rs",
            "src/translate.rs",
            "src/agent_client.rs",
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut uncoded: Vec<String> = Vec::new();
        for file in FILES {
            let path = root.join(file);
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            let prod = match source.find("\nmod tests {") {
                Some(idx) => &source[..idx],
                None => source.as_str(),
            };
            let mut from = 0;
            while let Some(rel) = prod[from..].find("RpcError::new(") {
                let start = from + rel;
                // Walk to the matching close paren of `new(`.
                let mut depth = 0usize;
                let mut end = prod.len();
                for (i, ch) in prod[start..].char_indices() {
                    match ch {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                end = start + i + 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                let after = prod[end..].trim_start();
                if !after.starts_with(".with_code(") {
                    let line = prod[..start].matches('\n').count() + 1;
                    uncoded.push(format!("{file}:{line}"));
                }
                from = start + "RpcError::new(".len();
            }
        }
        assert!(
            uncoded.is_empty(),
            "C2: uncoded RpcError sites — every gateway error must carry a §D.7 stable code via `.with_code(...)`: {uncoded:?}"
        );
    }
}
