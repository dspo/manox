//! Single shared connection multiplexer for the gpui desktop app.
//!
//! One app-level `AgentClient` (client_id `"desktop"`) carries every session
//! over a single in-process connection; a gpui Task pump demuxes incoming
//! `FromServer` to the matching per-session [`ClientStoreHandle`]. T6 extends
//! the demux to the v2 §D.1 stream frames: a `StreamId → session` registry
//! routes `StreamItem`/`StreamEnd`, a `MsgId → session` registry correlates
//! the leaf's `PageHistory` `Response`s, and §D.5 `Host` events broadcast to
//! every leaf. A second pump drains the leaf→server [`LeafRequest`] channel
//! so a handle can re-open its follow stream (seamless resync) or fetch a
//! repair page without owning the connection.
//!
//! The pump is a gpui `Task` spawned on `Context<Self>` so every
//! `entity.update` / `cx.notify` runs on the gpui thread — the
//! `assert_correct_thread` constraint (#754). The agent runtime never wakes
//! a gpui task.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{AppContext as _, Context, Entity, Task};

use manox_protocol::handshake::HookKind;
use manox_protocol::journal::ModelRef;
use manox_protocol::transport::RpcConnection as _;
use manox_protocol::{
    ClientCall, ClientNote, FromClient, FromServer, MsgId, RpcError, StreamId, StreamKind,
};
use manox_session_core::agent_client::AgentClient;
use manox_session_core::agent_server::AgentServer;

use crate::client_store_handle::{ClientStoreHandle, LeafRequest};

/// Outcome of a §D.2 `CreateSession` intent: on success the server-minted id
/// plus the registered (and now-following) leaf handle; on failure a message.
pub enum CreateSessionDone {
    Created {
        session_id: String,
        handle: Entity<ClientStoreHandle>,
    },
    Failed {
        message: String,
    },
}

/// The stable app-level client identity; the server re-seats the entry on a
/// same-id reconnect (the desktop is a singleton, so this never collides).
const CLIENT_ID: &str = "desktop";

/// Capabilities the desktop can adjudicate (mirrors the pre-multiplex
/// per-session handshake).
const CAPABILITIES: &[HookKind] = &[
    HookKind::Approve,
    HookKind::PlanVerdict,
    HookKind::AskUserQuestion,
];

/// One connection, many sessions. The pump reads `server_rx()` and routes
/// each `FromServer` to the [`ClientStoreHandle`] registered for its
/// session (v2 stream frames by `stream_id`, v1 notes by `session_id`,
/// `Response`s by the awaited `MsgId`, §D.5 `Host` to every leaf). Holds
/// strong references to every live handle so parked (background) sessions
/// keep accumulating state while the foreground is elsewhere.
pub struct SessionMultiplexer {
    client: Arc<AgentClient>,
    sessions: HashMap<String, Entity<ClientStoreHandle>>,
    /// `StreamId` → session id: routes `StreamItem`/`StreamEnd` frames.
    streams: HashMap<StreamId, String>,
    /// Awaiting `PageHistory` responses correlated by their request `MsgId`.
    page_fetches: HashMap<MsgId, String>,
    leaf_tx: async_channel::Sender<LeafRequest>,
    /// `ClientCall::CreateSession` responses correlated by request `MsgId`
    /// (the workspace attaches when the server-minted id lands).
    create_callbacks: HashMap<MsgId, Box<dyn FnOnce(CreateSessionDone, &mut Context<Self>)>>,
    _pump: Task<()>,
    _leaf_pump: Task<()>,
}

impl SessionMultiplexer {
    /// Boot the shared client + the single demux pump against `server`.
    pub fn new(server: &AgentServer, cx: &mut Context<Self>) -> Self {
        let client = Arc::new(AgentClient::connect(
            server,
            CLIENT_ID,
            CAPABILITIES.to_vec(),
            Vec::new(),
        ));
        Self::with_client(client, cx)
    }

