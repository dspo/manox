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

use agent::language_model::{MessageContent, ReasoningEffort};
use agent::thread::{PermissionMode, ThreadHandle};
use agent::thread_engine::BackendNotice;
use agent::{Message, MessageUiMetadata, Thread, ThreadEvent, ThreadId};

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
        }))
    }

    /// Accept a connection: spawn the handshake + dispatch task. The
    /// connection drives itself thereafter.
    pub fn accept(&self, conn: Arc<dyn RpcConnection>) {
        let inner = self.0.clone();
        agent::runtime::handle().spawn(async move {
            inner.serve_connection(conn).await;
        });
    }

    /// Test-only: set a scripted engine on a session before any turn runs, so
    /// the event pump can be exercised without a live provider.
    #[cfg(test)]
    pub fn set_session_engine_for_test(
        &self,
        session_id: &str,
        engine: Arc<dyn agent::thread_engine::ThreadEngine>,
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
        let client_id = match rx.recv().await {
            Ok(FromClient::Request {
                id,
                call:
                    ClientCall::Initialize(Initialize {
                        client_id,
                        capabilities,
                        sessions,
                    }),
            }) => {
                if client_id.is_empty() || self.clients.lock().contains_key(&client_id) {
                    conn.send_to_client(FromServer::Response {
                        id,
                        outcome: Err(RpcError::new(-1, "duplicate or empty client_id")),
                    });
                    return;
                }
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
                client_id
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
                    let outcome = handle_call(&self, &client_id, call).await;
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
        self.remove_client(&client_id);
    }

    // ── Pure state accessors (no spawning). ─────────────────────────────────
    fn session_thread(&self, session_id: &str) -> Option<ThreadHandle> {
        self.sessions
            .lock()
            .get(session_id)
            .map(|s| s.thread.clone())
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
        self.session_owners
            .lock()
            .entry(session_id.to_string())
            .or_default()
            .push(client_id.to_string());
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

    fn remove_client(&self, client_id: &str) {
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
        // β-3a: the display sequence (messages ⊕ UI note cards) needs a
        // dedicated serializer; ship an empty array until that lands.
        self.route_note(
            session_id,
            ServerNote::ThreadHistory {
                session_id: session_id.into(),
                messages,
                display_history: json!([]),
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

    /// Gather every `ThreadInfoPayload` field in a single read closure (no
    /// re-entrant locking of the handle).
    fn build_thread_info_payload(
        &self,
        thread: &ThreadHandle,
        _session_id: &str,
    ) -> ThreadInfoPayload {
        thread.read(|t| {
            let worktree_path = t.worktree_path().map(str::to_string);
            ThreadInfoPayload {
                cwd: t.cwd().to_string_lossy().into_owned(),
                project: t.project().map(|p| p.to_string_lossy().into_owned()),
                display_title: t.display_title(),
                model_id: t.model().map(|m| m.id.clone()),
                model_name: t.model().map(agent::pi_providers::display_name),
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
                worktree_active: worktree_path.is_some(),
                worktree_path,
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
        let models: Vec<Value> = deduped_models(agent::pi_providers::global().models())
            .iter()
            .map(model_json)
            .collect();
        json!(models)
    }

    fn commands_snapshot(&self) -> Value {
        let mut commands = Vec::new();
        for meta in agent::slash_builtins::BUILTIN_SLASH_COMMANDS {
            commands.push(json!({
                "name": meta.name,
                "description": null,
                "kind": "command",
                "argument_hint": null,
                "i18n_key": meta.description_key,
            }));
        }
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::from_iter(
            agent::slash_builtins::BUILTIN_SLASH_COMMANDS
                .iter()
                .map(|m| m.name.to_string()),
        );
        if let Some(registry) = agent::command::try_global() {
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
        if let Some(registry) = agent::skill::try_global() {
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
                        "name": model.map(agent::pi_providers::display_name),
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
        ClientCall::ModelChat { .. } => Err(RpcError::new(-1, "model_chat support lands in β-3b")),
    }
}

async fn open_session(
    inner: &Arc<AgentServerInner>,
    owner: &str,
    session_id: &str,
) -> Result<Value, RpcError> {
    // Idempotent reopen: a live session replays its snapshots instead of
    // loading a second copy.
    if let Some(thread) = inner.session_thread(session_id) {
        inner.add_owner(session_id, owner);
        inner.route_note(
            session_id,
            ServerNote::SessionCreated {
                session_id: session_id.into(),
            },
        );
        inner.emit_history_and_info(&thread, session_id, true);
        return Ok(json!({ "restored": true }));
    }
    let thread = agent::thread_store::global().with_mut(|s| s.load_thread(session_id));
    let thread = thread.ok_or_else(|| RpcError::new(-1, "thread not found"))?;
    agent::thread_store::global().with_mut(|s| s.set_unread(session_id, false));
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
        ClientNote::Goal {
            session_id,
            action,
            objective,
            budget,
            max_rounds,
        } => inner.goal(&session_id, &action, objective, budget, max_rounds),
        ClientNote::StopBackgroundTask { task_id, .. } => {
            agent::runtime::handle().spawn(async move {
                let _ = agent::background_task::stop(&task_id).await;
            });
        }
        ClientNote::ArchiveThread {
            session_id,
            archived,
        } => inner.archive_thread(owner, &session_id, archived),
        ClientNote::PinThread { session_id, pinned } => {
            agent::thread_store::global().with_mut(|s| s.pin_thread(&session_id, pinned));
        }
        ClientNote::FocusThread { session_id } => inner.focus_thread(session_id),
        ClientNote::TerminalInput { .. } | ClientNote::TerminalResize { .. } => {
            // β-3b: route to TerminalHandle.
        }
        ClientNote::CancelModelChat { .. } | ClientNote::Shutdown => {
            // β-3b: model_chat lifecycle / shutdown signal.
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
        if let Some(model) = agent::pi_providers::default_model() {
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
            agent::thread_store::global().with_mut(|s| s.mark_idle(session_id));
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
            && let Some(builtin) = agent::slash_builtins::canonical_builtin(name)
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
                let command_hit = agent::command::try_global().is_some()
                    && t.submit_command(&name, &args, Some(slash_ui.clone()));
                let skill_hit = agent::skill::try_global().is_some()
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
        let registry = agent::pi_providers::global();
        match pi_extensions::model_ref::resolve_model_ref(&registry, id) {
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
        thread.with_mut(|t| t.set_project(cwd.into()));
    }

    fn plan_seed(&self, session_id: &str, plan_file: &str) {
        let Some(thread) = self.session_thread(session_id) else {
            return self.note_error(session_id, "unknown session");
        };
        let plan_file = plan_file.to_string();
        let lang = thread.read(|t| t.agent_language());
        let seed_text = match agent::collaboration_mode::render_plan_mode_approved(lang, &plan_file)
        {
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
        let actor = agent::db::GoalActor::User;
        let result = thread.with_mut(|t| match action {
            "create" => t.set_goal(objective),
            "edit" => t.edit_goal(objective, budget, max_rounds, actor),
            "replace" => t.replace_goal(objective, budget, max_rounds, actor),
            "clear" => t.clear_goal(actor),
            "pause" => t.set_goal_status(
                agent::goal::GoalStatus::Paused,
                Some(agent::goal::GoalBlockReason {
                    code: "user-paused".into(),
                    message: "paused by user".into(),
                }),
                actor,
            ),
            "resume" => t.set_goal_status(agent::goal::GoalStatus::Active, None, actor),
            _ => Ok(()),
        });
        if let Err(e) = result {
            self.note_error(session_id, &e.to_string());
        }
    }

    fn archive_thread(&self, owner: &str, session_id: &str, archived: bool) {
        if archived {
            self.dispose_session(owner, session_id);
            agent::thread_store::global().with_mut(|s| s.archive_thread(session_id, true));
        } else {
            agent::thread_store::global().with_mut(|s| s.archive_thread(session_id, false));
        }
    }

    fn focus_thread(&self, session_id: Option<String>) {
        *self.focused.lock().unwrap() = session_id.clone();
        if let Some(id) = session_id {
            agent::thread_store::global().with_mut(|s| s.set_unread(&id, false));
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
                agent::permission::ToolAuthorizationResponse::Decision(
                    agent::permission::PermissionDecision::Deny,
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
        agent::permission::ToolAuthorizationResponse::Decision(
            agent::permission::PermissionDecision::AllowOnce,
        )
    } else {
        agent::permission::ToolAuthorizationResponse::Decision(
            agent::permission::PermissionDecision::Deny,
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
        Ok(v) => agent::permission::ToolAuthorizationResponse::AskUserQuestion {
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
        Err(_) => agent::permission::ToolAuthorizationResponse::AskUserQuestion {
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
                agent::permission::ToolAuthorizationResponse::AskUserQuestion {
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
    let seed_text = match agent::collaboration_mode::render_plan_mode_approved(lang, &plan_file) {
        Ok(text) => text,
        Err(e) => {
            thread.handle_notice(BackendNotice::Event(Box::new(ThreadEvent::Error(e))));
            return;
        }
    };
    let compact_instructions =
        compact.then(|| agent::collaboration_mode::plan_compact_instructions(lang, &plan_file));
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
impl agent::capability::CapabilityClient for AgentServerCapabilityClient {
    fn browser_op(
        &self,
        op: agent::thread_engine::BrowserOp,
    ) -> futures::future::BoxFuture<'static, Result<agent::thread_engine::BrowserReply, String>>
    {
        let inner = self.0.clone();
        Box::pin(async move {
            let session_id = agent::capability::CURRENT_SESSION
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
                Ok(v) => serde_json::from_value::<agent::thread_engine::BrowserReply>(v)
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
    agent::runtime::handle().spawn(async move {
        while let Ok(ev) = rx.recv().await {
            // Bookkeeping that mirrors the legacy host pump: thread-store list
            // flags and the queued-follow-up drain. No note is emitted here
            // except where translate returns Skip (HistoryRestored).
            match &*ev {
                ThreadEvent::TurnStarted => {
                    turn_active.store(true, Ordering::SeqCst);
                    let id = session_id.clone();
                    agent::thread_store::global().with_mut(|s| {
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
                    agent::thread_store::global().with_mut(|s| {
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
                    agent::thread_store::global().with_mut(|s| s.mark_pending_auth(&id, true));
                }
                ThreadEvent::Error(_) => {
                    let id = session_id.clone();
                    agent::thread_store::global().with_mut(|s| {
                        s.set_errored(&id, true);
                        s.mark_pending_plan(&id, false);
                        s.mark_background_work(&id, false);
                    });
                }
                ThreadEvent::PlanReady { plan_file, title } => {
                    let id = session_id.clone();
                    agent::thread_store::global().with_mut(|s| s.mark_pending_plan(&id, true));
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
                    agent::thread_store::global().with_mut(|s| {
                        s.mark_background_work(
                            &id,
                            agent::background_task::thread_has_running_tasks(&id),
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

fn deduped_models(models: Vec<pi::types::Model>) -> Vec<pi::types::Model> {
    let mut seen = std::collections::HashSet::new();
    models
        .into_iter()
        .filter(|m| seen.insert((m.provider.clone(), m.id.clone())))
        .collect()
}

fn model_json(model: &pi::types::Model) -> Value {
    json!({
        "id": model.id,
        "name": agent::pi_providers::display_name(model),
        "provider": model.provider,
        "provider_name": agent::pi_providers::display_provider_name(model),
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
    use crate::session::tests::{hermetic_home, init_globals, lock_globals};
    use manox_protocol::in_process_pair;

    /// A scripted engine: records runs/steers/authorizations and lets a test
    /// inject `BackendNotice`s to drive the pump.
    struct FakeEngine {
        runs: StdMutex<Vec<String>>,
        steer_calls: StdMutex<Vec<String>>,
        notices: tokio::sync::mpsc::UnboundedSender<BackendNotice>,
        auth_responses: StdMutex<Vec<(String, agent::permission::ToolAuthorizationResponse)>>,
        pending_auth: StdMutex<Vec<(String, agent::permission::PendingAuthMeta)>>,
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

    impl agent::thread_engine::ThreadEngine for FakeEngine {
        fn is_running(&self) -> bool {
            false
        }
        fn history(&self) -> Vec<agent::db::HistoryEntry> {
            Vec::new()
        }
        fn request_token_usage(&self) -> HashMap<String, agent::TokenUsage> {
            HashMap::new()
        }
        fn model(&self) -> Option<pi::types::Model> {
            None
        }
        fn run(&self, prompt: String, _: Vec<pi::types::ContentBlock>) {
            self.runs.lock().unwrap().push(prompt);
        }
        fn steer(&self, text: String, _: Vec<pi::types::ContentBlock>) -> String {
            self.steer_calls.lock().unwrap().push(text);
            String::new()
        }
        fn cancel_steer(&self, _: &str) -> bool {
            false
        }
        fn abort(&self) {}
        fn set_model(&self, _: pi::types::Model) {}
        fn set_thinking_level(&self, _: Option<String>) {}
        fn open_session(&self, _: PathBuf) {}
        fn new_session(&self, _: PathBuf, _: Option<PathBuf>) {}
        fn active_session_path(&self) -> Option<PathBuf> {
            None
        }
        fn session_list(&self) -> Vec<agent::ThreadSummary> {
            Vec::new()
        }
        fn pending_auth_entries(&self) -> Vec<(String, agent::permission::PendingAuthMeta)> {
            self.pending_auth.lock().unwrap().clone()
        }
        fn respond_tool_authorization(
            &self,
            id: &str,
            response: agent::permission::ToolAuthorizationResponse,
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
        agent::thread_store::init();
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
        agent::thread_store::drop_global_for_test();
    }

    #[test]
    fn open_session_replays_thread_history() {
        let _g = lock_globals();
        hermetic_home();
        let sessions = agent::paths::manox_config_dir()
            .expect("config dir")
            .join("pi-sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        seed_session_file(&sessions, "s1", "/proj");
        init_globals();
        agent::thread_store::init();
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
        agent::thread_store::drop_global_for_test();
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
                    agent::permission::ToolAuthorizationResponse::Decision(
                        agent::permission::PermissionDecision::AllowOnce
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
        agent::thread_store::drop_global_for_test();
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
                    agent::permission::ToolAuthorizationResponse::Decision(
                        agent::permission::PermissionDecision::Deny
                    )
                ))
        );
        drop(client);
        drop(server);
        agent::thread_store::drop_global_for_test();
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
        agent::thread_store::drop_global_for_test();
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
        agent::thread_store::drop_global_for_test();
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
                    tool_name: agent::tools::ASK_USER_QUESTION.to_string(),
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
                        agent::permission::ToolAuthorizationResponse::AskUserQuestion { .. }
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
        agent::thread_store::drop_global_for_test();
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
        agent::thread_store::drop_global_for_test();
    }
    #[test]
    fn browser_op_routes_to_client_and_returns_reply() {
        let _g = lock_globals();
        hermetic_home();
        init_globals();
        agent::thread_store::init();
        agent::capability::drop_provider_for_test();
        let (server, client) = harness(vec![HookKind::BrowserOp]);
        agent::capability::set_provider(Arc::new(AgentServerCapabilityClient::new(&server)));
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
                op: agent::thread_engine::BrowserOp::Open {
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
                serde_json::to_value(agent::thread_engine::BrowserReply::TabId(1)).unwrap(),
            ),
        });
        // The engine's responder got the BrowserReply.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(reply) = rx.try_recv() {
                assert!(reply.is_ok(), "browser op should succeed, not fail-closed");
                assert!(matches!(
                    reply.unwrap(),
                    agent::thread_engine::BrowserReply::TabId(_)
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
        agent::capability::drop_provider_for_test();
        agent::thread_store::drop_global_for_test();
    }
}
