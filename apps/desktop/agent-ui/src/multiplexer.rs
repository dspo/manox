//! Single shared connection multiplexer for the gpui desktop app.
//!
//! One app-level `AgentClient` (client_id `"desktop"`) carries every
//! session over a single in-process connection; a gpui Task pump demuxes
//! incoming `FromServer` by `session_id` to the matching per-session
//! [`ClientStoreHandle`]. Handles are pure leaves (store + emitter) — they
//! no longer own a connection or pump, so the app holds exactly one
//! long-lived connection regardless of how many sessions are open.
//!
//! The pump is a gpui `Task` spawned on `Context<Self>` so every
//! `entity.update` / `cx.notify` runs on the gpui thread — the
//! `assert_correct_thread` constraint (#754). The agent runtime never wakes
//! a gpui task.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{AppContext as _, Context, Entity, Task};

use manox_protocol::handshake::HookKind;
use manox_protocol::transport::RpcConnection as _;
use manox_protocol::{ClientCall, ClientNote, FromClient, FromServer, MsgId, RpcError};
use manox_session_core::agent_client::AgentClient;
use manox_session_core::agent_server::AgentServer;

use crate::client_store_handle::ClientStoreHandle;

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
/// `session_id`. Holds strong references to every live handle so parked
/// (background) sessions keep accumulating state while the foreground is
/// elsewhere.
pub struct SessionMultiplexer {
    client: Arc<AgentClient>,
    sessions: HashMap<String, Entity<ClientStoreHandle>>,
    _pump: Task<()>,
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

    /// Wire a pre-built client (handshake already sent) and spawn the pump.
    /// Production uses [`Self::new`]; tests pass a raw-connection wrapper so
    /// they can inject `FromServer` frames from the server side.
    pub fn with_client(client: Arc<AgentClient>, cx: &mut Context<Self>) -> Self {
        let rx = client.conn().server_rx();
        let _pump = cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            while let Ok(msg) = rx.recv().await {
                let _ = this.update(cx, |m, cx| m.route(msg, cx));
            }
        });
        Self {
            client,
            sessions: HashMap::new(),
            _pump,
        }
    }

    /// Route one `FromServer` to the handle registered for its session
    /// (notifications and ServerCalls both carry `session_id`). Globals
    /// (`Ready`/`Models`/`ThreadsUpdated`/`Commands`/bare-model) and
    /// `Response` frames have no session and are dropped here — the desktop
    /// derives everything from session-scoped push delivery.
    fn route(&mut self, msg: FromServer, cx: &mut Context<Self>) {
        let sid = match &msg {
            FromServer::Notification { note } => note.session_id().map(str::to_string),
            FromServer::Request { call, .. } => Some(call.session_id().to_string()),
            FromServer::Response { .. } => None,
        };
        let Some(sid) = sid else { return };
        let Some(handle) = self.sessions.get(&sid).cloned() else {
            return;
        };
        handle.update(cx, |h, cx| h.apply_from_server(msg, cx));
    }

    /// Open (reopen) or create a session on the shared connection and register
    /// a fresh leaf handle for it. `reopen = true` rebinds an existing thread
    /// via `OpenSession` (history replays as `ServerNote`s); `false` declares
    /// a fresh one via `CreateSession`. The handle is registered before the
    /// request is sent so the pump routes the server's `SessionCreated` /
    /// `ThreadHistory` reply straight to it.
    pub fn open_or_create(
        &mut self,
        session_id: &str,
        cwd: &str,
        reopen: bool,
        cx: &mut Context<Self>,
    ) -> Entity<ClientStoreHandle> {
        let handle = cx.new(|cx| ClientStoreHandle::leaf(session_id, cx));
        self.sessions.insert(session_id.to_string(), handle.clone());
        if reopen {
            self.client.send_call(ClientCall::OpenSession {
                session_id: session_id.into(),
            });
        } else {
            self.client.send_note(ClientNote::CreateSession {
                session_id: session_id.into(),
                cwd: Some(cwd.into()),
            });
        }
        handle
    }

    /// Drop a session from the multiplexer (the server-side owner is released
    /// separately via `DetachSession`). Keeps parked handles addressable.
    pub fn forget(&mut self, session_id: &str) -> Option<Entity<ClientStoreHandle>> {
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
}