    /// Wire a pre-built client (handshake already sent) and spawn the pumps.
    /// Production uses [`Self::new`]; tests pass a raw-connection wrapper so
    /// they can inject `FromServer` frames from the server side.
    pub fn with_client(client: Arc<AgentClient>, cx: &mut Context<Self>) -> Self {
        let rx = client.conn().server_rx();
        let _pump = cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            while let Ok(msg) = rx.recv().await {
                let _ = this.update(cx, |m, cx| m.route(msg, cx));
            }
        });
        let (leaf_tx, leaf_rx) = async_channel::unbounded::<LeafRequest>();
        let _leaf_pump = cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            while let Ok(req) = leaf_rx.recv().await {
                let _ = this.update(cx, |m, _| m.handle_leaf_request(req));
            }
        });
        Self {
            client,
            sessions: HashMap::new(),
            streams: HashMap::new(),
            page_fetches: HashMap::new(),
            leaf_tx,
            create_callbacks: HashMap::new(),
            _pump,
            _leaf_pump,
        }
    }

    fn handle_leaf_request(&mut self, req: LeafRequest) {
        match req {
            LeafRequest::Reopen {
                session_id,
                stream_id,
            } => self.open_follow(&session_id, stream_id),
            LeafRequest::PageHistory {
                id,
                session_id,
                through_seq,
            } => {
                let call = ClientCall::PageHistory {
                    session_id: session_id.clone(),
                    through_seq: through_seq as i64,
                    before_seq: None,
                    max_messages: None,
                };
                self.page_fetches.insert(id.clone(), session_id);
                self.client.conn().send_to_server(FromClient::Request { id, call });
            }
        }
    }

    /// Register + send the `StreamOpen` for a session's follow stream, binding
    /// the `stream_id` to the handle's session for routing.
    fn open_follow(&mut self, session_id: &str, stream_id: StreamId) {
        // The leaf must exist before frames route back to it.
        if !self.sessions.contains_key(session_id) {
            tracing::warn!(session = %session_id, "open_follow: no leaf registered");
            return;
        }
        self.streams.insert(stream_id.clone(), session_id.to_string());
        self.client.conn().send_to_server(FromClient::StreamOpen {
            stream_id,
            stream_kind: StreamKind::FollowSession {
                session_id: session_id.to_string(),
                max_messages: None,
            },
        });
    }

    /// Route one `FromServer` to the handle registered for its session
    /// (notifications and ServerCalls carry `session_id`; v2 stream frames
    /// route by `stream_id`; `PageHistory` `Response`s by their `MsgId`).
    /// §D.5 `Host` events broadcast to every leaf (the leaf filters the ones
    /// for its session).
    fn route(&mut self, msg: FromServer, cx: &mut Context<Self>) {
        // §D.5 host events are global: fan out to all leaves, then return.
        if let FromServer::Host { host } = &msg {
            // Mirror `SessionStatus` into the thread-store (sidebar) flags
            // under the §D.5 monotonic rules, so parked and foreground rows
            // stay honest even without a live per-thread subscription.
            if let manox_protocol::stream::HostEvent::SessionStatus {
                session_id,
                running,
                errored,
                unread,
                pending_auth,
                pending_plan,
                background_work,
            } = host
            {
                self.mirror_session_status(
                    session_id,
                    *running,
                    *errored,
                    *unread,
                    *pending_auth,
                    *pending_plan,
                    *background_work,
                );
            }
            for handle in self.sessions.values() {
                let m = FromServer::Host { host: host.clone() };
                handle.update(cx, |h, cx| h.apply_from_server(m, cx));
            }
            return;
        }
        let sid = match &msg {
            FromServer::Notification { note } => {
                // v1 compat: a `SessionCreated` note re-seats the handle and
                // triggers the first follow-stream open (the create path —
                // `open_or_create(reopen=false)` has no client-chosen id, so
                // the id only lands here).
                let sid = note.session_id().map(str::to_string);
                if let Some(sid) = sid.as_ref() {
                    if matches!(note, manox_protocol::ServerNote::SessionCreated { .. })
                        && !self.has_follow(sid)
                    {
                        let stream_id = StreamId::new(uuid::Uuid::new_v4().to_string());
                        self.open_follow(sid, stream_id);
                    }
                }
                sid
            }
            FromServer::Request { call, .. } => Some(call.session_id().to_string()),
            FromServer::Response { id, outcome } => {
                if let Some(cb) = self.create_callbacks.remove(id) {
                    let done = match outcome {
                        Ok(v) => match v.get("session_id").and_then(|s| s.as_str()) {
                            Some(sid) => {
                                let sid = sid.to_string();
                                let handle = self.ensure_leaf(&sid, cx);
                                let stream_id = StreamId::new(uuid::Uuid::new_v4().to_string());
                                self.open_follow(&sid, stream_id);
                                CreateSessionDone::Created { session_id: sid, handle }
                            }
                            None => CreateSessionDone::Failed {
                                message: "create response without session_id".into(),
                            },
                        },
                        Err(e) => CreateSessionDone::Failed {
                            message: e.to_string(),
                        },
                    };
                    cb(done, cx);
                    return;
                }
                if let Some(session_id) = self.page_fetches.remove(id) {
                    let outcome = outcome.clone();
                    if let Some(handle) = self.sessions.get(&session_id).cloned() {
                        handle.update(cx, |h, cx| h.apply_page_response(id.clone(), outcome, cx));
                    }
                }
                return;
            }
            FromServer::StreamItem { stream_id, .. } => self.streams.get(stream_id).cloned(),
            FromServer::StreamEnd { stream_id, reason } => {
                let sid = self.streams.remove(stream_id);
                // A terminal reason ends this stream binding; the leaf's own
                // `apply_stream_end` re-opens on `Resync`/`Failure` (minting a
                // new `StreamId`), so the old mapping must go to avoid a
                // stale route.
                let _ = reason;
                sid
            }
            FromServer::Host { .. } => None,
        };
        let Some(sid) = sid else { return };
        let Some(handle) = self.sessions.get(&sid).cloned() else {
            return;
        };
        handle.update(cx, |h, cx| h.apply_from_server(msg, cx));
    }

    /// Open (reopen) or create a session on the shared connection and register
    /// a fresh leaf handle for it. `reopen = true` rebinds an existing thread
    /// via `OpenSession` (history replays as `ServerNote`s + a follow-stream
    /// `Snapshot`); `false` declares a fresh one via `CreateSession` (the id
    /// comes back on the `SessionCreated` note, which opens the follow stream).
    /// The handle is registered before the request is sent so the pump routes
    /// the server's replies straight to it.
    pub fn open_or_create(
        &mut self,
        session_id: &str,
        cwd: &str,
        reopen: bool,
        cx: &mut Context<Self>,
    ) -> Entity<ClientStoreHandle> {
        let handle = cx.new(|cx| {
            let mut h = ClientStoreHandle::leaf(session_id, cx);
            h.set_outbound(self.leaf_tx.clone());
            h
        });
        self.sessions
            .insert(session_id.to_string(), handle.clone());
        if reopen {
            self.client.send_call(ClientCall::OpenSession {
                session_id: session_id.into(),
            });
            let stream_id = StreamId::new(uuid::Uuid::new_v4().to_string());
            self.open_follow(session_id, stream_id);
        } else {
            self.client.send_note(ClientNote::CreateSession {
                session_id: session_id.into(),
                cwd: Some(cwd.into()),
            });
            // The follow stream opens when the `SessionCreated` note lands
            // (see `route`): the server only has the session after it answers
            // the create, so opening eagerly would race a not-found failure.
        }
        handle
    }

    /// Register (or reuse) a leaf for `session_id`, wired to the outbound
    /// control channel. Does not open a follow stream.
    fn ensure_leaf(
        &mut self,
        session_id: &str,
        cx: &mut Context<Self>,
    ) -> Entity<ClientStoreHandle> {
        if let Some(existing) = self.sessions.get(session_id) {
            return existing.clone();
        }
        let handle = cx.new(|cx| {
            let mut h = ClientStoreHandle::leaf(session_id, cx);
            h.set_outbound(self.leaf_tx.clone());
            h
        });
        self.sessions.insert(session_id.to_string(), handle.clone());
        handle
    }

    /// Create a session from intent (§D.2): the server walks its
    /// `new_in_project` path and answers `{session_id}`; there is no
    /// client-minted id and no local pre-creation. The caller supplies an
    /// `on_done` callback (the workspace attaches when the id lands).
    pub fn create_session_intent(
        &mut self,
        cwd: Option<String>,
        project: Option<String>,
        initial_model: Option<String>,
        approval_mode: Option<String>,
        reasoning_effort: Option<String>,
        on_done: Box<dyn FnOnce(CreateSessionDone, &mut Context<Self>)>,
    ) {
        let id = self.client.send_call(ClientCall::CreateSession {
            cwd,
            project,
            initial_model: initial_model.map(ModelRef::new),
            approval_mode,
            reasoning_effort,
        });
        self.create_callbacks.insert(id, on_done);
    }

    /// §D.5 monotonic mirror of a `SessionStatus` delta into the thread-store
    /// sidebar flags: running takes the latest value, `errored` is a rising
    /// edge cleared by a fresh turn, `unread` only rises (focus clears it),
    /// the pending flags take the latest value.
    fn mirror_session_status(
        &self,
        session_id: &str,
        running: Option<bool>,
        errored: Option<bool>,
        unread: Option<bool>,
        pending_auth: Option<bool>,
        pending_plan: Option<bool>,
        background_work: Option<bool>,
    ) {
        use manox_agent::thread_store::global;
        let id = session_id.to_string();
        global().with_mut(|s| {
            if running == Some(true) {
                // A fresh turn supersedes the previous turn's error edge.
                s.set_errored(&id, false);
                s.mark_running(&id);
            } else if running == Some(false) {
                s.mark_idle(&id);
            }
            if errored == Some(true) {
                s.set_errored(&id, true);
            }
            if unread == Some(true) {
                s.set_unread(&id, true);
            }
            if let Some(p) = pending_auth {
                s.mark_pending_auth(&id, p);
            }
            if let Some(p) = pending_plan {
                s.mark_pending_plan(&id, p);
            }
            if let Some(b) = background_work {
                s.mark_background_work(&id, b);
            }
        });
    }

    /// Drop a session from the multiplexer (the server-side owner is released
    /// separately via `DetachSession`). Keeps parked handles addressable.
    pub fn forget(&mut self, session_id: &str) -> Option<Entity<ClientStoreHandle>> {
        self.streams.retain(|_, s| s != session_id);
        self.sessions.remove(session_id)
    }

    /// The shared client (for `ClientNote` sends and `Reply` verdicts). Replies
    /// correlate by `MsgId` server-side — no per-session routing needed.
    pub fn client(&self) -> &AgentClient {
        &self.client
    }

    /// Send a `ClientNote` scoped to `session_id` (the note carries it).
    pub fn send_note(&self, note: ClientNote) {
        self.client.send_note(note);
    }

    /// Answer a `ServerCall` (Approve / PlanVerdict / AskUserQuestion / …).
    pub fn send_reply(&self, id: MsgId, outcome: Result<serde_json::Value, RpcError>) {
        self.client.send_reply(id, outcome);
    }

    /// Fire-and-forget a `FromClient` frame (used by the transitional send
    /// paths that already build the full `FromClient`).
    pub fn send_raw(&self, msg: FromClient) {
        self.client.conn().send_to_server(msg);
    }

    fn has_follow(&self, session_id: &str) -> bool {
        self.streams.values().any(|s| s == session_id)
    }
}
