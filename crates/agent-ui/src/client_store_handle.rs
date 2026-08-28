//! gpui wrapper around `ClientStore` that pumps `FromServer` messages from an
//! `RpcConnection` into the store. γ-1b: the Entity wrapper + pump; γ-2 wires
//! the views to read from this instead of `ThreadProxy`.

use gpui::{Context, Task};
use manox_protocol::{FromServer, RpcConnection};

use crate::client_store::ClientStore;

/// A gpui entity that owns a `ClientStore` and a background pump. The pump
/// reads `FromServer::Notification` from the connection's server channel and
/// applies each `ServerNote` to the store, then calls `cx.notify()`.
pub struct ClientStoreHandle {
    pub store: ClientStore,
    _pump: Task<()>,
}

impl ClientStoreHandle {
    /// Create a handle that pumps `FromServer` notifications into the store.
    pub fn new(client: manox_protocol::InProcessConnection, cx: &mut Context<Self>) -> Self {
        let server_rx = client.server_rx();
        let _pump = cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            while let Ok(msg) = server_rx.recv().await {
                if let FromServer::Notification { note } = msg {
                    let _ = this.update(cx, |h, cx| {
                        h.store.apply_server_note(&note);
                        cx.notify();
                    });
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
    /// views read kernel state from the store instead of `ThreadProxy`.
    pub fn for_session(
        server: &manox_session_core::agent_server::AgentServer,
        session_id: &str,
        cwd: &str,
        cx: &mut Context<Self>,
    ) -> Self {
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
        client_conn.send_to_server(FromClient::Notification {
            note: ClientNote::CreateSession {
                session_id: session_id.into(),
                cwd: Some(cwd.into()),
            },
        });
        Self::new(client_conn, cx)
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
