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
use manox_protocol::handshake::{ClientHello, HookKind, Initialize};
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
/// event pump. The pump is aborted when the session is dropped.
struct ServerSession {
    thread: ThreadHandle,
    _pump: tokio::task::JoinHandle<()>,
    turn_active: Arc<AtomicBool>,
    pending_submits: Arc<StdMutex<Vec<QueuedSubmit>>>,
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
    focused: Arc<StdMutex<Option<String>>>,
    call_seq: AtomicU64,
    /// In-flight bare-model completions by request id (the LanguageModelChat
    /// provider path); cancellation tokens shared with the spawned streams.
    model_chats: Arc<StdMutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// Monotonically increasing counter for client entry generations, used to
    /// detect stale entries during same-client-id reconnection.
    next_generation: AtomicU64,
    /// §E.3 Q-face cache: `(thread_id, cursor)` → the folded conversation
    /// info payload (recomputed only when the cursor advances).
    conversation_info_cache: Arc<StdMutex<journal_query::ConversationInfoCache>>,
}

impl AgentServerInner {
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
            focused: Arc::new(StdMutex::new(None)),
            call_seq: AtomicU64::new(0),
            model_chats: Arc::new(StdMutex::new(HashMap::new())),
            next_generation: AtomicU64::new(1),
            conversation_info_cache: Arc::new(StdMutex::new(
                journal_query::ConversationInfoCache::default(),
            )),
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
                    }),
            }) => {
                if client_id.is_empty() {
                    conn.send_to_client(FromServer::Response {
                        id,
                        outcome: Err(RpcError::new(-1, "empty client_id")),
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
                    old.peer.cancel_all(RpcError::new(-1, "client reconnected"));
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
                for s in &hello.sessions {
                    self.session_owners
                        .lock()
                        .entry(s.clone())
                        .or_default()
                        .push(client_id.clone());
                }
                conn.send_to_client(FromServer::Response {
                    id,
                    outcome: Ok(json!({"ack": true})),
                });
                conn.send_to_client(FromServer::Notification {
                    note: ServerNote::Ready,
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
                    outcome: Err(RpcError::new(-1, "expected Initialize first")),
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
                    // protocol completeness.
                    let push_after = match &call {
                        ClientCall::ListModels => Some(ListPush::Models),
                        ClientCall::ListThreads => Some(ListPush::Threads),
                        ClientCall::ListCommands => Some(ListPush::Commands),
                        _ => None,
                    };
                    let outcome = handle_call(&self, &client_id, call).await;
                    if let Some(push) = push_after {
                        let note = match push {
                            ListPush::Models => ServerNote::Models {
                                models: self.models_snapshot(),
                            },
                            ListPush::Threads => ServerNote::ThreadsUpdated {
                                threads: self.threads_snapshot(),
                            },
                            ListPush::Commands => ServerNote::Commands {
                                commands: self.commands_snapshot(),
                            },
                        };
                        conn.send_to_client(FromServer::Notification { note });
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
    fn note_to_client(&self, client_id: &str, note: manox_protocol::ServerNote) {
        let clients = self.clients.lock();
        if let Some(entry) = clients.get(client_id) {
            entry.conn.send_to_client(FromServer::Notification { note });
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
            sessions.remove(&sid);
            // Ownership lost ⇒ every live stream of the session closes
            // (§D.1 `Closed`).
            self.end_streams_for_session(&sid, StreamEndReason::Closed);
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
        for entry in self.clients.lock().values() {
            entry.conn.send_to_client(frame.clone());
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

    fn note_error(&self, session_id: &str, message: &str) {
        self.route_note(
            session_id,
            ServerNote::Error {
                session_id: Some(session_id.into()),
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
                    unread: t.has_unread,
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
        ClientCall::Initialize(_) => Err(RpcError::new(-1, "already initialized")),
        // ── v2 write calls (§D.2: receipts only, L7). ───────────────────────
        ClientCall::CreateSession {
            cwd,
            project,
            initial_model,
            approval_mode,
            reasoning_effort,
        } => AgentServerInner::create_session_request(
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
        ),
        ClientCall::Submit {
            session_id,
            text,
            images,
            origin_rpc,
        } => inner.submit(client_id, &session_id, text, images, None, origin_rpc),
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
            let thread = inner.session_thread(&session_id).ok_or_else(|| {
                RpcError::new(-1, "unknown session")
                    .with_code(manox_protocol::msg::CODE_SESSION_NOT_FOUND)
            })?;
            journal_query::page_history(&thread, through_seq, before_seq, max_messages).await
        }
        ClientCall::GetConversationInfo { session_id } => {
            let thread = inner.session_thread(&session_id).ok_or_else(|| {
                RpcError::new(-1, "unknown session")
                    .with_code(manox_protocol::msg::CODE_SESSION_NOT_FOUND)
            })?;
            journal_query::conversation_info(&inner.conversation_info_cache, &thread, &session_id)
                .await
        }
        ClientCall::OpenSession { session_id } => open_session(inner, client_id, &session_id).await,
        ClientCall::ListThreads => serde_json::to_value(inner.threads_snapshot())
            .map_err(|_| RpcError::new(-1, "threads serialization failed")),
        ClientCall::ListModels => serde_json::to_value(inner.models_snapshot())
            .map_err(|_| RpcError::new(-1, "models serialization failed")),
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
        ClientCall::TerminalAttach { .. } | ClientCall::TerminalSnapshot { .. } => {
            Err(RpcError::new(-1, "terminal support lands in β-3b"))
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
    // Idempotent reopen: a live session is re-owned instead of loading a
    // second copy. T10 (§D.6): no v1 snapshot replay here — the client's
    // history comes from the §D.1 follow stream's `Snapshot` frame. Use
    // directed `SessionCreated` (not broadcast) to avoid disturbing owners.
    if inner.session_thread(session_id).is_some() {
        inner.add_owner(session_id, owner);
        inner.note_to_client(
            owner,
            ServerNote::SessionCreated {
                session_id: session_id.into(),
            },
        );
        return Ok(json!({ "restored": true }));
    }
    let thread = manox_agent::thread_store::global().with_mut(|s| s.load_thread(session_id));
    let thread = thread.ok_or_else(|| RpcError::new(-1, "thread not found"))?;
    manox_agent::thread_store::global().with_mut(|s| s.set_unread(session_id, false));
    let turn_active = Arc::new(AtomicBool::new(false));
    let pending_submits = Arc::new(StdMutex::new(Vec::new()));
    let pump = spawn_pump(
        Arc::clone(inner),
        session_id.into(),
        thread.clone(),
        turn_active.clone(),
        pending_submits.clone(),
        inner.focused.clone(),
    );
    inner.sessions.lock().insert(
        session_id.into(),
        ServerSession {
            thread: thread.clone(),
            _pump: pump,
            turn_active,
            pending_submits,
        },
    );
    inner.add_owner(session_id, owner);
    inner.route_note(
        session_id,
        ServerNote::SessionCreated {
            session_id: session_id.into(),
        },
    );
    Ok(json!({ "restored": true }))
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
            let _ = AgentServerInner::create_session_request(inner, owner, intent);
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
            let _ = inner.submit(owner, &session_id, text, images, client_id, None);
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
        ClientNote::FocusThread { session_id } => inner.focus_thread(session_id),
        ClientNote::TerminalInput { .. } | ClientNote::TerminalResize { .. } => {
            // β-3b: route to TerminalHandle.
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
    /// existing id answers and the live session is left untouched.
    fn create_session_request(
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
        let pump = spawn_pump(
            Arc::clone(inner),
            session_id.clone(),
            thread.clone(),
            turn_active.clone(),
            pending_submits.clone(),
            inner.focused.clone(),
        );
        inner.sessions.lock().insert(
            session_id.clone(),
            ServerSession {
                thread: thread.clone(),
                _pump: pump,
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
        }
        self.remove_owner(owner, session_id);
        if self.owners(session_id).is_empty()
            && let Some(session) = self.sessions.lock().remove(session_id)
        {
            // Disposal closes every live stream of the session (§D.1
            // `Closed`).
            self.end_streams_for_session(session_id, StreamEndReason::Closed);
            if session.turn_active.load(Ordering::SeqCst) {
                session.thread.with_mut(|t| t.cancel());
                manox_agent::thread_store::global().with_mut(|s| s.mark_idle(session_id));
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
        }
        self.remove_owner(owner, session_id);
        if self.owners(session_id).is_empty() {
            self.sessions.lock().remove(session_id);
            // Ownership lost ⇒ live streams close (§D.1 `Closed`).
            self.end_streams_for_session(session_id, StreamEndReason::Closed);
        }
    }

    /// §D.2 `Submit`: performs the submission and answers with the receipt
    /// `{accepted, message_id?}` (L7 — the transcript arrives through the
    /// follow stream). The `origin_rpc` correlation is accepted but not
    /// journaled: the kernel user-message row has no origin field yet
    /// (kernel-type change, lands at T5 — T4 gap in the delivery report);
    /// the receipt + `message_id` is the interim retirement key. The
    /// compat `ClientNote::Submit` forwards here with `origin_rpc = None`.
    fn submit(
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
        // `slash` consumed the text display; the outcome distinguishes an
        // empty submission (accepted = false) from a command / transcript
        // insert.
        enum Outcome {
            Slash,
            Empty,
            Inserted,
        }
        let outcome = thread.with_mut(|t| {
            t.set_pending_turn_origin(origin_rpc);
            let ui = MessageUiMetadata {
                model_id: t.model().map(|m| m.id.clone()),
                approval_mode: Some(t.permission_mode().as_i64()),
                ..Default::default()
            };
            if let Some((name, args)) = slash {
                let slash_ui = MessageUiMetadata {
                    display_text: Some(text.clone()),
                    ..ui.clone()
                };
                let builtin_hit = t.run_slash_builtin(&name, &args, Some(slash_ui.clone()));
                let command_hit = manox_agent::command::try_global().is_some()
                    && t.submit_command(&name, &args, Some(slash_ui.clone()));
                let skill_hit = manox_agent::skill::try_global().is_some()
                    && t.submit_skill(&name, &args, Some(slash_ui));
                if builtin_hit || command_hit || skill_hit {
                    return Outcome::Slash;
                }
            }
            let content = to_message_content(text, images);
            if content.is_empty() {
                return Outcome::Empty;
            }
            t.insert_user_message_with_content_and_ui_metadata(content, Some(ui));
            t.run_turn();
            Outcome::Inserted
        });
        match outcome {
            Outcome::Empty => receipt(false, None),
            Outcome::Slash => receipt(true, None),
            Outcome::Inserted => {
                let message_id = thread.read(|t| t.last_user_message_id().map(str::to_string));
                receipt(true, message_id)
            }
        }
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

    fn focus_thread(&self, session_id: Option<String>) {
        *self.focused.lock().unwrap() = session_id.clone();
        if let Some(id) = session_id {
            manox_agent::thread_store::global().with_mut(|s| s.set_unread(&id, false));
        }
    }
}

// ── ServerCall routing (β-3b: Approve / AskUserQuestion / PlanVerdict). ─────
async fn route_call(inner: &Arc<AgentServerInner>, session_id: &str, call: ServerCall) {
    let kind = hook_kind_for(&call);
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
            .map(|cid| {
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
                let rx = entry.peer.register(id.clone());
                (cid.clone(), entry.conn.clone(), rx, id)
            })
            .collect::<Vec<_>>()
    };
    if targets.is_empty() {
        fail_closed(inner, session_id, &ctx);
        return;
    }

    if adjudication {
        route_waterfall(inner, session_id, ctx, call, targets).await;
        return;
    }

    let (conn, rx, id) = {
        let (_, conn, rx, id) = targets.into_iter().next().expect("non-empty checked");
        (conn, rx, id)
    };
    conn.send_to_client(FromServer::Request { id, call });
    let outcome = match tokio::time::timeout(CALL_TIMEOUT, rx.recv()).await {
        Ok(Ok(o)) => o,
        _ => Err(RpcError::new(-1, "capability call timed out or cancelled")),
    };
    if outcome.is_err() {
        // Plan §5.2: a timed-out / errored call must surface the reason,
        // mirroring the no-owner fail-closed path.
        inner.route_note(
            session_id,
            ServerNote::Error {
                session_id: Some(session_id.into()),
                message: "capability call timed out or cancelled".into(),
            },
        );
    }
    apply_reply(inner, session_id, ctx, outcome);
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

async fn route_waterfall(
    inner: &Arc<AgentServerInner>,
    session_id: &str,
    ctx: ReplyCtx,
    call: ServerCall,
    targets: Vec<AdjudicationTarget>,
) {
    let (funnel_tx, mut funnel_rx) =
        tokio::sync::mpsc::unbounded_channel::<(String, Result<Value, RpcError>)>();
    let mut waterfall = crate::waterfall::Waterfall::new(session_id.to_string(), {
        let mut ids = targets
            .iter()
            .map(|(cid, ..)| cid.clone())
            .collect::<Vec<_>>();
        ids.sort();
        ids
    });
    for (cid, conn, rx, id) in targets {
        conn.send_to_client(FromServer::Request {
            id,
            call: call.clone(),
        });
        let tx = funnel_tx.clone();
        manox_agent::runtime::handle().spawn(async move {
            let outcome = match tokio::time::timeout(CALL_TIMEOUT, rx.recv()).await {
                Ok(Ok(o)) => o,
                _ => Err(RpcError::new(-1, "adjudication reply timed out")),
            };
            let _ = tx.send((cid, outcome));
        });
    }
    drop(funnel_tx);
    let mut settled: Option<Result<Value, RpcError>> = None;
    while let Some((cid, outcome)) = funnel_rx.recv().await {
        let next = outcome.is_ok();
        if let Some(outcome_of_settler) = waterfall.reply(&cid, next).map(|_why| outcome) {
            settled = Some(outcome_of_settler);
            break;
        }
    }
    let outcome = settled.unwrap_or_else(|| {
        Err(
            RpcError::new(-1, "adjudication unsettled (all deliveries expired)")
                .with_code(manox_protocol::msg::CODE_GATEWAY_INTERNAL),
        )
    });
    if outcome.is_err() {
        inner.route_note(
            session_id,
            ServerNote::Error {
                session_id: Some(session_id.into()),
                message: "adjudication rejected or timed out".into(),
            },
        );
    }
    apply_reply(inner, session_id, ctx, outcome);
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
        ReplyCtx::PlanVerdict { .. } => inner.route_note(
            session_id,
            ServerNote::Error {
                session_id: Some(session_id.into()),
                message: "no client can review this plan".into(),
            },
        ),
        ReplyCtx::Other => {}
    }
}

fn apply_reply(
    inner: &Arc<AgentServerInner>,
    session_id: &str,
    ctx: ReplyCtx,
    outcome: Result<Value, RpcError>,
) {
    match ctx {
        ReplyCtx::Approve { auth_id } => apply_approve_reply(inner, session_id, auth_id, outcome),
        ReplyCtx::AskUser { auth_id } => apply_ask_reply(inner, session_id, auth_id, outcome),
        ReplyCtx::PlanVerdict { plan_file } => {
            apply_plan_verdict(inner, session_id, plan_file, outcome)
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
    inner.route_note(
        session_id,
        ServerNote::Error {
            session_id: Some(session_id.into()),
            message: "no client can answer this approval".into(),
        },
    );
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
    inner.route_note(
        session_id,
        ServerNote::Error {
            session_id: Some(session_id.into()),
            message: "no client can answer this question".into(),
        },
    );
}

fn apply_plan_verdict(
    inner: &Arc<AgentServerInner>,
    session_id: &str,
    plan_file: String,
    outcome: Result<Value, RpcError>,
) {
    let choice = match outcome {
        Ok(v) => v
            .get("choice")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Err(_) => return, // fail-closed: leave plan mode; the engine stays parked.
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
            .map(|cid| {
                let entry = clients.get(cid).expect("just checked");
                let rx = entry.peer.register(id.clone());
                let conn = entry.conn.clone();
                (conn, rx)
            })
    };
    let Some((conn, rx)) = target else {
        return Err(RpcError::new(
            -1,
            "no client can answer this capability call",
        ));
    };
    conn.send_to_client(FromServer::Request { id, call });
    match tokio::time::timeout(CALL_TIMEOUT, rx.recv()).await {
        Ok(Ok(o)) => o,
        _ => Err(RpcError::new(-1, "capability call timed out or cancelled")),
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
    focused: Arc<StdMutex<Option<String>>>,
) -> tokio::task::JoinHandle<()> {
    // Subscribe synchronously so the receiver is registered before any
    // broadcast (a subscribe inside the task can lose events fired before
    // the task is first polled).
    let rx = thread.subscribe();
    manox_agent::runtime::handle().spawn(async move {
        while let Ok(ev) = rx.recv().await {
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
                    let unread = focused.lock().unwrap().as_deref() != Some(session_id.as_str());
                    let id = session_id.clone();
                    manox_agent::thread_store::global().with_mut(|s| {
                        s.mark_idle(&id);
                        s.mark_pending_auth(&id, false);
                        s.mark_pending_plan(&id, false);
                        if !*failed {
                            s.set_errored(&id, false);
                        }
                        if unread {
                            s.set_unread(&id, true);
                        }
                    });
                    inner.broadcast_host(host_status(&session_id, |f| {
                        f.running = Some(false);
                        f.pending_auth = Some(false);
                        f.pending_plan = Some(false);
                        if unread {
                            f.unread = Some(true);
                        }
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
                    // β-3b: initiate PlanVerdict (carries the plan body) and
                    // skip translate's bare PlanReady note — the call is the
                    // actionable review card; the bare note would duplicate.
                    route_call(
                        &inner,
                        &session_id,
                        ServerCall::PlanVerdict {
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
                    manox_agent::thread_store::global().with_mut(|s| {
                        s.mark_background_work(
                            &id,
                            manox_agent::background_task::thread_has_running_tasks(&id),
                        )
                    });
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
        /// Journal read-seam override (§C.3): tests append `JournalEvent`s
        /// through this sender and seed the snapshot read directly, so
        /// follow streams / PageHistory / the fold are exercised without a
        /// live PiEngine actor.
        journal_tx: tokio::sync::broadcast::Sender<manox_agent::engine::JournalFeed>,
        journal_data: StdMutex<manox_agent::engine::JournalSnapshotData>,
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
                    journal_tx: tokio::sync::broadcast::channel(64).0,
                    journal_data: StdMutex::new(manox_agent::engine::JournalSnapshotData {
                        cursor: 0,
                        records: Vec::new(),
                    }),
                }),
                events,
            )
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
            let _ = tx.send(self.journal_data.lock().unwrap().clone());
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
        (server, client)
    }

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
        match client.recv() {
            FromServer::Response { id, outcome: Ok(v) } => {
                assert_eq!(id.0, "open");
                assert_eq!(v["restored"], true);
            }
            other => panic!("expected the open ack, got {other:?}"),
        }
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
                        frame: manox_protocol::StreamFrame::Entry { seq, event },
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
                other => panic!("expected snapshots on both streams, got {other:?}"),
            }
        }
        // Live entries route to the right stream only.
        engine1.push_journal(1, jentry("a-1", Some("a-0"), ent_agent_text_delta));
        let got = loop {
            match client.recv() {
                FromServer::StreamItem { stream_id, frame } => break (stream_id, frame),
                FromServer::Notification { .. } => continue,
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
}
