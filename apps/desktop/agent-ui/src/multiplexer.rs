//! Single shared connection multiplexer for the gpui desktop app.
//!
//! One app-level `AgentClient` (client_id `"desktop"`) carries every session
//! over a single in-process connection; a gpui Task pump demuxes incoming
//! `FromServer` to the matching per-session [`ClientStoreHandle`]. T6 extends
//! the demux to the v2 §D.1 stream frames: a `StreamId → session` registry
//! routes `StreamItem`/`StreamEnd`, a `MsgId → session` registry correlates
//! the leaf's `PageHistory` `Response`s, and §D.5 `Host` events broadcast to
//! every leaf. U2 makes the multiplexer the desktop's list/registry state
//! home as well: the `Ready` handshake frame fires the `ListThreads` /
//! `ListModels` / `ListCommands` first pull, the `ThreadsUpdated` / `Models`
//! / `Commands` host mirrors and the MsgId-correlated List responses replace
//! the same state, and `SessionStatus` deltas merge into the rows under the
//! §D.5 monotonic mirror rules — so the sidebar and the model/command
//! surfaces read the gateway, never the kernel store. A second pump drains
//! the leaf→server [`LeafRequest`] channel so a handle can re-open its
//! follow stream (seamless resync) or fetch a repair page without owning the
//! connection.
//!
//! The pump is a gpui `Task` spawned on `Context<Self>` so every
//! `entity.update` / `cx.notify` runs on the gpui thread — the
//! `assert_correct_thread` constraint (#754). The agent runtime never wakes
//! a gpui task.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{App, AppContext as _, Context, Entity, Task};

use manox_protocol::handshake::HookKind;
use manox_protocol::journal::ModelRef;
use manox_protocol::transport::RpcConnection as _;
use manox_protocol::{
    ClientCall, ClientNote, FromClient, FromServer, HostEvent, ModelInfo, MsgId, PROTOCOL_EPOCH,
    RpcError, StreamId, StreamKind, ThreadListItem,
};
use manox_session_core::agent_client::AgentClient;
use manox_session_core::agent_server::AgentServer;

use crate::client_store_handle::{ClientStoreHandle, LeafRequest};

/// The one-shot continuation for a §D.2 `CreateSession` intent.
type CreateCallback = Box<dyn FnOnce(CreateSessionDone, &mut Context<SessionMultiplexer>)>;

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

/// Which §D.2 list call an awaiting `Response` belongs to (U2: the list
/// channels correlate by `MsgId` like the leaf `PageHistory` fetches).
#[derive(Debug, Clone, Copy)]
enum ListFetch {
    Threads,
    Models,
    Commands,
}

