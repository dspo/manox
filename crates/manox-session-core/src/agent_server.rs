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
use manox_protocol::server::ThreadInfoPayload;
use manox_protocol::{
    ClientCall, ClientNote, FromClient, FromServer, MsgId, RpcConnection, RpcError, RpcPeer,
    ServerCall, ServerNote,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

use manox_agent::language_model::{MessageContent, ReasoningEffort};
use manox_agent::thread::{PermissionMode, ThreadHandle};
use manox_agent::thread_engine::BackendNotice;
use manox_agent::{Message, MessageUiMetadata, Thread, ThreadEvent, ThreadId};

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
    focused: Arc<StdMutex<Option<String>>>,
    call_seq: AtomicU64,
    /// In-flight bare-model completions by request id (the LanguageModelChat
    /// provider path); cancellation tokens shared with the spawned streams.
    model_chats: Arc<StdMutex<HashMap<String, tokio_util::sync::CancellationToken>>>,
    /// Monotonically increasing counter for client entry generations, used to
    /// detect stale entries during same-client-id reconnection.
    next_generation: AtomicU64,
}

impl AgentServer {
    pub fn new(cwd: PathBuf) -> Self {
        Self(Arc::new(AgentServerInner {
            cwd,
            sessions: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            session_owners: Mutex::new(HashMap::new()),
            focused: Arc::new(StdMutex::new(None)),
            call_seq: AtomicU64::new(0),
            model_chats: Arc::new(StdMutex::new(HashMap::new())),
            next_generation: AtomicU64::new(1),
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
                        _ => None,
                    };
                    let outcome = handle_call(&self, &client_id, call).await;
                    if let Some(push) = push_after
                        && let Ok(value) = &outcome
                    {
                        let note = match push {
                            ListPush::Models => ServerNote::Models {
                                models: value.clone(),
                            },
                            ListPush::Threads => ServerNote::ThreadsUpdated {
                                threads: value.clone(),
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
    fn emit_history_and_info(&self, thread: &ThreadHandle, session_id: &str, restored: bool) {
        let messages = thread.read(|t| strip_messages_for_wire(t.messages()));
        let display_history = serde_json::to_value(thread.read(|t| t.display_history().to_vec()))
            .unwrap_or_else(|_| json!([]));
        self.route_note(
            session_id,
            ServerNote::ThreadHistory {
                session_id: session_id.into(),
                messages,
                display_history,
                auto_approved_tools: None,
                restored,
                loading: false,
            },
        );
        self.route_note(
            session_id,
            ServerNote::ThreadInfo {
                session_id: session_id.into(),
                info: Box::new(self.build_thread_info_payload(thread, session_id)),
            },
        );
    }

    /// Like `emit_history_and_info` but sends only to one specific client
    /// (used for same-client-id reopen to avoid disturbing other owners).
    fn emit_history_and_info_to(
        &self,
        thread: &ThreadHandle,
        session_id: &str,
        restored: bool,
        client_id: &str,
    ) {
        let messages = thread.read(|t| strip_messages_for_wire(t.messages()));
        let display_history = serde_json::to_value(thread.read(|t| t.display_history().to_vec()))
            .unwrap_or_else(|_| json!([]));
        self.note_to_client(
            client_id,
            ServerNote::ThreadHistory {
                session_id: session_id.into(),
                messages,
                display_history,
                auto_approved_tools: None,
                restored,
                loading: false,
            },
        );
        self.note_to_client(
            client_id,
            ServerNote::ThreadInfo {
                session_id: session_id.into(),
                info: Box::new(self.build_thread_info_payload(thread, session_id)),
            },
        );
    }

    /// Gather every `ThreadInfoPayload` field in a single read closure (no
    /// re-entrant locking of the handle).
    fn build_thread_info_payload(
        &self,
        thread: &ThreadHandle,
        _session_id: &str,
    ) -> ThreadInfoPayload {
        thread.read(|t| {
            let cwd_path = t.cwd_path().map(str::to_string);
            ThreadInfoPayload {
                cwd: t.cwd().to_string_lossy().into_owned(),
                project: t.project().map(|p| p.to_string_lossy().into_owned()),
                display_title: t.display_title(),
                model_id: t.model().map(|m| m.id.clone()),
                model_name: t.model().map(manox_agent::provider_glue::display_name),
                model: t
                    .model()
                    .map(|m| serde_json::to_value(m).unwrap_or(serde_json::Value::Null)),
                permission_mode: serde_json::to_value(t.permission_mode())
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
                reasoning_effort: t.reasoning_effort().wire_value().to_string(),
                pinned: t.is_pinned(),
                archived: t.archived(),
                depth: t.depth(),
                agent_label: t.agent_label().to_string(),
                self_author: t.self_author().routing().to_string(),
                cwd_path,
                branch: None, // β-3b: async git lookup → ServerNote::Branch.
                goal: serde_json::to_value(t.goal()).ok(),
                goal_elapsed_seconds: t.goal_elapsed_seconds(),
                plan_mode: t.plan_mode(),
                browser_suites: t
                    .browser_suites()
                    .iter()
                    .map(|s| format!("{s:?}").to_lowercase())
                    .collect(),
                history_phase: format!("{:?}", t.history_phase()).to_lowercase(),
                running: t.is_running(),
                has_interacted: t.has_interacted(),
            }
        })
    }

    fn threads_snapshot(&self) -> Value {
        // β-3a: a plain summary list is deferred (ThreadSummary is not serde);
        // the workspace filter and live ThreadsUpdated push land in β-3b.
        json!([])
    }

    fn models_snapshot(&self) -> Value {
        let models: Vec<Value> = deduped_models(manox_agent::provider_glue::global().models())
            .iter()
            .map(model_json)
            .collect();
        json!(models)
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
}

// ── ClientCall dispatch (free fn — borrowed inner, no move per call). ────────
async fn handle_call(
    inner: &Arc<AgentServerInner>,
    client_id: &str,
    call: ClientCall,
) -> Result<Value, RpcError> {
    match call {
        ClientCall::Initialize(_) => Err(RpcError::new(-1, "already initialized")),
        ClientCall::OpenSession { session_id } => open_session(inner, client_id, &session_id).await,
        ClientCall::ListThreads => Ok(inner.threads_snapshot()),
        ClientCall::ListModels => Ok(inner.models_snapshot()),
        ClientCall::ListCommands => Ok(inner.commands_snapshot()),
        ClientCall::GetUsage { session_id } => inner
            .session_thread(&session_id)
            .ok_or_else(|| RpcError::new(-1, "unknown session"))
            .map(|t| {
                let (usage, cost) = t.read(|t| (t.cumulative_token_usage(), t.cumulative_cost()));
                json!({ "usage": serde_json::to_value(usage).unwrap_or(json!({})), "cost": cost })
            }),
        ClientCall::GetCurrentModel { session_id } => inner
            .session_thread(&session_id)
            .ok_or_else(|| RpcError::new(-1, "unknown session"))
            .map(|t| {
                t.read(|t| {
                    let model = t.model();
                    json!({
                        "id": model.map(|m| m.id.clone()),
                        "name": model.map(manox_agent::provider_glue::display_name),
                    })
                })
            }),
        ClientCall::ThreadInfo { session_id } => {
            let thread = inner
                .session_thread(&session_id)
                .ok_or_else(|| RpcError::new(-1, "unknown session"))?;
            inner.route_note(
                &session_id,
                ServerNote::ThreadInfo {
                    session_id: session_id.clone(),
                    info: Box::new(inner.build_thread_info_payload(&thread, &session_id)),
                },
            );
            Ok(json!({}))
        }
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
    // Idempotent reopen: a live session replays its snapshots instead of
    // loading a second copy. Use directed sending (not broadcast) to avoid
    // disturbing other owners (e.g. a background thread on the same client).
    if let Some(thread) = inner.session_thread(session_id) {
        inner.add_owner(session_id, owner);
        inner.note_to_client(
            owner,
            ServerNote::SessionCreated {
                session_id: session_id.into(),
            },
        );
        inner.emit_history_and_info_to(&thread, session_id, true, owner);
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
    inner.emit_history_and_info(&thread, session_id, true);
    Ok(json!({ "restored": true }))
}

// ── ClientNote dispatch (fire-and-forget). ───────────────────────────────────
async fn handle_note(inner: &Arc<AgentServerInner>, owner: &str, note: ClientNote) {
    match note {
        ClientNote::CreateSession { session_id, cwd } => {
            AgentServerInner::create_session(inner, owner, &session_id, cwd);
        }
        ClientNote::DisposeSession { session_id } => inner.dispose_session(owner, &session_id),
        ClientNote::DetachSession { session_id } => inner.detach_session(owner, &session_id),
        ClientNote::Submit {
            session_id,
            text,
            images,
            client_id,
        } => inner.submit(owner, &session_id, text, images, client_id),
        ClientNote::Steer {
            session_id,
            client_id,
            text,
            images,
        } => inner.steer(&session_id, client_id, text, images),
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
impl AgentServerInner {
    fn create_session(
        inner: &Arc<AgentServerInner>,
        owner: &str,
        session_id: &str,
        cwd: Option<String>,
    ) {
        let cwd = cwd.map(PathBuf::from).unwrap_or_else(|| inner.cwd.clone());
        let thread = Thread::new_fresh(ThreadId(session_id.into()), cwd);
        if let Some(model) = manox_agent::provider_glue::default_model() {
            thread.with_mut(|t| t.set_model(model));
        }
        let mode = thread.read(|t| t.permission_mode());
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
        inner.route_note(
            session_id,
            ServerNote::PermissionModeChanged {
                session_id: session_id.into(),
                mode: serde_json::to_value(mode)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
            },
        );
    }

    fn dispose_session(&self, owner: &str, session_id: &str) {
        // Notify while the disposing client is still an owner (route_note is
        // owner-based; after remove_owner an orphaned session has no
        // recipient). β-3a: single-owner; multi-client dispose semantics
        // (preserve the session for remaining owners) land in β-3b.
        self.route_note(
            session_id,
            ServerNote::SessionDisposed {
                session_id: session_id.into(),
            },
        );
        self.remove_owner(owner, session_id);
        if self.owners(session_id).is_empty()
            && let Some(session) = self.sessions.lock().remove(session_id)
            && session.turn_active.load(Ordering::SeqCst)
        {
            session.thread.with_mut(|t| t.cancel());
            manox_agent::thread_store::global().with_mut(|s| s.mark_idle(session_id));
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
        }
    }

    fn submit(
        &self,
        owner: &str,
        session_id: &str,
        text: String,
        images: Vec<ImageAttachment>,
        client_id: Option<String>,
    ) {
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
            return;
        }
        let Some(session) = self.sessions.lock().get(session_id).map(|s| {
            (
                s.thread.clone(),
                s.turn_active.clone(),
                s.pending_submits.clone(),
            )
        }) else {
            self.note_error(session_id, "unknown session");
            return;
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
            });
            return;
        }
        thread.with_mut(|t| {
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
                    return;
                }
            }
            let content = to_message_content(text, images);
            if content.is_empty() {
                return;
            }
            t.insert_user_message_with_content_and_ui_metadata(content, Some(ui));
            t.run_turn();
        });
    }

    fn steer(
        &self,
        session_id: &str,
        client_id: String,
        text: String,
        images: Vec<ImageAttachment>,
    ) {
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
            return;
        };
        // A steer removes its own parked follow-up so the turn-end drain does
        // not resend the same text as a plain follow-up.
        pending_submits
            .lock()
            .unwrap()
            .retain(|q| q.client_id != client_id);
        let steer_pending: Option<String> = thread.with_mut(|t| {
            let ui = MessageUiMetadata {
                model_id: t.model().map(|m| m.id.clone()),
                approval_mode: Some(t.permission_mode().as_i64()),
                ..Default::default()
            };
            let content = to_message_content(text, images);
            if t.is_running() {
                Some(t.enqueue_steer(content, Some(ui)))
            } else {
                t.insert_user_message_with_content_and_ui_metadata(content, Some(ui));
                t.run_turn();
                None
            }
        });
        // Emit outside the write lock so the thread stays available to the
        // engine pump while the SteerPending note is delivered.
        if let Some(message_id) = steer_pending {
            self.route_note(
                session_id,
                ServerNote::SteerPending {
                    session_id: session_id.into(),
                    client_id,
                    message_id,
                },
            );
        }
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
            Some(model) => thread.with_mut(|t| t.set_model(model)),
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
        let mode = serde_json::from_value::<PermissionMode>(Value::String(mode.to_string()))
            .unwrap_or_default();
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
    // Find an owner that also declared this capability; register the waiter
    // under the clients lock (brief — register is synchronous).
    let target = {
        let owners = inner.owners(session_id);
        let clients = inner.clients.lock();
        owners
            .iter()
            .find(|cid| clients.get(*cid).is_some_and(|e| e.hello.can(kind)))
            .map(|cid| {
                let entry = clients.get(cid).expect("just checked");
                // Deterministic MsgId per kind so a client without bridge state
                // can correlate its Reply: Approve/AskUser echo the auth_id the
                // card carries; PlanVerdict uses the session id (one pending
                // review per session); Other (β-3b-ii capability calls) mints a
                // fresh opaque id.
                let id = match &ctx {
                    ReplyCtx::Approve { auth_id } | ReplyCtx::AskUser { auth_id } => {
                        MsgId::new(auth_id.clone())
                    }
                    ReplyCtx::PlanVerdict { .. } => MsgId::new(session_id.to_string()),
                    ReplyCtx::Other => inner.next_call_id(),
                };
                let rx = entry.peer.register(id.clone());
                let conn = entry.conn.clone();
                (conn, rx, id)
            })
    };
    let Some((conn, rx, id)) = target else {
        fail_closed(inner, session_id, &ctx);
        return;
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
            // flags and the queued-follow-up drain. No note is emitted here
            // except where translate returns Skip (HistoryRestored).
            match &*ev {
                ThreadEvent::TurnStarted => {
                    turn_active.store(true, Ordering::SeqCst);
                    let id = session_id.clone();
                    manox_agent::thread_store::global().with_mut(|s| {
                        s.mark_running(&id);
                        s.set_errored(&id, false);
                    });
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
                    if !*cancelled {
                        let drained = pending_submits
                            .lock()
                            .unwrap()
                            .drain(..)
                            .collect::<Vec<_>>();
                        let drained_any = !drained.is_empty();
                        if drained_any {
                            thread.with_mut(|t| {
                                for q in drained {
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
                                t.run_turn();
                            }
                        });
                    }
                }
                ThreadEvent::ToolCallAuthorization { .. } => {
                    let id = session_id.clone();
                    manox_agent::thread_store::global()
                        .with_mut(|s| s.mark_pending_auth(&id, true));
                }
                ThreadEvent::Error(_) => {
                    let id = session_id.clone();
                    manox_agent::thread_store::global().with_mut(|s| {
                        s.set_errored(&id, true);
                        s.mark_pending_plan(&id, false);
                        s.mark_background_work(&id, false);
                    });
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
                ThreadEvent::HistoryRestored => {
                    // translate Skips this; the pump owns the enriched
                    // authoritative snapshot.
                    inner.emit_history_and_info(&thread, &session_id, false);
                    continue;
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

/// Serialize messages with image bytes trimmed (bounded wire payload).
fn strip_messages_for_wire(messages: &[Message]) -> Value {
    let mut value = serde_json::to_value(messages).unwrap_or(Value::Array(Vec::new()));
    let Some(list) = value.as_array_mut() else {
        return value;
    };
    for msg in list {
        let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in content {
            let Some(map) = block.as_object_mut() else {
                continue;
            };
            if let Some(image) = map.get_mut("Image").and_then(Value::as_object_mut) {
                let byte_len = image
                    .get("data")
                    .and_then(Value::as_str)
                    .map(base64_byte_len)
                    .unwrap_or(0);
                image.remove("data");
                image.insert("byte_len".into(), json!(byte_len));
            }
        }
    }
    value
}

fn base64_byte_len(b64: &str) -> u64 {
    let quarters = (b64.len() as u64) * 3 / 4;
    let padding = b64.bytes().rev().take_while(|&b| b == b'=').count() as u64;
    quarters.saturating_sub(padding)
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

fn model_json(model: &manox_harness::types::Model) -> Value {
    json!({
        "id": model.id,
        "name": manox_agent::provider_glue::display_name(model),
        "provider": model.provider,
        "provider_name": manox_agent::provider_glue::display_provider_name(model),
        "api": model.api,
        "context_window": model.context_window,
        "max_tokens": model.max_tokens,
    })
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
        notices: tokio::sync::mpsc::UnboundedSender<BackendNotice>,
        auth_responses: StdMutex<Vec<(String, manox_agent::permission::ToolAuthorizationResponse)>>,
        pending_auth: StdMutex<Vec<(String, manox_agent::permission::PendingAuthMeta)>>,
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
                    notices,
                    auth_responses: StdMutex::new(Vec::new()),
                    pending_auth: StdMutex::new(Vec::new()),
                }),
                events,
            )
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
        fn set_model(&self, _: manox_harness::types::Model) {}
        fn set_thinking_level(&self, _: Option<String>) {}
        fn open_session(&self, _: PathBuf) {}
        fn new_session(&self, _: PathBuf, _: Option<PathBuf>) {}
        fn set_cwd(&self, _path: std::path::PathBuf) {}

        fn active_session_path(&self) -> Option<PathBuf> {
            None
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
            self.recv_timeout(Duration::from_secs(10))
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

    /// Query `ThreadInfo` and return the typed payload.
    fn thread_info(client: &Client, session_id: &str) -> ThreadInfoPayload {
        client.send(FromClient::Request {
            id: MsgId::new(format!("ti-{session_id}")),
            call: ClientCall::ThreadInfo {
                session_id: session_id.into(),
            },
        });
        loop {
            if let FromServer::Notification {
                note: ServerNote::ThreadInfo { info, .. },
            } = client.recv()
            {
                return *info;
            }
        }
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
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::TurnStarted { session_id } } if session_id == "s1"),
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
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::TurnFinished { session_id, .. } } if session_id == "s1"),
        );
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn open_session_replays_thread_history() {
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
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::ThreadHistory { restored: true, .. }
                }
            )
        });
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::ThreadInfo { .. }
                }
            )
        });
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
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::TurnStarted { .. }
                }
            )
        });
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
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::TurnFinished { .. }
                }
            )
        });
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
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::TurnStarted { .. }
                }
            )
        });
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
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        let (server, client) = harness(vec![]);
        create(&server, &client, "s1");
        // ThreadInfo carries the typed payload with all 22 fields populated.
        client.send(FromClient::Request {
            id: MsgId::new("ti"),
            call: ClientCall::ThreadInfo {
                session_id: "s1".into(),
            },
        });
        let mut info: Option<ThreadInfoPayload> = None;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let FromServer::Notification {
                note: ServerNote::ThreadInfo { info: payload, .. },
            } = client.recv_timeout(Duration::from_secs(2))
            {
                info = Some(*payload);
                break;
            }
        }
        let info = info.expect("ThreadInfo never arrived");
        assert_eq!(info.cwd, "/");
        assert_eq!(info.history_phase, "ready");
        assert_eq!(info.permission_mode, "workspace-write");
        assert_eq!(info.self_author, "lead");
        assert!(!info.running);
        assert!(!info.plan_mode);
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
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::TurnStarted { .. }
                }
            )
        });
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
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::TurnStarted { .. }
                }
            )
        });
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
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::TurnFinished { .. }
                }
            )
        });
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
        // Before the verdict, plan_mode is on (confirms SetPlanMode applied).
        assert!(thread_info(&client, "s1").plan_mode);
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
            if !thread_info(&client, "s1").plan_mode {
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
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::TurnStarted { .. }
                }
            )
        });
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

    #[test]
    fn open_session_snapshot_subscribe_is_atomic() {
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
        // Open the session — the pump subscribes synchronously inside
        // spawn_pump, then emit_history_and_info sends the snapshot. Any
        // event that fires after the subscribe is captured by the
        // subscription and must not appear in the snapshot.
        client.send(FromClient::Request {
            id: MsgId::new("open"),
            call: ClientCall::OpenSession {
                session_id: "s1".into(),
            },
        });
        // Expect SessionCreated.
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::SessionCreated { session_id } } if session_id == "s1"),
        );
        // Expect ThreadHistory (the snapshot).
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::ThreadHistory { session_id, .. }
                } if session_id == "s1"
            )
        });
        // Expect ThreadInfo.
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::ThreadInfo { session_id, .. }
                } if session_id == "s1"
            )
        });
        // Now inject an event — the pump subscribed before the snapshot
        // was sent, so the event must arrive via the subscription stream.
        let (engine, events) = FakeEngine::new();
        server.set_session_engine_for_test("s1", engine.clone(), events);
        engine
            .notices
            .send(BackendNotice::Event(Box::new(ThreadEvent::TurnStarted)))
            .unwrap();
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::TurnStarted { session_id } } if session_id == "s1"),
        );
        // Verify no duplicate TurnStarted. The snapshot is empty (fresh
        // thread with no history), so the subscription should deliver the
        // event exactly once.
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            match client.conn.server_rx().try_recv() {
                Ok(FromServer::Notification {
                    note: ServerNote::TurnStarted { .. },
                }) => {
                    panic!(
                        "duplicate TurnStarted delivered — event is in both snapshot and subscription"
                    );
                }
                Ok(_) => continue,
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(_) => break,
            }
        }
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

        // Sequence the script: TurnStarted must land on BOTH paths before the
        // auth notice is injected — otherwise the dispatch task's TurnStarted
        // and the pump task's Approve interleave non-deterministically (two
        // concurrent server-side sources, not a transport difference).
        let mut seq_ip: Vec<FromServer> = Vec::new();
        loop {
            let m = client_ip.recv();
            let hit = matches!(
                &m,
                FromServer::Notification {
                    note: ServerNote::TurnStarted { .. }
                }
            );
            seq_ip.push(m);
            if hit {
                break;
            }
        }
        let mut seq_sl: Vec<FromServer> = Vec::new();
        loop {
            let m = client_sl.recv_timeout(Duration::from_secs(10));
            let hit = matches!(
                &m,
                FromServer::Notification {
                    note: ServerNote::TurnStarted { .. }
                }
            );
            seq_sl.push(m);
            if hit {
                break;
            }
        }

        // Engine-side script: one authorization round-trip per session,
        // injected only after both paths settled TurnStarted.
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

        // Drain both until each has delivered a ThreadInfo response to a
        // final request (bounded settle, no sleeps beyond the recv polling).
        for (sid, send_ip) in [("sess-inproc", true), ("sess-serde", false)] {
            let req = FromClient::Request {
                id: MsgId::new("info"),
                call: ClientCall::ThreadInfo {
                    session_id: sid.into(),
                },
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
        assert!(got_ip_info && got_sl_info, "both paths answered ThreadInfo");

        // ── Normalize + compare ──
        let normalize = |msgs: Vec<FromServer>| -> Vec<serde_json::Value> {
            msgs.into_iter()
                .filter(|m| {
                    !matches!(
                        m,
                        FromServer::Notification { note }
                            if note.backpressure_policy() == BackpressurePolicy::Drop
                    )
                })
                .map(|m| {
                    let v = serde_json::to_value(&m).expect("serializable");
                    let s = v.to_string();
                    let s = s
                        .replace("sess-inproc", "SESS")
                        .replace("sess-serde", "SESS");
                    serde_json::from_str(&s).expect("re-parses")
                })
                .collect()
        };
        let (nip, nsl) = (normalize(seq_ip), normalize(seq_sl));
        assert_eq!(
            nip, nsl,
            "in-process and serde paths must produce identical FromServer sequences"
        );

        drop(client_ip);
        drop(client_sl);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }

    /// ε-2b: multi-client routing — two clients, two sessions, one server.
    /// Broadcast notes reach every owner; the spec is the observed behavior.
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

        // A note for sa reaches ONLY sa's owner (session routing, not a
        // global broadcast): client_b must not see it.
        engine
            .notices
            .send(BackendNotice::Event(Box::new(ThreadEvent::Error(
                anyhow::anyhow!("routing probe"),
            ))))
            .unwrap();
        expect(&client_a, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::Error { .. }
                }
            )
        });
        // Routing spec: b may hold unrelated pending traffic, but never one of
        // sa's session notes. Drain-and-classify instead of asserting emptiness.
        let mut b_pending = Vec::new();
        while let Ok(m) = client_b.conn.server_rx().try_recv() {
            b_pending.push(m);
        }
        let leaked = b_pending.iter().any(|m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::Error { .. }
                }
            )
        });
        assert!(
            !leaked,
            "client_b must never receive sa's notes: {b_pending:?}"
        );

        // Dispose: each client detaches its own session; the other is
        // unaffected and the server keeps serving.
        client_a.send(FromClient::Notification {
            note: ClientNote::DisposeSession {
                session_id: "sa".into(),
            },
        });
        client_b.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "sb".into(),
                text: "still alive".into(),
                images: vec![],
                client_id: None,
            },
        });
        // sb has no engine bound; the submit surfaces an error note to b —
        // proof the server survived a's dispose.
        expect(&client_b, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::Error { .. }
                }
            )
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
        // First open of s1: must succeed.
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
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::ThreadHistory { restored: true, .. }
                }
            )
        });
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::ThreadInfo { .. }
                }
            )
        });
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
        // Reopen s1 on the new connection: must load the session again.
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
        expect(&client_reconn, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::ThreadHistory { restored: true, .. }
                }
            )
        });
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
        // Drain the reopen's directed snapshot (history + info) so the channel
        // is clean before we probe single-delivery of the turn event.
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::ThreadHistory { .. }
                }
            )
        });
        expect(&client, |m| {
            matches!(
                m,
                FromServer::Notification {
                    note: ServerNote::ThreadInfo { .. }
                }
            )
        });
        // Drive one turn: exactly one TurnStarted must reach the client.
        client.send(FromClient::Notification {
            note: ClientNote::Submit {
                session_id: "s1".into(),
                text: "idempotency probe".into(),
                images: vec![],
                client_id: None,
            },
        });
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::TurnStarted { session_id } } if session_id == "s1"),
        );
        // Nothing further may be queued: a duplicate owner would have delivered
        // a second TurnStarted (or any second note) here.
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
        expect(
            &client,
            |m| matches!(m, FromServer::Notification { note: ServerNote::TurnFinished { session_id, .. } } if session_id == "s1"),
        );
        drop(client);
        drop(server);
        manox_agent::thread_store::drop_global_for_test();
    }
}
