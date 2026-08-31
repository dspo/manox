//! gpui wrapper around `ClientStore` that pumps `FromServer` messages from an
//! `RpcConnection` into the store. γ-1b: the Entity wrapper + pump; γ-2 wires
//! the views read from this.

use agent::ThreadEvent;
use gpui::{Context, EventEmitter, Task};
use manox_protocol::{FromServer, RpcConnection};

use crate::client_store::ClientStore;
use crate::server_note_translate::{server_call_to_thread_event, server_note_to_thread_event};

/// A gpui entity that owns a `ClientStore` and a background pump. The pump
/// reads `FromServer` messages from the connection's server channel, applies
/// each `ServerNote` to the store, re-emits the note as the `ThreadEvent` the
/// workspace's conversation layer consumes, and calls `cx.notify()`.
pub struct ClientStoreHandle {
    pub store: ClientStore,
    _pump: Task<()>,
}

impl EventEmitter<ThreadEvent> for ClientStoreHandle {}

impl ClientStoreHandle {
    /// Create a handle that pumps `FromServer` messages into the store and
    /// re-emits them as `ThreadEvent`s.
    pub fn new(client: manox_protocol::InProcessConnection, cx: &mut Context<Self>) -> Self {
        let server_rx = client.server_rx();
        let _pump = cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            while let Ok(msg) = server_rx.recv().await {
                match msg {
                    FromServer::Notification { note } => {
                        let _ = this.update(cx, |h, cx| {
                            h.store.apply_server_note(&note);
                            if let Some(ev) = server_note_to_thread_event(&note) {
                                cx.emit(ev);
                            }
                            cx.notify();
                        });
                    }
                    FromServer::Request { id, call } => {
                        let _ = this.update(cx, |h, cx| {
                            if let Some(auth_id) = auth_id_of(&call) {
                                h.store.pending_auth.insert(auth_id, id.clone());
                                cx.notify();
                            }
                            if let Some(plan_file) = plan_file_of(&call) {
                                h.store.pending_plan_verdict.insert(plan_file, id.clone());
                                cx.notify();
                            }
                            if let Some(ev) = server_call_to_thread_event(&call) {
                                cx.emit(ev);
                            }
                        });
                    }
                    FromServer::Response { .. } => {}
                }
            }
        });
        Self {
            store: ClientStore::default(),
            _pump,
        }
    }

    /// Create a handle wired to a live `AgentServer` session: the server
    /// accepts the in-process connection and the client immediately declares
    /// itself (handshake + session creation), so the pump starts mirroring
    /// `ServerNote`s for `session_id`. The desktop's transitional read path —
    /// Create a handle wired to a live `AgentServer` session. `reopen` picks
    /// the binding: `false` declares a fresh thread (`CreateSession`), `true`
    /// reopens an existing one (`OpenSession` — idempotent: a live session
    /// replays its snapshots, a persisted one loads from disk; the history
    /// arrives via `ServerNote`s, so the Request response is ignored).
    pub fn for_session(
        server: &manox_session_core::agent_server::AgentServer,
        session_id: &str,
        cwd: &str,
        reopen: bool,
        cx: &mut Context<Self>,
    ) -> (Self, manox_protocol::InProcessConnection) {
        use manox_protocol::*;
        let (client_conn, server_conn) = in_process_pair();
        server.accept(std::sync::Arc::new(server_conn));
        client_conn.send_to_server(FromClient::Request {
            id: MsgId::new("init"),
            call: ClientCall::Initialize(Initialize {
                client_id: format!("desktop-{session_id}"),
                capabilities: vec![
                    handshake::HookKind::Approve,
                    handshake::HookKind::PlanVerdict,
                    handshake::HookKind::AskUserQuestion,
                ],
                sessions: vec![],
            }),
        });
        if reopen {
            client_conn.send_to_server(FromClient::Request {
                id: MsgId::new("open"),
                call: ClientCall::OpenSession {
                    session_id: session_id.into(),
                },
            });
        } else {
            client_conn.send_to_server(FromClient::Notification {
                note: ClientNote::CreateSession {
                    session_id: session_id.into(),
                    cwd: Some(cwd.into()),
                },
            });
        }
        let sender = client_conn.clone();
        let handle = Self::new(client_conn, cx);
        (handle, sender)
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use manox_protocol::{ServerNote, in_process_pair};

    #[gpui::test]
    async fn pump_feeds_server_note_to_store(cx: &mut TestAppContext) {
        let (client_conn, server_conn) = in_process_pair();
        server_conn.send_to_client(FromServer::Notification {
            note: ServerNote::TurnStarted {
                session_id: "s1".into(),
            },
        });
        let entity = cx.new(|cx| ClientStoreHandle::new(client_conn, cx));
        cx.run_until_parked();
        assert!(
            entity.update(cx, |h, _| h.store.running),
            "the pump should have applied TurnStarted → running=true"
        );
    }

    #[gpui::test]
    async fn pump_applies_thread_info(cx: &mut TestAppContext) {
        let (client_conn, server_conn) = in_process_pair();
        let info = manox_protocol::server::ThreadInfoPayload {
            cwd: "/proj".into(),
            project: None,
            display_title: "Test".into(),
            model_id: None,
            model_name: None,
            model: None,
            permission_mode: "workspace-write".into(),
            reasoning_effort: "high".into(),
            pinned: false,
            archived: false,
            depth: 0,
            agent_label: "lead".into(),
            self_author: "lead".into(),
            worktree_active: false,
            worktree_path: None,
            branch: None,
            goal: None,
            goal_elapsed_seconds: None,
            plan_mode: false,
            browser_suites: vec![],
            history_phase: "ready".into(),
            running: false,
            has_interacted: false,
        };
        server_conn.send_to_client(FromServer::Notification {
            note: ServerNote::ThreadInfo {
                session_id: "s1".into(),
                info: std::boxed::Box::new(info),
            },
        });
        let entity = cx.new(|cx| ClientStoreHandle::new(client_conn, cx));
        cx.run_until_parked();
        assert_eq!(
            entity.update(cx, |h, _| h.store.display_title.clone()),
            "Test"
        );
        assert_eq!(entity.update(cx, |h, _| h.store.cwd.clone()), "/proj");
        assert!(!entity.update(cx, |h, _| h.store.running));
    }
}

/// Extract the `auth_id` from an adjudication ServerCall (Approve/AskUser).
fn auth_id_of(call: &manox_protocol::ServerCall) -> Option<String> {
    use manox_protocol::ServerCall;
    match call {
        ServerCall::Approve { auth_id, .. } | ServerCall::AskUserQuestion { auth_id, .. } => {
            Some(auth_id.clone())
        }
        _ => None,
    }
}

/// Extract the `plan_file` from a `ServerCall::PlanVerdict`.
fn plan_file_of(call: &manox_protocol::ServerCall) -> Option<String> {
    match call {
        manox_protocol::ServerCall::PlanVerdict { plan_file, .. } => Some(plan_file.clone()),
        _ => None,
    }
}