/// Boot-race retry budget for an empty model snapshot: the `Ready` first
/// pull can outrun the server's background provider registration, and the
/// server has no registration/reload push yet (§D.5 "Models pushed on
/// provider reload" is a cross-domain ask). Ten tries at a fixed short
/// interval cover the usual keychain-free build; the picker's open-time
/// refetch covers the rest.
const MODELS_EMPTY_RETRY_MAX: u32 = 10;
const MODELS_EMPTY_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

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
    info_fetches: HashMap<MsgId, String>,
    page_fetches: HashMap<MsgId, String>,
    leaf_tx: async_channel::Sender<LeafRequest>,
    /// `ClientCall::CreateSession` responses correlated by request `MsgId`
    /// (the workspace attaches when the server-minted id lands).
    create_callbacks: HashMap<MsgId, CreateCallback>,
    /// Awaiting list-call responses correlated by request `MsgId` (U2).
    list_fetches: HashMap<MsgId, ListFetch>,
    /// §D.5 list channels — the desktop's list/registry state home (U2).
    /// The sidebar and the model/command surfaces read these instead of the
    /// kernel store; they are fed by the handshake pull, the §D.5 Host
    /// mirrors, and the correlated List responses.
    thread_list: Vec<ThreadListItem>,
    models: Vec<ModelInfo>,
    commands: serde_json::Value,
    /// U2 cross-domain #1: the known-projects registry snapshot (§D.5 list
    /// channel family) — the sidebar grouping's wire source, pushed with
    /// every ListThreads answer.
    known_projects: Vec<String>,
    /// The epoch the server accepted (§D.5 `Ready`); the handshake pull only
    /// runs when it equals [`PROTOCOL_EPOCH`] (C1).
    ready_epoch: Option<u32>,
    /// Boot-race retry bookkeeping for an empty model snapshot (see
    /// [`MODELS_EMPTY_RETRY_MAX`]); a non-empty snapshot resets the budget.
    models_retries: u32,
    models_retry_inflight: bool,
    _pump: Task<()>,
    _leaf_pump: Task<()>,
    /// The client-owned focus (§F.2/GW5): the attached session's leaf
    /// suppresses unread rises. Replaces the retired server-side mirror.
    focused: Option<String>,
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
                if let Err(err) = this.update(cx, |m, cx| m.route(msg, cx)) {
                    tracing::warn!(error = %err, "mux pump route update failed");
                }
            }
            tracing::error!("mux pump exited (channel closed)");
        });
        let (leaf_tx, leaf_rx) = async_channel::unbounded::<LeafRequest>();
        let _leaf_pump = cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            while let Ok(req) = leaf_rx.recv().await {
                // Same discipline as the main pump above: a released entity
                // is a normal teardown, but it must not be SILENT — the leaf
                // requests carry reopen / page-history / conversation-info /
                // list-refresh, and losing them without a trace leaves
                // session-lifecycle debugging blind (round 4 §3.1).
                if let Err(err) = this.update(cx, |m, _| m.handle_leaf_request(req)) {
                    tracing::warn!(error = %err, "leaf pump update failed (entity released?)");
                }
            }
            tracing::error!("leaf pump exited (channel closed)");
        });
        Self {
            client,
            sessions: HashMap::new(),
            streams: HashMap::new(),
            info_fetches: HashMap::new(),
            page_fetches: HashMap::new(),
            leaf_tx,
            create_callbacks: HashMap::new(),
            list_fetches: HashMap::new(),
            thread_list: Vec::new(),
            known_projects: Vec::new(),
            models: Vec::new(),
            commands: serde_json::json!([]),
            ready_epoch: None,
            models_retries: 0,
            models_retry_inflight: false,
            _pump,
            _leaf_pump,
            focused: None,
        }
    }

    fn handle_leaf_request(&mut self, req: LeafRequest) {
        match req {
            // Cross-domain #5: the leaf's materialization edge asks for a
            // list refetch — the server self-holds the rescan in its
            // ListThreads answer.
            LeafRequest::RefreshList => self.fetch_thread_list(),
            LeafRequest::ConversationInfo { id, session_id } => {
                let call = ClientCall::GetConversationInfo {
                    session_id: session_id.clone(),
                };
                self.info_fetches.insert(id.clone(), session_id);
                self.client
                    .conn()
                    .send_to_server(FromClient::Request { id, call });
            }
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
                self.client
                    .conn()
                    .send_to_server(FromClient::Request { id, call });
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
        self.streams
            .insert(stream_id.clone(), session_id.to_string());
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
    /// for its session) and then feed the multiplexer's own list/registry
    /// state (U2).
    fn route(&mut self, msg: FromServer, cx: &mut Context<Self>) {
        // §D.5 host events are global: fan out to all leaves, then consume
        // them here — the multiplexer IS the gateway client, so the list and
        // registry channels (§D.5 `ThreadsUpdated`/`Models`/`Commands`, the
        // `Ready` pull, the `SessionStatus` row deltas) live on it (U2).
        // U3/GW5: the leaves' client-owned mirrors stay the per-session
        // consumer — the former thread-store mirror block here duplicated
        // the server pump's own store writes (single-writer, §F.2).
        if let FromServer::Host { host } = &msg {
            for handle in self.sessions.values() {
                let m = FromServer::Host { host: host.clone() };
                handle.update(cx, |h, cx| h.apply_from_server(m, cx));
            }
            self.apply_host(host, cx);
            return;
        }
        let sid = match &msg {
            FromServer::Notification { note } => {
                // C4a: the v1 `SessionCreated` note is reconciliation-only
                // — the host mirror owns the leaf creation + the first
                // follow open (see `apply_host`). The note still fans out
                // to the leaf below (its store-id bind is idempotent with
                // the leaf's host normalization — the GW1 dual-track
                // contract) until C4b retires the note arms server-side.
                let sid = note.session_id().map(str::to_string);
                if let Some(sid) = sid.as_ref()
                    && matches!(note, manox_protocol::ServerNote::SessionCreated { .. })
                    && !self.has_follow(sid)
                {
                    tracing::debug!(
                        session = %sid,
                        "v1 SessionCreated note (the host mirror is authoritative)"
                    );
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
                                CreateSessionDone::Created {
                                    session_id: sid,
                                    handle,
                                }
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
                if let Some(session_id) = self.info_fetches.remove(id) {
                    let outcome = outcome.clone();
                    if let Some(handle) = self.sessions.get(&session_id).cloned() {
                        handle.update(cx, |h, cx| {
                            h.apply_conversation_info_response(id.clone(), outcome, cx)
                        });
                    }
                    return;
                }
                if let Some(session_id) = self.page_fetches.remove(id) {
                    let outcome = outcome.clone();
                    if let Some(handle) = self.sessions.get(&session_id).cloned() {
                        handle.update(cx, |h, cx| h.apply_page_response(id.clone(), outcome, cx));
                    }
                }
                if let Some(kind) = self.list_fetches.remove(id) {
                    // U2: a List response carries the same snapshot the GW1
                    // Host mirror delivered to the requester — both feed the
                    // same state, so mirror + pull are idempotent.
                    match outcome {
                        Ok(value) => self.apply_list_response(kind, value.clone(), cx),
                        Err(e) => tracing::warn!(error = %e, "list fetch failed"),
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
        // On-demand leaf: the create path opens the follow stream the moment
        // `SessionCreated` lands — BEFORE the workspace's attach callback
        // (deferred a tick out of this update borrow) calls `open_or_create`.
        // The opening Snapshot can therefore race ahead of the leaf; dropping
        // it here dead-airs the fold forever (no frame ever reaches it, so it
        // cannot even Resync). Creating the leaf on demand is always safe: it
        // is a plain store, and the later `open_or_create` reuses it via
        // `ensure_leaf`.
        let handle = match self.sessions.get(&sid).cloned() {
            Some(handle) => handle,
            None => self.ensure_leaf(&sid, cx),
        };
        handle.update(cx, |h, cx| h.apply_from_server(msg, cx));
    }

    // ── U2: the list / registry state home ────────────────────────────────
    //
    // The multiplexer is the gateway client, so the §D.5 list channels live
    // here and the views read them off it — never off the kernel store. The
    // state is fed by three idempotent sources: the `Ready` first pull, the
    // Host mirrors (`ThreadsUpdated` / `Models` / `Commands`), and the
    // MsgId-correlated List responses (GW1 dual-emits mirror + response with
    // the same snapshot, so whichever lands last replaces with equal value).

    /// The authoritative threads-list rows: wire `ThreadListItem`s with the
    /// §D.5 `SessionStatus` deltas merged in (sidebar row source, U2).
    pub fn thread_list(&self) -> &[ThreadListItem] {
        &self.thread_list
    }

    /// The model-registry snapshot (selector / cascade display source, U2).
    pub fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    /// The slash-command / skill registry snapshot (wire JSON array of
    /// `{name, description, kind, argument_hint, i18n_key?}` entries, U2).
    pub fn commands(&self) -> &serde_json::Value {
        &self.commands
    }

    /// The epoch the server's `Ready` carried (C1).
    /// The known-projects registry snapshot (U2 cross-domain #1).
    pub fn known_projects(&self) -> &[String] {
        &self.known_projects
    }

    pub fn ready_epoch(&self) -> Option<u32> {
        self.ready_epoch
    }

    /// Fire the three §D.2 list calls (the `Ready` first pull); responses
    /// correlate by `MsgId` like the leaf page fetches.
    fn fetch_lists(&mut self) {
        self.fetch(ListFetch::Threads, ClientCall::ListThreads);
        self.fetch(ListFetch::Models, ClientCall::ListModels);
        self.fetch(ListFetch::Commands, ClientCall::ListCommands);
    }

    /// Re-pull the threads list through the gateway (the workspace's
    /// store-event pump calls this after the in-process rescan lands; the
    /// server snapshot then reflects the fresh metadata).
    pub fn fetch_thread_list(&mut self) {
        self.fetch(ListFetch::Threads, ClientCall::ListThreads);
    }

    /// Re-pull the model registry through the gateway — the picker's
    /// open-time refresh (a settings-side provider reload has no server push
    /// yet, so the open re-pulls) and the boot-race retry below.
    pub fn fetch_models(&mut self) {
        self.fetch(ListFetch::Models, ClientCall::ListModels);
    }

    fn fetch(&mut self, kind: ListFetch, call: ClientCall) {
        let id = self.client.send_call(call);
        self.list_fetches.insert(id, kind);
    }

    /// Store a model snapshot. An EMPTY one schedules a bounded refetch: the
    /// `Ready` first pull can outrun the server's background provider
    /// registration (the §D.5 "Models pushed on provider reload" broadcast is
    /// a cross-domain ask), and the pre-U2 selector self-healed by reading
    /// the registry at render time. A non-empty snapshot resets the budget.
    fn set_models(&mut self, models: Vec<ModelInfo>, cx: &mut Context<Self>) {
        self.models = models;
        if !self.models.is_empty() {
            self.models_retries = 0;
            cx.notify();
            return;
        }
        if self.models_retries < MODELS_EMPTY_RETRY_MAX && !self.models_retry_inflight {
            self.models_retries += 1;
            self.models_retry_inflight = true;
            // Detached with a weak entity: a dropped multiplexer no-ops the
            // wakeup (the retry is a bounded convenience, not a lifecycle
            // obligation).
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(MODELS_EMPTY_RETRY_DELAY)
                    .await;
                let _ = this.update(cx, |m, _| {
                    m.models_retry_inflight = false;
                    m.fetch_models();
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// Consume one §D.5 host frame for the multiplexer's own state, after
    /// the leaf fan-out. C4a: the HOST frames are the authority face for
    /// the per-session control events — `SessionCreated` owns the leaf
    /// creation + the `has_follow`-guarded follow open here, and the leaves
    /// normalize `SessionCreated`/`SessionDisposed`/`Error` host frames
    /// into their store/emit path. The v1 note arms are reconciliation-only
    /// (idempotent under the GW1 dual-emit) and retire with C4b.
    fn apply_host(&mut self, host: &HostEvent, cx: &mut Context<Self>) {
        match host {
            // U2 cross-domain #1: the registry snapshot replaces the
            // workspace's out-of-band decoration push for grouping.
            HostEvent::Projects { known } => {
                self.known_projects = known.clone();
                cx.notify();
            }
            HostEvent::Ready { epoch } => {
                self.ready_epoch = Some(*epoch);
                if *epoch == PROTOCOL_EPOCH {
                    self.fetch_lists();
                } else {
                    // C1: a server on another epoch speaks a vocabulary this
                    // client cannot trust — never pull lists from it.
                    tracing::error!(
                        epoch,
                        expected = PROTOCOL_EPOCH,
                        "protocol epoch mismatch — skipping the list first pull"
                    );
                }
                cx.notify();
            }
            HostEvent::Models { models } => {
                self.set_models(models.clone(), cx);
            }
            HostEvent::Commands { commands } => {
                self.commands = commands.clone();
                cx.notify();
            }
            HostEvent::ThreadsUpdated { threads } => {
                self.set_threads(threads.clone());
                cx.notify();
            }
            HostEvent::SessionStatus {
                session_id,
                running,
                errored,
                unread,
                pending_auth,
                pending_plan,
                background_work,
            } => {
                if self.mirror_status(
                    session_id,
                    *running,
                    *errored,
                    *unread,
                    *pending_auth,
                    *pending_plan,
                    *background_work,
                ) {
                    cx.notify();
                }
            }
            HostEvent::SessionCreated { session_id, .. } => {
                // C4a authority: the leaf creation + the first follow open
                // ride the host frame (the create path —
                // `open_or_create(reopen=false)` — pre-creates nothing, so
                // the on-demand leaf lands here; the `has_follow` guard
                // keeps the open single when the v2 create Response path
                // raced ahead). The leaf must exist BEFORE the follow
                // opens (the round-5 warn: `open_follow: no leaf
                // registered` bailed and the stream only survived via the
                // receipt arm's re-open).
                if !self.has_follow(session_id) {
                    let stream_id = StreamId::new(uuid::Uuid::new_v4().to_string());
                    self.ensure_leaf(session_id, cx);
                    self.open_follow(session_id, stream_id);
                }
                if !self.thread_list.iter().any(|r| r.id == *session_id) {
                    tracing::debug!(
                        session = %session_id,
                        "host SessionCreated ahead of the list snapshot"
                    );
                }
                cx.notify();
            }
            HostEvent::SessionDisposed { session_id } => {
                // C4a: the leaf normalizes the disposal (store/emit); the
                // v1 note was already a desktop no-op — nothing to inherit.
                tracing::debug!(
                    session = %session_id,
                    "host SessionDisposed (the leaf normalizes; C4b retires the note)"
                );
            }
            HostEvent::Error {
                session_id,
                message,
            } => {
                // C4a: a scoped error rides the leaf normalization (the
                // leaf emits it as the ThreadEvent the workspace surfaces);
                // a connection-scoped error (session_id None) has no leaf —
                // logged here, matching the v1 route's drop.
                tracing::debug!(
                    session = ?session_id,
                    error = %message,
                    "host Error (the leaf normalization is authoritative)"
                );
            }
        }
    }

    /// Fill one list state from its correlated `Response` payload.
    fn apply_list_response(
        &mut self,
        kind: ListFetch,
        value: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        match kind {
            ListFetch::Threads => match serde_json::from_value::<Vec<ThreadListItem>>(value) {
                Ok(threads) => self.set_threads(threads),
                Err(e) => {
                    tracing::warn!(error = %e, "ListThreads response did not parse");
                    return;
                }
            },
            ListFetch::Models => match serde_json::from_value::<Vec<ModelInfo>>(value) {
                Ok(models) => {
                    self.set_models(models, cx);
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "ListModels response did not parse");
                    return;
                }
            },
            // The commands snapshot is opaque wire JSON (the popover reads
            // name/description/kind/i18n_key off the entries).
            ListFetch::Commands => self.commands = value,
        }
        cx.notify();
    }

    /// Replace the rows from a list snapshot, preserving the client-owned
    /// unread flags of rows that persist across the fold (GW5: the wire
    /// field is the deprecated constant-false, so the monotonic mirror must
    /// survive the refresh — the deleted webui host's foldThreads had the same
    /// property).
    fn set_threads(&mut self, mut threads: Vec<ThreadListItem>) {
        for row in &mut threads {
            if self
                .thread_list
                .iter()
                .any(|prev| prev.id == row.id && prev.unread)
            {
                row.unread = true;
            }
        }
        self.thread_list = threads;
    }

    /// §D.5 monotonic mirror rules onto the list rows (the deleted webui
    /// `mirrorSessionStatus` parity): `running` / the pending flags are
    /// latest-wins, `errored` sets on true edges (only a list snapshot
    /// clears it), `unread` only rises for a non-focused session and clears
    /// locally on focus. Persisted columns (title/pin/archive/model) belong
    /// to the snapshots and are never touched here. Returns whether any row
    /// changed.
    #[allow(clippy::too_many_arguments)]
    fn mirror_status(
        &mut self,
        session_id: &str,
        running: Option<bool>,
        errored: Option<bool>,
        unread: Option<bool>,
        pending_auth: Option<bool>,
        pending_plan: Option<bool>,
        background_work: Option<bool>,
    ) -> bool {
        let focused = self.focused.as_deref() == Some(session_id);
        let mut changed = false;
        for row in &mut self.thread_list {
            if row.id != session_id {
                continue;
            }
            if let Some(v) = running
                && row.running != v
            {
                row.running = v;
                changed = true;
            }
            if errored == Some(true) && !row.errored {
                row.errored = true;
                changed = true;
            }
            if unread == Some(true) && !focused && !row.unread {
                row.unread = true;
                changed = true;
            }
            if let Some(v) = pending_auth
                && row.pending_auth != v
            {
                row.pending_auth = v;
                changed = true;
            }
            if let Some(v) = pending_plan
                && row.pending_plan != v
            {
                row.pending_plan = v;
                changed = true;
            }
            if let Some(v) = background_work
                && row.background_work != v
            {
                row.background_work = v;
                changed = true;
            }
        }
        changed
    }

    /// Open (reopen) or create a session on the shared connection and bind a
    /// leaf handle for it — an existing leaf is REUSED (its window,
    /// projections and in-flight fold survive; a double `StreamOpen` would
    /// otherwise clobber the fold and double-route frames). `reopen = true`
    /// rebinds an existing thread via `OpenSession` (history replays as the
    /// follow-stream `Snapshot` — T10c retired the v1 note replay); `false`
    /// declares a fresh one via the compat `CreateSession` note (the server
    /// answers `SessionCreated`, which opens the follow stream — see
    /// `route`).
    pub fn open_or_create(
        &mut self,
        session_id: &str,
        cwd: &str,
        reopen: bool,
        cx: &mut Context<Self>,
    ) -> Entity<ClientStoreHandle> {
        let handle = self.ensure_leaf(session_id, cx);
        if reopen {
            self.client.send_call(ClientCall::OpenSession {
                session_id: session_id.into(),
            });
            // A live follow (e.g. opened by the create intent) survives the
            // re-open; only bind a new one when none is running.
            if !self.has_follow(session_id) {
                let stream_id = StreamId::new(uuid::Uuid::new_v4().to_string());
                self.open_follow(session_id, stream_id);
            }
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
        // A leaf created while its session is focused starts active (the
        // attach flow can create the leaf after set_focused).
        if self.focused.as_deref() == Some(session_id) {
            handle.update(cx, |h, cx| {
                h.set_active(true, cx);
            });
        }
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
        on_done: CreateCallback,
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

    /// Client-side focus transition (§F.2/GW5): the newly attached
    /// session's leaf goes active (clearing its monotonic unread/errored
    /// mirrors); the previously attached one goes inert. Selection is
    /// client-owned — the server-side focus mirror is retired. U2: the
    /// list row's client-owned unread flag clears on the same transition
    /// (the leaf mirror and the row mirror obey one focus owner).
    pub fn set_focused(&mut self, session_id: Option<&str>, cx: &mut Context<Self>) {
        if self.focused.as_deref() == session_id {
            return;
        }
        if let Some(prev) = self.focused.take()
            && let Some(leaf) = self.sessions.get(&prev)
        {
            leaf.update(cx, |h, cx| {
                h.set_active(false, cx);
            });
        }
        self.focused = session_id.map(str::to_string);
        if let Some(sid) = session_id {
            if let Some(leaf) = self.sessions.get(sid) {
                leaf.update(cx, |h, cx| {
                    h.set_active(true, cx);
                });
            }
            let mut cleared = false;
            for row in &mut self.thread_list {
                if row.id == sid && row.unread {
                    row.unread = false;
                    cleared = true;
                }
            }
            if cleared {
                cx.notify();
            }
        }
    }

    /// The client-owned unread mirrors of every live leaf (GW5): the
    /// sidebar badge source — rows prefer these over the list summary's
    /// flag, which the server-side mirror retirement empties.
    pub fn unread_map(&self, cx: &App) -> HashMap<String, bool> {
        self.sessions
            .iter()
            .map(|(sid, leaf)| (sid.clone(), leaf.read(cx).store.unread))
            .collect()
    }

    /// Raise a parked session's client-owned unread mirror from local
    /// knowledge (GW5): facts the server deltas do not carry — a parked
    /// error or a background-task update — light the badge through the
    /// leaf. The leaf's active gate suppresses the focused session.
    pub fn note_unread(&mut self, session_id: &str, cx: &mut Context<Self>) {
        if let Some(leaf) = self.sessions.get(session_id) {
            leaf.update(cx, |h, cx| h.note_local_unread(cx));
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use manox_protocol::in_process_pair;
    use manox_session_core::agent_client::AgentClient;

    /// A multiplexer backed by a raw connection pair so a test can inject
    /// `FromServer` frames from the server side without a live AgentServer
    /// (the client_store_handle test module's `test_mux` idiom).
    fn test_mux(
        cx: &mut TestAppContext,
    ) -> (
        Entity<SessionMultiplexer>,
        manox_protocol::InProcessConnection,
    ) {
        let (client_conn, server_conn) = in_process_pair();
        let client = Arc::new(AgentClient::from_conn(client_conn));
        let mux = cx.new(|cx| SessionMultiplexer::with_client(client, cx));
        (mux, server_conn)
    }

    /// Cross-domain #5: the leaf's RefreshList ask becomes a wire
    /// `ListThreads` fetch — the kernel rescan trigger it replaces is
    /// retired; the server self-holds the scan in the answer.
    #[gpui::test]
    fn leaf_refresh_list_request_pulls_the_wire_list(cx: &mut TestAppContext) {
        let (mux, server_conn) = test_mux(cx);
        let tx = mux.update(cx, |m, _| m.leaf_tx.clone());
        tx.try_send(crate::client_store_handle::LeafRequest::RefreshList)
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let calls = drain_calls(&server_conn);
            if calls
                .iter()
                .any(|(_, c)| matches!(c, manox_protocol::ClientCall::ListThreads))
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the RefreshList ask never became a ListThreads fetch"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// U2 cross-domain #1: the Projects host mirror fills the
    /// multiplexer's known-projects state (the sidebar grouping's wire
    /// source — the workspace decoration push retires against it).
    #[gpui::test]
    fn projects_host_frame_fills_the_known_projects_state(cx: &mut TestAppContext) {
        let (mux, server_conn) = test_mux(cx);
        server_conn.send_to_client(manox_protocol::FromServer::Host {
            host: manox_protocol::stream::HostEvent::Projects {
                known: vec!["/p/a".into(), "/p/b".into()],
            },
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            let known = mux.read_with(cx, |m, _| m.known_projects().to_vec());
            if known == vec!["/p/a".to_string(), "/p/b".to_string()] {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the Projects mirror never landed: {known:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn wire_row(id: &str) -> ThreadListItem {
        ThreadListItem {
            id: id.into(),
            title: format!("row {id}"),
            updated_at: 100,
            running: false,
            unread: false,
            errored: false,
            pending_auth: false,
            pending_plan: false,
            background_work: false,
            model_id: "m".into(),
            pinned: false,
            archived: false,
            parent_id: None,
            depth: 0,
            project: Some("/p/wire".into()),
            tag: None,
            approval_mode: Some(0),
        }
    }

    fn wire_model(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: format!("Model {id}"),
            provider: "prov-a".into(),
            provider_name: Some("Provider A".into()),
            api: "anthropic".into(),
            context_window: 200_000,
            max_tokens: Some(8_192),
            config_id: Some(format!("cfg-{id}")),
            agents: None,
        }
    }

    /// Drain the server side of the pair and collect the `ClientCall`s.
    fn drain_calls(conn: &manox_protocol::InProcessConnection) -> Vec<(MsgId, ClientCall)> {
        let rx = conn.client_rx();
        let mut out = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            if let FromClient::Request { id, call } = msg {
                out.push((id, call));
            }
        }
        out
    }

    /// U2/C1: the handshake's `HostEvent::Ready` with the expected epoch
    /// fires the three-call list first pull (`ListThreads` / `ListModels` /
    /// `ListCommands`) and records the epoch.
    #[gpui::test]
    async fn ready_host_frame_triggers_the_initial_list_pulls(cx: &mut TestAppContext) {
        let (mux, server_conn) = test_mux(cx);
        server_conn.send_to_client(FromServer::Host {
            host: HostEvent::Ready {
                epoch: PROTOCOL_EPOCH,
            },
        });
        cx.run_until_parked();
        assert_eq!(
            mux.read_with(cx, |m, _| m.ready_epoch()),
            Some(PROTOCOL_EPOCH)
        );
        let calls = drain_calls(&server_conn);
        assert!(
            calls.iter().any(|(_, c)| *c == ClientCall::ListThreads),
            "Ready must pull the threads list"
        );
        assert!(
            calls.iter().any(|(_, c)| *c == ClientCall::ListModels),
            "Ready must pull the model registry"
        );
        assert!(
            calls.iter().any(|(_, c)| *c == ClientCall::ListCommands),
            "Ready must pull the command registry"
        );
    }

    /// C1: an epoch the client does not speak records but never pulls —
    /// list snapshots from a mismatched server are not trusted.
    #[gpui::test]
    async fn ready_epoch_mismatch_records_but_never_pulls(cx: &mut TestAppContext) {
        let (mux, server_conn) = test_mux(cx);
        server_conn.send_to_client(FromServer::Host {
            host: HostEvent::Ready {
                epoch: PROTOCOL_EPOCH + 1,
            },
        });
        cx.run_until_parked();
        assert_eq!(
            mux.read_with(cx, |m, _| m.ready_epoch()),
            Some(PROTOCOL_EPOCH + 1)
        );
        let calls = drain_calls(&server_conn);
        assert!(
            calls.is_empty(),
            "an epoch mismatch must not fire any list pull, got {calls:?}"
        );
    }

    /// U2: the §D.5 host mirrors replace the list/registry state (the
    /// surfaces read the multiplexer, never the kernel store).
    #[gpui::test]
    async fn host_list_mirrors_replace_multiplexer_state(cx: &mut TestAppContext) {
        let (mux, server_conn) = test_mux(cx);
        let rows = vec![wire_row("t1"), wire_row("t2")];
        let models = vec![wire_model("m1")];
        let commands = serde_json::json!([{"name": "compact", "kind": "command"}]);
        server_conn.send_to_client(FromServer::Host {
            host: HostEvent::ThreadsUpdated {
                threads: rows.clone(),
            },
        });
        server_conn.send_to_client(FromServer::Host {
            host: HostEvent::Models {
                models: models.clone(),
            },
        });
        server_conn.send_to_client(FromServer::Host {
            host: HostEvent::Commands {
                commands: commands.clone(),
            },
        });
        cx.run_until_parked();
        mux.read_with(cx, |m, _| {
            assert_eq!(m.thread_list(), rows.as_slice());
            assert_eq!(m.models(), models.as_slice());
            assert_eq!(m.commands(), &commands);
        });
    }

    /// U2: List responses correlate by `MsgId` (the `info_fetches` /
    /// `page_fetches` pattern) and fill the same state the mirrors feed; a
    /// failed response leaves the state untouched.
    #[gpui::test]
    async fn list_responses_fill_state_by_msgid(cx: &mut TestAppContext) {
        let (mux, server_conn) = test_mux(cx);
        mux.update(cx, |m, _| m.fetch_thread_list());
        let calls = drain_calls(&server_conn);
        let (id, call) = calls.into_iter().next().expect("the pull reached the wire");
        assert_eq!(call, ClientCall::ListThreads);
        let rows = vec![wire_row("t9")];
        server_conn.send_to_client(FromServer::Response {
            id: id.clone(),
            outcome: Ok(serde_json::to_value(&rows).unwrap()),
        });
        cx.run_until_parked();
        mux.read_with(cx, |m, _| assert_eq!(m.thread_list(), rows.as_slice()));

        // A garbage payload (right id, wrong shape) warns and keeps the state.
        mux.update(cx, |m, _| m.fetch_thread_list());
        let calls = drain_calls(&server_conn);
        let (id2, _) = calls.into_iter().next().expect("the second pull");
        server_conn.send_to_client(FromServer::Response {
            id: id2,
            outcome: Ok(serde_json::json!({"not": "a list"})),
        });
        cx.run_until_parked();
        mux.read_with(cx, |m, _| {
            assert_eq!(
                m.thread_list(),
                rows.as_slice(),
                "an unparsable response must not clobber the state"
            )
        });

        // An Err response is swallowed with a warn (no panic, state kept).
        mux.update(cx, |m, _| m.fetch_thread_list());
        let calls = drain_calls(&server_conn);
        let (id3, _) = calls.into_iter().next().expect("the third pull");
        server_conn.send_to_client(FromServer::Response {
            id: id3,
            outcome: Err(RpcError::new(-1, "boom")),
        });
        cx.run_until_parked();
        mux.read_with(cx, |m, _| assert_eq!(m.thread_list(), rows.as_slice()));
    }

    /// §D.5 monotonic mirror rules onto the rows (the deleted webui
    /// `mirrorSessionStatus` parity): running/pending latest-wins, errored
    /// edge-sets and only a snapshot clears it, unread is client-owned —
    /// it rises for a non-focused session, survives list snapshots, and
    /// clears on the focus transition.
    #[gpui::test]
    async fn session_status_deltas_mirror_into_the_rows(cx: &mut TestAppContext) {
        let (mux, server_conn) = test_mux(cx);
        let rows = vec![wire_row("t1"), wire_row("t2")];
        server_conn.send_to_client(FromServer::Host {
            host: HostEvent::ThreadsUpdated {
                threads: rows.clone(),
            },
        });
        cx.run_until_parked();

        let delta = |host: HostEvent| FromServer::Host { host };
        server_conn.send_to_client(delta(HostEvent::SessionStatus {
            session_id: "t1".into(),
            running: Some(true),
            errored: None,
            unread: None,
            pending_auth: Some(true),
            pending_plan: None,
            background_work: None,
        }));
        cx.run_until_parked();
        mux.read_with(cx, |m, _| {
            let t1 = &m.thread_list()[0];
            assert!(t1.running && t1.pending_auth, "latest-wins flags landed");
        });

        // running latest-wins: a false delta flips it back.
        server_conn.send_to_client(delta(HostEvent::SessionStatus {
            session_id: "t1".into(),
            running: Some(false),
            errored: None,
            unread: None,
            pending_auth: None,
            pending_plan: None,
            background_work: None,
        }));
        // errored edge-sets on true…
        server_conn.send_to_client(delta(HostEvent::SessionStatus {
            session_id: "t1".into(),
            running: None,
            errored: Some(true),
            unread: None,
            pending_auth: None,
            pending_plan: None,
            background_work: None,
        }));
        // …and an explicit false delta does NOT clear it (snapshot-only).
        server_conn.send_to_client(delta(HostEvent::SessionStatus {
            session_id: "t1".into(),
            running: None,
            errored: Some(false),
            unread: None,
            pending_auth: None,
            pending_plan: None,
            background_work: None,
        }));
        // unread rises for the non-focused t2.
        server_conn.send_to_client(delta(HostEvent::SessionStatus {
            session_id: "t2".into(),
            running: None,
            errored: None,
            unread: Some(true),
            pending_auth: None,
            pending_plan: None,
            background_work: None,
        }));
        cx.run_until_parked();
        mux.read_with(cx, |m, _| {
            let t1 = &m.thread_list()[0];
            assert!(!t1.running, "running is latest-wins");
            assert!(t1.errored, "errored edge-sets; a false delta never clears");
            let t2 = &m.thread_list()[1];
            assert!(t2.unread, "the non-focused row's unread rises");
        });

        // A list snapshot clears the errored edge but PRESERVES the
        // client-owned unread (the deleted webui host's foldThreads behaved the
        // same: the wire field is
        // the deprecated constant-false).
        server_conn.send_to_client(FromServer::Host {
            host: HostEvent::ThreadsUpdated { threads: rows },
        });
        cx.run_until_parked();
        mux.read_with(cx, |m, _| {
            assert!(!m.thread_list()[0].errored, "the snapshot clears errored");
            assert!(
                m.thread_list()[1].unread,
                "the client-owned unread survives the snapshot"
            );
        });

        // The focus transition clears it locally (GW5 client-owned unread).
        mux.update(cx, |m, cx| m.set_focused(Some("t2"), cx));
        mux.read_with(cx, |m, _| {
            assert!(
                !m.thread_list()[1].unread,
                "focus clears the row's unread mirror"
            );
        });

        // A focused session's unread delta never rises.
        server_conn.send_to_client(delta(HostEvent::SessionStatus {
            session_id: "t2".into(),
            running: None,
            errored: None,
            unread: Some(true),
            pending_auth: None,
            pending_plan: None,
            background_work: None,
        }));
        cx.run_until_parked();
        mux.read_with(cx, |m, _| {
            assert!(
                !m.thread_list()[1].unread,
                "GW5: the focused row never lights up"
            );
        });
    }

    /// The boot registration race: an EMPTY model snapshot schedules a
    /// bounded refetch (the pre-U2 selector self-healed by reading the
    /// registry at render time; the gateway snapshot needs the retry until
    /// the server grows the §D.5 reload push), and a non-empty snapshot
    /// stops the retry chain.
    #[gpui::test]
    async fn empty_models_snapshot_schedules_a_bounded_refetch(cx: &mut TestAppContext) {
        let (mux, server_conn) = test_mux(cx);
        mux.update(cx, |m, _| m.fetch_models());
        let calls = drain_calls(&server_conn);
        let (id, call) = calls.into_iter().next().expect("the pull reached the wire");
        assert_eq!(call, ClientCall::ListModels);
        // The server's registration is still building: an empty snapshot.
        server_conn.send_to_client(FromServer::Response {
            id,
            outcome: Ok(serde_json::json!([])),
        });
        cx.run_until_parked();
        mux.read_with(cx, |m, _| assert!(m.models().is_empty()));
        // The retry fires after the fixed delay.
        cx.executor()
            .advance_clock(MODELS_EMPTY_RETRY_DELAY + std::time::Duration::from_millis(10));
        cx.run_until_parked();
        let calls = drain_calls(&server_conn);
        assert!(
            calls.iter().any(|(_, c)| *c == ClientCall::ListModels),
            "the empty snapshot must schedule a refetch"
        );
        // The retry's non-empty answer ends the chain: no further timer.
        let (id2, _) = calls.into_iter().next().unwrap();
        server_conn.send_to_client(FromServer::Response {
            id: id2,
            outcome: Ok(serde_json::to_value(vec![wire_model("m1")]).unwrap()),
        });
        cx.run_until_parked();
        mux.read_with(cx, |m, _| assert_eq!(m.models().len(), 1));
        cx.executor()
            .advance_clock(MODELS_EMPTY_RETRY_DELAY + std::time::Duration::from_millis(10));
        cx.run_until_parked();
        let calls = drain_calls(&server_conn);
        assert!(
            !calls.iter().any(|(_, c)| *c == ClientCall::ListModels),
            "a non-empty snapshot must reset the retry budget"
        );
    }

    /// C4a authority flip: the `SessionCreated` HOST mirror owns the leaf
    /// creation + the first follow open (the `has_follow` guard keeps it
    /// single); the v1 note is reconciliation-only. Red-green of the flip:
    /// the note alone never opens the follow; the host frame does.
    #[gpui::test]
    fn session_created_host_mirror_owns_the_leaf_and_follow(cx: &mut TestAppContext) {
        let (_mux, server_conn) = test_mux(cx);
        let header = manox_protocol::journal::ThreadHeader {
            id: "s1".into(),
            cwd: "/w".into(),
            parent_session: None,
            metadata: None,
            created_at: "2026-09-05T00:00:00Z".into(),
        };
        // The v1 note alone: reconciliation only — no leaf, no StreamOpen.
        server_conn.send_to_client(FromServer::Notification {
            note: manox_protocol::ServerNote::SessionCreated {
                session_id: "s1".into(),
            },
        });
        // A bounded settle window (a slow pump must not fake the absence).
        // The fan-out's on-demand leaf is generic routing, not the created
        // side effect — only the FOLLOW OPEN flipped to the host frame.
        for _ in 0..20 {
            cx.run_until_parked();
            let rx = server_conn.client_rx();
            while let Ok(msg) = rx.try_recv() {
                if let FromClient::StreamOpen { .. } = msg {
                    panic!("the reconciliation-only note must not open the follow")
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // The host mirror: leaf + follow.
        server_conn.send_to_client(FromServer::Host {
            host: HostEvent::SessionCreated {
                session_id: "s1".into(),
                header,
            },
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut opened = false;
        while !opened {
            cx.run_until_parked();
            let rx = server_conn.client_rx();
            while let Ok(msg) = rx.try_recv() {
                if let FromClient::StreamOpen { stream_kind, .. } = msg
                    && matches!(&stream_kind, manox_protocol::StreamKind::FollowSession { session_id, .. } if session_id == "s1")
                {
                    opened = true;
                }
            }
            if opened {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the host mirror never opened the follow stream"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // Idempotence: a second host frame (the dual-emit tail) never
        // double-opens (the has_follow guard).
        server_conn.send_to_client(FromServer::Host {
            host: HostEvent::SessionCreated {
                session_id: "s1".into(),
                header: manox_protocol::journal::ThreadHeader {
                    id: "s1".into(),
                    cwd: "/w".into(),
                    parent_session: None,
                    metadata: None,
                    created_at: "2026-09-05T00:00:00Z".into(),
                },
            },
        });
        for _ in 0..20 {
            cx.run_until_parked();
            let rx = server_conn.client_rx();
            while let Ok(msg) = rx.try_recv() {
                if let FromClient::StreamOpen { .. } = msg {
                    panic!("the has_follow guard must keep the open single")
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}
