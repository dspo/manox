//! AgentServer — the single protocol gateway.
//!
//! The only public surface between frontends and the gpui-free kernel: every
//! client (the gpui desktop in-process, and any WS-gateway or napi host)
//! speaks [`manox_protocol`] over
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
    // parking_lot (round 4 §3.2): a panic inside a lock holder must not
    // poison the queue into a permanent cascade — the std .unwrap() locks
    // turned one panic into every subsequent submit/steer/drain panicking.
    pending_submits: Arc<Mutex<Vec<QueuedSubmit>>>,
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
    /// §D.6 replay: in-flight adjudications per session — the authoritative
    /// copy the gateway re-delivers to every owner that joins later (open /
    /// re-own / handshake re-declaration), keyed by the deterministic
    /// MsgId identity (`auth_id`, or `plan_file` for PlanVerdict). A record
    /// lives until its adjudication settles at `apply_reply`; `targets`
    /// holds the owners already holding a live waiter so a re-join of an
    /// existing target never re-registers (GW2 duplicate guard).
    pending_adjudications: Mutex<HashMap<String, Vec<PendingAdjudication>>>,
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

    /// Insert only when the key is absent, under ONE lock hold (§二.4③).
    /// The create path's live-session check and its insert were two lock
    /// acquisitions, so a racing same-id create/open could pass both checks
    /// and mint two pumps — the old replace-and-stop_pump insert kept both
    /// from running, but at the cost of CLOBBERING the winner's fresh state
    /// (model / approval / effort seeds lost to the loser's). This recheck
    /// adopts the entry that won the race; the loser is returned to its
    /// caller for disposal (same shape as `open_session`'s phase-3).
    fn insert_session_if_absent(
        &self,
        session_id: String,
        session: ServerSession,
    ) -> Option<ServerSession> {
        let mut sessions = self.sessions.lock();
        if sessions.contains_key(&session_id) {
            return Some(session);
        }
        sessions.insert(session_id, session);
        None
    }

    /// Register a live stream and return its control handle.
    ///
    /// A key that is already live means the previous stream task is being
    /// replaced while still running: `untrack_stream` is identity-guarded,
    /// so an unconditionally overwritten handle would be unreachable from
    /// BOTH ends forever (no end request, no unregister — a live orphan).
    /// The superseded stream is therefore ENDED (`Closed`) under the same
    /// insert (§二.4②) — its task sends its one `StreamEnd` and the
    /// identity guard keeps the new entry intact.
    fn track_stream(&self, client_id: &str, stream_id: &StreamId, handle: StreamHandle) {
        let replaced = self
            .streams
            .lock()
            .insert((client_id.to_string(), stream_id.clone()), handle);
        if let Some(old) = replaced {
            tracing::warn!(
                client_id,
                stream_id = stream_id.0,
                "replaced a live stream entry; ending the superseded stream"
            );
            old.end(StreamEndReason::Closed);
        }
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
        Self::new_inner(cwd, true)
    }

    /// Test-only constructor WITHOUT the U6a store watcher: the
    /// strict-frame-sequence tests keep deterministic streams (the
    /// watcher's list-refresh broadcasts are pinned by their dedicated
    /// regression, `store_change_broadcasts_the_list_refresh`, through
    /// `harness_with_store_watcher`).
    #[cfg(test)]
    pub fn new_without_store_watcher(cwd: PathBuf) -> Self {
        Self::new_inner(cwd, false)
    }

    fn new_inner(cwd: PathBuf, store_watcher: bool) -> Self {
        let inner = Arc::new(AgentServerInner {
            cwd,
            sessions: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            session_owners: Mutex::new(HashMap::new()),
            streams: Mutex::new(HashMap::new()),
            call_seq: AtomicU64::new(0),
            delivery_seq: Mutex::new(HashMap::new()),
            pending_deliveries: Mutex::new(HashMap::new()),
            pending_adjudications: Mutex::new(HashMap::new()),
            model_chats: Arc::new(StdMutex::new(HashMap::new())),
            next_generation: AtomicU64::new(1),
            conversation_info_cache: Arc::new(StdMutex::new(
                journal_query::ConversationInfoCache::default(),
            )),
            pumps_spawned: AtomicU64::new(0),
            pumps_finished: AtomicU64::new(0),
        });
        // U2 cross-domain #2 (§D.5 Models: pushed immediately on provider reload): a
        // provider reload broadcasts the fresh snapshot to every
        // connection. Weak, so a dropped server leaves an inert listener;
        // a newer server re-registers (last one wins).
        manox_agent::provider_glue::set_reload_listener(Some(Box::new({
            let weak = Arc::downgrade(&inner);
            move || {
                if let Some(inner) = weak.upgrade() {
                    inner.broadcast_models_after_reload();
                }
            }
        })));
        // U6a (§D.5 ThreadsUpdated — full snapshot on metadata change — made real): the
        // store-event watcher owns the list-refresh broadcast — any store
        // summary write (title auto-stamps, interacted_at bumps, pin/
        // archive/tag, rescans) reaches EVERY connection as the
        // ThreadsUpdated push, so no client needs an in-process store
        // subscription to keep its list fresh (the desktop's store-event
        // bridge retired against this). Weak like the reload listener: a
        // dropped server leaves the watcher inert, and the store's channel
        // close (teardown) retires the task. A store-less construction
        // (foreign fixtures) skips the watcher — there is nothing to watch.
        if store_watcher && let Some(store) = manox_agent::thread_store::try_global() {
            let rx = store.subscribe();
            let weak = Arc::downgrade(&inner);
            manox_agent::runtime::handle().spawn(async move {
                use manox_agent::thread_store::ThreadStoreEvent;
                while let Ok(ev) = rx.recv().await {
                    // Coalesce bursts (a bulk rescan or the startup refresh
                    // pushes one event per write): one broadcast covers the
                    // drained batch. RunningChanged is skipped — the running
                    // column rides the §D.5 SessionStatus deltas.
                    let mut summaries = matches!(*ev, ThreadStoreEvent::SummariesUpdated);
                    while let Ok(more) = rx.try_recv() {
                        summaries |= matches!(*more, ThreadStoreEvent::SummariesUpdated);
                    }
                    if !summaries {
                        continue;
                    }
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    inner.broadcast_threads_after_store_change();
                }
            });
        }
        Self(inner)
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
                let mut reseated = false;
                if let Some(old) = self.clients.lock().get(&client_id) {
                    // `client/reseated` is the waiter-side signal that the
                    // settle obligation transfers to the §D.6 replay, not a
                    // delivery failure: the waterfall abandons the old
                    // recipient instead of fail-closing the adjudication.
                    old.peer.cancel_all(
                        RpcError::new(-1, "client reconnected")
                            .with_code(manox_protocol::msg::CODE_CLIENT_RESEATED),
                    );
                    old.conn.disconnect();
                    // §D.1: the replaced connection's streams die with it
                    // (`Closed`). Safe here — the new connection cannot have
                    // opened any stream yet (handshake is first).
                    self.end_streams_for_client(&client_id);
                    reseated = true;
                }
                // A re-seat that drops a session declaration is the owner
                // abandoning that session's parked calls: retire them
                // fail-closed here, because the replay below will never
                // re-deliver to a session this hello does not declare.
                // Outside the `clients` lock — the retirement routes notes
                // through the client registry.
                if reseated {
                    self.abandon_unredeclared_adjudications(&client_id, &sessions);
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
                // §D.6: a handshake is a join — re-deliver every unsettled
                // adjudication of the declared sessions to this owner. The
                // gateway's pending registry is the authoritative copy, so a
                // parked card resurfaces on reconnect without the client
                // having to have seen the original fan-out.
                for s in &hello.sessions {
                    self.replay_pending_adjudications(&client_id, s);
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
                                // U2 cross-domain #1: the known-projects
                                // registry rides the list push (host-only —
                                // a new surface, no v1 consumer for an
                                // unsolicited registry push). The desktop's
                                // store-event pump refetches ListThreads
                                // after register_project, so the registry
                                // snapshot stays in lockstep with the rows.
                                let known = manox_agent::thread_store::try_global()
                                    .map(|store| store.read(|s| s.known_projects().to_vec()))
                                    .unwrap_or_default();
                                conn.send_to_client(FromServer::Host {
                                    host: HostEvent::Projects { known },
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
        // Generation guard + removal under ONE lock hold (§二.4①). The old
        // check-then-remove pair took the clients lock twice: a same-id
        // reconnect landing in between installed a newer generation, and the
        // unconditional remove then deleted the NEW entry. One hold closes
        // the window — a stale generation never removes a fresher entry.
        let removed = {
            let mut clients = self.clients.lock();
            match clients.get(client_id) {
                Some(entry) if entry.generation == generation => clients.remove(client_id),
                _ => None,
            }
        };
        let Some(_entry) = removed else {
            return;
        };
        // Disconnect clears this connection's live streams (§D.1 `Closed`;
        // the sends into the closed connection are no-ops by then).
        self.end_streams_for_client(client_id);
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

    // ── §D.6 adjudication replay. ──────────────────────────────────────────
    fn register_pending_adjudication(&self, session_id: &str, rec: PendingAdjudication) {
        self.pending_adjudications
            .lock()
            .entry(session_id.to_string())
            .or_default()
            .push(rec);
    }

    /// A settled adjudication leaves the replay registry: no owner joining
    /// afterwards is ever re-delivered it. Keys only ever collide within one
    /// session's list (`auth_id` is globally unique; a session parks at most
    /// one plan review at a time, so `plan_file` is its session-local key).
    fn retire_pending_adjudication(&self, session_id: &str, key: &str) {
        let mut pending = self.pending_adjudications.lock();
        if let Some(list) = pending.get_mut(session_id) {
            list.retain(|rec| rec.key != key);
            if list.is_empty() {
                pending.remove(session_id);
            }
        }
    }

    /// §D.6: re-deliver every unsettled adjudication of `session_id` to an
    /// owner that just joined it (the gateway's authoritative pending copy).
    /// A new waiter mints for owners that never received the fan-out; an
    /// owner already holding a live waiter gets the same deterministic
    /// `MsgId` frame re-sent WITHOUT re-registering (GW2 would refuse the
    /// duplicate, and its reply still flows through the open waiter) — that
    /// is the thread-switch-back path: the client lost its local card state,
    /// the gate never did. The engine's pending set is the settle truth for
    /// Approve/AskUser; the first delivery to settle answers wins
    /// (`gate.respond` ignores late duplicates). Synchronous registration +
    /// `send_to_client` only — the reply wait is a spawned task, so this is
    /// safe to call from the dispatch loop.
    fn replay_pending_adjudications(self: &Arc<Self>, client_id: &str, session_id: &str) {
        let recs: Vec<PendingAdjudication> = self
            .pending_adjudications
            .lock()
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        if recs.is_empty() {
            return;
        }
        let live_auth_ids: std::collections::HashSet<String> = self
            .session_thread(session_id)
            .map(|t| {
                t.read(|t| {
                    t.pending_auth_entries()
                        .into_iter()
                        .map(|(id, _)| id)
                        .collect()
                })
            })
            .unwrap_or_default();
        let (conn, peer, hello, generation) = {
            let clients = self.clients.lock();
            let Some(entry) = clients.get(client_id) else {
                return;
            };
            (
                entry.conn.clone(),
                entry.peer.clone(),
                entry.hello.clone(),
                entry.generation,
            )
        };
        for rec in recs {
            if !hello.can(rec.kind) {
                continue;
            }
            // The settle check and the `register`/`add_adjudication_target`
            // below are separate lock acquisitions: a settle landing in that
            // window retires the record while this replay still delivers a
            // frame. Benign by construction — the retired record's gate
            // already settled first-wins, so the fresh waiter's reply
            // double-applies into the same idempotent gate, and the next
            // join re-checks and finds the record gone.
            let gate_settled = matches!(rec.kind, HookKind::Approve | HookKind::AskUserQuestion)
                && !live_auth_ids.contains(&rec.key);
            if gate_settled {
                self.retire_pending_adjudication(session_id, &rec.key);
                continue;
            }
            let id = MsgId::new(rec.key.clone());
            if rec
                .targets
                .iter()
                .any(|(cid, target_gen)| cid == client_id && *target_gen == generation)
                && peer.has_waiter(&id)
            {
                // The owner's CURRENT connection still carries the live
                // waiter: re-send the frame only (GW2 forbids a duplicate
                // register; the reply flows through the open waiter).
                conn.send_to_client(FromServer::Request {
                    id,
                    call: rec.call.clone(),
                });
                continue;
            }
            // A target whose waiter died with a re-seated connection must
            // re-mint here: the entry's peer is a fresh instance, so without
            // this the client's eventual reply would resolve nothing and the
            // call would strand until timeout.
            let Some(rx) = peer.register(id.clone()) else {
                continue;
            };
            self.add_adjudication_target(session_id, &rec.key, client_id, generation);
            conn.send_to_client(FromServer::Request {
                id,
                call: rec.call.clone(),
            });
            // Not registered under the delivery's GW3 cancel tokens: the
            // settling waterfall's `DeliveryGuard` cancels the whole
            // delivery id, which would mis-kill this owner's fresh waiter.
            // A replayed delivery superseded elsewhere converges on
            // CALL_TIMEOUT; the engine gate's first-wins idempotence absorbs
            // the late double-apply.
            let inner = Arc::clone(self);
            let sid = session_id.to_string();
            manox_agent::runtime::handle().spawn(async move {
                let outcome = match tokio::time::timeout(CALL_TIMEOUT, rx.recv()).await {
                    Ok(Ok(o)) => o,
                    _ => Err(RpcError::new(-1, "replayed adjudication reply timed out")
                        .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL)),
                };
                apply_reply(&inner, &sid, rec.ctx, outcome, None);
            });
        }
    }

    /// §D.6: an owner that re-seats WITHOUT re-declaring a session has
    /// abandoned that session's parked calls — no future replay reaches it,
    /// so a record whose targets empty out retires and fail-closes now.
    /// (A record with other targets just loses this recipient from the
    /// authoritative pending copy; their waterfalls learn of the hand-off
    /// through the `client/reseated` funnel event.)
    fn abandon_unredeclared_adjudications(self: &Arc<Self>, client_id: &str, declared: &[String]) {
        let drained: Vec<(String, PendingAdjudication)> = {
            let mut pending = self.pending_adjudications.lock();
            let mut drained = Vec::new();
            for (session_id, list) in pending.iter_mut() {
                if declared.iter().any(|s| s == session_id) {
                    continue;
                }
                let mut keep = Vec::new();
                for mut rec in list.drain(..) {
                    rec.targets.retain(|(cid, _)| cid != client_id);
                    if rec.targets.is_empty() {
                        drained.push((session_id.clone(), rec));
                    } else {
                        keep.push(rec);
                    }
                }
                *list = keep;
            }
            pending.retain(|_, list| !list.is_empty());
            drained
        };
        for (session_id, rec) in drained {
            self.note_error(
                &session_id,
                "adjudication abandoned: owner re-seated without re-declaring the session",
            );
            apply_reply(
                self,
                &session_id,
                rec.ctx,
                Err(RpcError::new(
                    -1,
                    "adjudication abandoned: the answering owner re-seated without the session",
                )
                .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL)),
                None,
            );
        }
    }

    fn add_adjudication_target(
        &self,
        session_id: &str,
        key: &str,
        client_id: &str,
        generation: u64,
    ) {
        if let Some(list) = self.pending_adjudications.lock().get_mut(session_id)
            && let Some(rec) = list.iter_mut().find(|rec| rec.key == key)
        {
            // One row per owner: a re-mint on a newer connection replaces the
            // owner's stale-generation row rather than stacking duplicates.
            rec.targets.retain(|(cid, _)| cid != client_id);
            rec.targets.push((client_id.to_string(), generation));
        }
    }

    // ── Note routing. ──────────────────────────────────────────────────────
    /// §D.5: broadcast a host event to EVERY connected client (global,
    /// change-driven — not owner-scoped like `route_note`).
    /// §D.5 as-built (U2 cross-domain #2): the provider-reload broadcast —
    /// Host frame only, to every connection. An unsolicited Models push has
    /// no v1 note consumer (both migrated clients fold HostEvent::Models);
    /// the ListModels RESPONSE side keeps its GW1 dual-emit.
    fn broadcast_models_after_reload(&self) {
        let models = self.models_snapshot();
        self.broadcast_host(HostEvent::Models { models });
    }

    /// U6a: the list-refresh broadcast on a store change — the same frame
    /// shape as the `ListPush::Threads` arm (the v1 note first, then the
    /// Host mirror, then the host-only `Projects` registry — GW1 dual emit
    /// to the global list audience), sent to EVERY connection (the list is
    /// a global registry channel, not owner-scoped).
    fn broadcast_threads_after_store_change(&self) {
        let threads = self.threads_snapshot();
        let known = manox_agent::thread_store::try_global()
            .map(|store| store.read(|s| s.known_projects().to_vec()))
            .unwrap_or_default();
        // Clone the connection list under the lock, then send outside it
        // (the broadcast_host discipline: a stalled peer must not freeze
        // the shared `clients` lock).
        let conns: Vec<Arc<dyn RpcConnection>> = self
            .clients
            .lock()
            .values()
            .map(|entry| entry.conn.clone())
            .collect();
        for conn in conns {
            conn.send_to_client(FromServer::Notification {
                note: ServerNote::ThreadsUpdated {
                    threads: threads.clone(),
                },
            });
            conn.send_to_client(FromServer::Host {
                host: HostEvent::ThreadsUpdated {
                    threads: threads.clone(),
                },
            });
            conn.send_to_client(FromServer::Host {
                host: HostEvent::Projects {
                    known: known.clone(),
                },
            });
        }
    }

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
        // broadcast to non-owners). C4a: the host frame carries the session
        // scope itself — the desktop leaf normalization (the authority face
        // after C4a) filters on it.
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
                session_id: Some(session_id.into()),
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
        // Teardown-tolerant (cross-domain #5 side): the self-held rescan
        // delayed list answers enough that a straggler dispatch task can
        // outlive its test's store guard — a strict global() there panics
        // a foreign worker thread and trips the gpui test scheduler of an
        // UNRELATED test. Production initializes the store for the process
        // lifetime; None answers an empty list.
        let Some(store) = manox_agent::thread_store::try_global() else {
            return Vec::new();
        };
        store.read(|s| {
            s.summaries()
                .iter()
                .map(|t| ThreadListItem {
                    id: t.id.clone(),
                    title: t.display_title().to_string(),
                    // U2-cross-domain #3: the wire column is documented as
                    // the LAST INTERACTION — interacted_at advances on real
                    // activity only, while updated_at advances on every
                    // metadata save and would float stale threads in the
                    // clients' recency ordering.
                    updated_at: t.interacted_at as i32,
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
                    // U2 cross-domain #1: the grouping / label / approval
                    // columns ride the wire row (the sidebar's decoration
                    // push retires against them).
                    project: (!t.project.is_empty()).then(|| t.project.clone()),
                    tag: t.tag.clone(),
                    approval_mode: Some(t.approval_mode),
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
            // §D.2: the cold read does not materialize the engine; the jsonl is
            // read directly (GW6). The live engine
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
                None => match journal_query::cold_read(&session_id).await {
                    journal_query::ColdRead::Data(data) => data,
                    // A live session with no file yet has an EMPTY journal,
                    // not a missing one (unchanged semantics).
                    journal_query::ColdRead::NotFound if thread.is_some() => {
                        manox_agent::engine::JournalSnapshotData {
                            cursor: 0,
                            records: Vec::new(),
                        }
                    }
                    journal_query::ColdRead::NotFound => {
                        return Err(RpcError::new(-1, "unknown session")
                            .with_code(manox_protocol::msg::CODE_SESSION_NOT_FOUND));
                    }
                    // §二.6: a corrupt journal is a loud error, never an
                    // empty page — the old `.ok()?` collapse contradicted
                    // the journal_query contract.
                    journal_query::ColdRead::Corrupt(err) => {
                        return Err(RpcError::new(-1, format!("journal corrupt: {err}"))
                            .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL));
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
        ClientCall::ListThreads => {
            // Cross-domain #5: the rescan self-hold — answer from a FRESH
            // scan (awaited, not the fire-and-forget spawn) so no client
            // needs an in-process rescan trigger. The desktop's
            // store-event bridge retires against this.
            //
            // The scan runs block-in-place on the dispatch worker: the
            // answer keeps the same-poll timing profile it had before the
            // self-hold (an `.await` gap let the response wake slip out of
            // the gpui test scheduler's parked window — its determinism
            // asserts tripped on the foreign-thread wake), and the dispatch
            // loop never interleaves a half-scanned list. The agent runtime
            // is multi-threaded, so one worker blocking on a millisecond
            // scan is contained.
            if !manox_agent::thread_store::test_override_active()
                && let Some(store) = manox_agent::thread_store::try_global()
            {
                tokio::task::block_in_place(|| {
                    manox_agent::runtime::handle().block_on(store.refresh_now())
                });
            }
            serde_json::to_value(inner.threads_snapshot()).map_err(|_| {
                RpcError::new(-1, "threads serialization failed")
                    .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL)
            })
        }
        ClientCall::ListModels => serde_json::to_value(inner.models_snapshot()).map_err(|_| {
            RpcError::new(-1, "models serialization failed")
                .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL)
        }),
        ClientCall::ListCommands => Ok(inner.commands_snapshot()),
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
        let pending_submits = Arc::new(Mutex::new(Vec::new()));
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
    // §D.6: joining a live session re-delivers its unsettled adjudications.
    inner.replay_pending_adjudications(owner, session_id);
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
            // `session_id` is passed through so the desktop ids stay
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
        // U6b①: the browser-suite toggle rides the gateway (the setter-note
        // family shape: a string suite name, fire-and-forget — the effect
        // returns via the facade's BrowserSuitesChanged echo). The desktop's
        // direct facade write was the U6 dual-source face; unknown suite
        // names answer an error note (never a panic).
        ClientNote::SetBrowserSuite {
            session_id,
            suite,
            enable,
        } => {
            let Some(parsed) = manox_agent::engine::BrowserSuite::from_wire(&suite) else {
                inner.note_error(&session_id, &format!("unknown browser suite: {suite}"));
                return;
            };
            let Some(thread) = inner.session_thread(&session_id) else {
                inner.note_error(&session_id, "unknown session");
                return;
            };
            thread.with_mut(|t| t.set_browser_suite(parsed, enable));
        }
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
                    // Connection-scoped (the note's session_id is None):
                    // no leaf owns it, consumers log.
                    session_id: None,
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
    // B5 (review round 2): wire-supplied ids reach this join BEFORE the
    // not-found guards (PageHistory / the follow cold read), so an
    // unvalidated id could probe arbitrary jsonl-shaped files under the
    // manox home ("subagents/<uuid>", "../../x"). Session ids are the
    // minters' uuid charset; admit ASCII alphanumeric, '-' and '_' only —
    // no separators, no dots, no control bytes — and keep this function
    // the sole wire-id → path mint (the repository's own
    // `session_file_name` join reads ids from journal headers, never from
    // the wire).
    if session_id.is_empty()
        || !session_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    // Sessions-dir single authority (review round 3, P0-1): the thread
    // store owns the sessions dir — the production store is built from
    // `paths::sessions_dir()` (same value, no behavior change), a test
    // store points at its standalone temp dir, and the gateway's cold read
    // must resolve through the store's seam or store-side fixtures starve
    // the cold path (the `sidebar_thread_switch_restores_transcript` red).
    // An uninitialized store falls back to the paths authority (the
    // pre-fix behavior).
    let dir = manox_agent::thread_store::try_global()
        .map(|_| manox_agent::thread_store::global_sessions_dir())
        .or_else(|| manox_agent::paths::sessions_dir().ok())?;
    Some(
        dir.join(manox_harness::session::repository::session_file_name(
            session_id,
        )),
    )
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
            // §D.6: joining a live session re-delivers its unsettled
            // adjudications (an orphaned-but-running session parked a card
            // for this owner-to-be).
            inner.replay_pending_adjudications(owner, existing);
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
        let pending_submits = Arc::new(Mutex::new(Vec::new()));
        let pump_cancel = tokio_util::sync::CancellationToken::new();
        let pump = spawn_pump(
            Arc::clone(inner),
            session_id.clone(),
            thread.clone(),
            turn_active.clone(),
            pending_submits.clone(),
            pump_cancel.clone(),
        );
        // GW2 + §二.4③: the insert is a single-lock recheck — if a racing
        // same-id create/open won the race since the live check above, our
        // freshly spawned pump is retired and the WINNER's entry is adopted
        // (never clobbered: the loser's intent seeds must not overwrite the
        // winner's).
        let loser = inner.insert_session_if_absent(
            session_id.clone(),
            ServerSession {
                thread: thread.clone(),
                pump_cancel,
                pump,
                turn_active,
                pending_submits,
            },
        );
        if let Some(loser) = loser {
            tracing::warn!(
                session_id,
                "create lost a same-id race; adopting the winning entry and retiring this pump"
            );
            loser.stop_pump();
            inner.add_owner(&session_id, owner);
            return Ok(json!({ "session_id": session_id }));
        }
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
        // B4 (review round 2): clone the connection in its OWN statement —
        // an `if let` scrutinee temporary (the clients MutexGuard) lives to
        // the end of the body, so the blocking sends below would otherwise
        // run under the lock and one saturated s2c queue would freeze every
        // dispatch, broadcast and route_call on it (§D.7: clone under the
        // lock, send outside it — the route_note pattern).
        let conn = self.clients.lock().get(owner).map(|e| e.conn.clone());
        if let Some(conn) = conn {
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
        // B4 (review round 2): clone the connection in its OWN statement —
        // an `if let` scrutinee temporary (the clients MutexGuard) lives to
        // the end of the body, so the blocking sends below would otherwise
        // run under the lock and one saturated s2c queue would freeze every
        // dispatch, broadcast and route_call on it (§D.7: clone under the
        // lock, send outside it — the route_note pattern).
        let conn = self.clients.lock().get(owner).map(|e| e.conn.clone());
        if let Some(conn) = conn {
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
            pending_submits.lock().push(QueuedSubmit {
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
        // U6b②/④: the implicit dismissal — a Submit landing while a plan
        // review is pending means the user is discussing or revising, not
        // accepting (the desktop consumes its card locally; the durable
        // half lives HERE, where the session facade has the engine that
        // journals the `resolved` plan_review edge). Both planes clear:
        // the session facade (engine cmd → journal + sidecar) and the
        // store badge flag; this turn's own §D.5 deltas then carry the
        // cleared flag to every client.
        if manox_agent::thread_store::try_global()
            .map(|store| store.read(|s| s.pending_plan_contains(session_id)))
            .unwrap_or(false)
        {
            if let Some(thread) = self.session_thread(session_id) {
                thread.with_mut(|t| t.set_plan_review_pending(false));
            }
            if let Some(store) = manox_agent::thread_store::try_global() {
                store.with_mut(|s| s.mark_pending_plan(session_id, false));
            }
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
            // when blank), images as kernel ContentBlocks. K5 edge: the
            // engine expands before persisting, so the entry (and the pin)
            // carry the POST-expansion shape the run announces.
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
        pending_submits.lock().retain(|q| q.client_id != message_id);
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
            pending.lock().retain(|q| q.client_id != client_id);
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
                // The expanded plan directive is written by the harness on
                // the user's behalf, so the bubble header names the harness
                // (the desktop's local seed path always did; U6b③ aligns
                // the gateway path as both migrate onto it).
                author: Some(manox_agent::MessageAuthor::Harness),
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
            // K3 (delivery request): journal the archive decision BEFORE
            // the dispose — while the engine route is still alive the
            // pinned_archived row rides the actor's serializer (K4
            // fail-loud) instead of racing the retire protocol's
            // cold-append fallback. The pump is also still alive, so
            // follow streams deliver the entry before the dispose closes
            // them.
            manox_agent::thread_store::global().with_mut(|s| s.archive_thread(session_id, true));
            self.dispose_session(owner, session_id);
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
            // One lookup carries both the eligibility filter and the entry
            // (round 4 §3.4): the old filter + `expect("just checked")` pair
            // indexed the map twice and panicked the moment the two views
            // disagreed.
            .filter_map(|cid| {
                let entry = clients.get(cid).filter(|e| e.hello.can(kind))?;
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
                    Some(rx) => Some((cid.clone(), entry.generation, entry.conn.clone(), rx, id)),
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
        // §D.6 replay registry: live from fan-out until `apply_reply`
        // settles it — any owner joining this session in the window gets
        // the same call re-delivered.
        inner.register_pending_adjudication(
            session_id,
            PendingAdjudication {
                key: ctx
                    .settle_key()
                    .expect("adjudication kinds all carry a settle key"),
                kind,
                ctx: ctx.clone(),
                call: call.clone(),
                targets: targets
                    .iter()
                    .map(|(cid, generation, ..)| (cid.clone(), *generation))
                    .collect(),
            },
        );
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
        let (_, _, conn, rx, id) = targets.into_iter().next().expect("non-empty checked");
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
/// One adjudication delivery: (client id, connection generation, connection,
/// reply receiver, deterministic MsgId).
type AdjudicationTarget = (
    String,
    u64,
    Arc<dyn RpcConnection>,
    async_channel::Receiver<Result<Value, RpcError>>,
    MsgId,
);

/// One unsettled adjudication, kept so every owner that joins the session
/// later — re-open, re-own, or a handshake re-declaring the session —
/// receives the same still-live call (§D.6 replay). `call` carries the
/// original `delivery_id` (the client-side reply correlation is per
/// delivery; the engine settle is per `auth_id` and first-wins).
#[derive(Clone)]
struct PendingAdjudication {
    key: String,
    kind: HookKind,
    ctx: ReplyCtx,
    call: ServerCall,
    /// Owners holding a live reply waiter for this call, as
    /// (client id, connection generation). One row per owner at most: the
    /// generation is the handshake that minted the waiter, so a re-seat's
    /// replay cannot mistake a same-cid waiter belonging to another
    /// connection's call for its own (GW2 forbids duplicates only within one
    /// generation). Replay re-sends without registering only while that
    /// exact waiter is open on the owner's current connection.
    targets: Vec<(String, u64)>,
}

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

/// One recipient's delivery lifecycle event, funneled from its waiter task.
enum DeliveryEvent {
    /// The client answered: `Ok` = answered next, `Err` = explicit rejection.
    Reply(Result<Value, RpcError>),
    /// The delivery lapsed without an answer (timeout, `CancelDelivery`, or
    /// a closed channel) — kept distinct from a rejection so the GW9
    /// PlanVerdict convergence can name the cause.
    Expired(RpcError),
    /// §D.6: the gateway replaced this owner's connection. The delivery's
    /// settle obligation transfers to the replayed waiter on the new
    /// connection — this waterfall drops the recipient without rejecting.
    Reseated,
}

async fn route_waterfall(
    inner: &Arc<AgentServerInner>,
    session_id: &str,
    ctx: ReplyCtx,
    call: ServerCall,
    targets: Vec<AdjudicationTarget>,
    delivery_id: String,
) {
    let (funnel_tx, mut funnel_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, DeliveryEvent)>();
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
    for (cid, _generation, conn, rx, id) in targets {
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
            let event = tokio::select! {
                _ = token.cancelled() => DeliveryEvent::Expired(
                    RpcError::new(-1, "delivery withdrawn by client (cancelDelivery)").with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL),
                ),
                replied = tokio::time::timeout(CALL_TIMEOUT, rx.recv()) => match replied {
                    // The re-seat cancel resolves this waiter with a coded
                    // Err — the one Err outcome that is a hand-off, not a
                    // delivery failure or a rejection.
                    Ok(Ok(Err(e))) if e.stable_code() == Some(manox_protocol::msg::CODE_CLIENT_RESEATED) => {
                        DeliveryEvent::Reseated
                    }
                    Ok(Ok(o)) => DeliveryEvent::Reply(o),
                    _ => DeliveryEvent::Expired(
                        RpcError::new(-1, "adjudication reply timed out").with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL),
                    ),
                },
            };
            let _ = tx.send((cid, event));
        });
    }
    drop(funnel_tx);
    let mut settled: Option<Result<Value, RpcError>> = None;
    // The settling delivery when it settled the waterfall AGAINST the call:
    // (client id, expired).
    let mut settled_by: Option<(String, bool)> = None;
    // The most recent answered-next payload: a re-seat that completes the
    // all-next quorum settles with the surviving answer.
    let mut last_ok: Option<Value> = None;
    let mut reseat_seen = false;
    while let Some((cid, event)) = funnel_rx.recv().await {
        let (expired, outcome) = match event {
            DeliveryEvent::Reseated => {
                reseat_seen = true;
                if waterfall.abandon(&cid).is_some() {
                    // The only settle `abandon` produces is Allowed: this
                    // removal completed the all-next quorum, so an answer
                    // is already cached.
                    settled = Some(Ok(
                        last_ok.expect("an Allowed quorum holds an answered delivery")
                    ));
                    break;
                }
                continue;
            }
            DeliveryEvent::Reply(o) => (false, o),
            DeliveryEvent::Expired(err) => (true, Err(err)),
        };
        let next = outcome.is_ok();
        if let Ok(value) = &outcome {
            last_ok = Some(value.clone());
        }
        if waterfall.reply(&cid, next).is_some() {
            if !next {
                settled_by = Some((cid, expired));
            }
            settled = Some(outcome);
            break;
        }
    }
    // A re-seat hands the undecided deliveries to the §D.6 replay's fresh
    // waiters: this waterfall must not settle them — an Err would deny a
    // call the replay can still answer, and an Error note would report a
    // hand-off as a failure. This exit's `DeliveryGuard` drop also cancels
    // the surviving co-recipients' tokens: their deliveries are silently
    // retired until they rejoin, where the replay re-mints their waiters.
    if settled.is_none() && reseat_seen {
        return;
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
#[derive(Clone)]
enum ReplyCtx {
    Approve { auth_id: String },
    AskUser { auth_id: String },
    PlanVerdict { plan_file: String },
    Other,
}

impl ReplyCtx {
    /// Identity of the adjudication this context settles: the deterministic
    /// reply MsgId minus its envelope. `None` for capability calls, which
    /// have no re-routable identity.
    fn settle_key(&self) -> Option<String> {
        match self {
            ReplyCtx::Approve { auth_id } | ReplyCtx::AskUser { auth_id } => Some(auth_id.clone()),
            ReplyCtx::PlanVerdict { plan_file } => Some(plan_file.clone()),
            ReplyCtx::Other => None,
        }
    }
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
    // Settlement retires the replay record first: a later owner joining
    // after this point must not be re-delivered a settled call.
    if let Some(key) = ctx.settle_key() {
        inner.retire_pending_adjudication(session_id, &key);
    }
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
    clear_pending_auth_if_settled(inner, session_id);
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
    clear_pending_auth_if_settled(inner, session_id);
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
        // The reply never arrived as an answer (adjudication timeout,
        // withdrawn delivery, disconnected peer): an explicit non-answer —
        // an empty `AskUserQuestion` would read to the model as the user
        // answering nothing on purpose.
        Err(_) => manox_agent::permission::ToolAuthorizationResponse::AskUserQuestionExpired,
    };
    if let Some(thread) = inner.session_thread(session_id) {
        thread.with_mut(|t| t.respond_authorization(&auth_id, response));
    }
    clear_pending_auth_if_settled(inner, session_id);
}

fn respond_ask_fail_closed(inner: &Arc<AgentServerInner>, session_id: &str, auth_id: String) {
    if let Some(thread) = inner.session_thread(session_id) {
        thread.with_mut(|t| {
            t.respond_authorization(
                &auth_id,
                manox_agent::permission::ToolAuthorizationResponse::AskUserQuestionExpired,
            )
        });
    }
    inner.note_error(session_id, "no client can answer this question");
    clear_pending_auth_if_settled(inner, session_id);
}

/// U3b: the verdict-time pending-auth clear. The store flag drops when the
/// LAST authorization settles (the facade's pending set is the truth — a
/// concurrent second authorization keeps the badge up) and the §D.5 delta
/// tells the mirrors. This replaces the desktop's heuristic clears (tool
/// traffic past a parked authorization), which only ever ran in-proc and
/// only for one client.
fn clear_pending_auth_if_settled(inner: &Arc<AgentServerInner>, session_id: &str) {
    let settled = inner
        .session_thread(session_id)
        .is_none_or(|t| t.read(|t| t.pending_auth_entries().is_empty()));
    if !settled {
        return;
    }
    manox_agent::thread_store::global().with_mut(|s| s.mark_pending_auth(session_id, false));
    inner.broadcast_host(host_status(session_id, |f| {
        f.pending_auth = Some(false);
    }));
}

/// U3b: the verdict-time pending-plan clear — the kernel flag is consumed
/// by the verdict arms themselves; this drops the store mirror and tells
/// the §D.5 delta (formerly a desktop-local write, which left every other
/// client's badge stale). The reject/expire path has its own convergence
/// (`converge_plan_rejected`, GW9).
fn clear_pending_plan_flags(inner: &Arc<AgentServerInner>, session_id: &str) {
    manox_agent::thread_store::global().with_mut(|s| s.mark_pending_plan(session_id, false));
    inner.broadcast_host(host_status(session_id, |f| {
        f.pending_plan = Some(false);
    }));
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
        clear_pending_plan_flags(inner, session_id);
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
    clear_pending_plan_flags(inner, session_id);
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
            // Same single-lookup shape as the fan-out site above (§3.4).
            .filter_map(|cid| Some((cid, clients.get(cid).filter(|e| e.hello.can(kind))?)))
            .next()
            .and_then(|(cid, entry)| {
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
    pending_submits: Arc<Mutex<Vec<QueuedSubmit>>>,
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
                        let drained = pending_submits.lock().drain(..).collect::<Vec<_>>();
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
                    // U3b: the Error edge idles the store and clears the
                    // adjudication badges server-side — the desktop mirror
                    // blocks that covered these gaps (mark_idle,
                    // pending_auth) are redundant now and retire after the
                    // desktop list migration.
                    manox_agent::thread_store::global().with_mut(|s| {
                        s.mark_idle(&id);
                        s.set_errored(&id, true);
                        s.mark_pending_plan(&id, false);
                        s.mark_pending_auth(&id, false);
                        s.mark_background_work(&id, false);
                    });
                    inner.broadcast_host(host_status(&session_id, |f| {
                        f.errored = Some(true);
                        f.running = Some(false);
                        f.pending_plan = Some(false);
                        f.pending_auth = Some(false);
                        f.background_work = Some(false);
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
        // U2 cross-domain #4: the config key + agents visibility — the
        // external-CLI launch cascade's columns (config_id falls back to
        // the model id; empty/absent agents = visible to all).
        config_id: Some(manox_agent::provider_glue::config_id(model)),
        agents: model
            .metadata
            .get("agents")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            }),
    }
}

#[cfg(test)]
mod tests;
