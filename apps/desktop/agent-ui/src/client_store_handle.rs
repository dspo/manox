//! Per-session gpui leaf over a [`ClientStore`]. T-D: the handle no longer
//! owns a connection or pump — it is a pure leaf (store + emitter) fed by the
//! app-level [`crate::multiplexer::SessionMultiplexer`], which demuxes the
//! single shared connection by `session_id` and calls
//! [`ClientStoreHandle::apply_from_server`].

use gpui::{Context, EventEmitter};
use manox_agent::ThreadEvent;
use manox_protocol::FromServer;

use crate::client_store::ClientStore;
use crate::server_note_translate::{server_call_to_thread_event, server_note_to_thread_event};

/// A gpui entity that owns a single session's `ClientStore` and re-emits
/// `ServerNote`s / `ServerCall`s as the `ThreadEvent`s the workspace's
/// conversation layer consumes. The multiplexer is the sole writer.
pub struct ClientStoreHandle {
    pub store: ClientStore,
}

impl EventEmitter<ThreadEvent> for ClientStoreHandle {}

impl ClientStoreHandle {
    /// A fresh leaf for `session_id`. The store's `id` is set when the
    /// `SessionCreated` note (routed by the multiplexer) lands.
    pub fn leaf(_session_id: &str, _cx: &mut Context<Self>) -> Self {
        Self {
            store: ClientStore::default(),
        }
    }

    /// Apply one routed `FromServer` frame: fold the `ServerNote` into the
    /// store (and re-emit a `ThreadEvent`), or record a `ServerCall`'s
    /// `MsgId` against its `auth_id` / `plan_file` for the reply path.
    /// `Response` frames are ignored — readiness derives from
    /// `SessionCreated` / `ThreadHistory` push delivery.
    pub fn apply_from_server(&mut self, msg: FromServer, cx: &mut Context<Self>) {
        match msg {
            FromServer::Notification { note } => {
                self.store.apply_server_note(&note);
                if let Some(ev) = server_note_to_thread_event(&note) {
                    cx.emit(ev);
                }
                cx.notify();
            }
            FromServer::Request { id, call } => {
                if let Some(auth_id) = auth_id_of(&call) {
                    self.store.pending_auth.insert(auth_id, id.clone());
                    cx.notify();
                }
                if let Some(plan_file) = plan_file_of(&call) {
                    self.store
                        .pending_plan_verdict
                        .insert(plan_file, id.clone());
                    cx.notify();
                }
                if let Some(ev) = server_call_to_thread_event(&call) {
                    cx.emit(ev);
                }
            }
            FromServer::Response { .. } => {}
            // T4 envelope compat: the §D.1 stream frames ride the same
            // envelope; the desktop store consumes the v1 note path until
            // T6, so an unknown stream frame is logged and dropped —
            // harmless, per the dual-protocol window (§K.5).
            FromServer::StreamItem { stream_id, .. } => {
                tracing::debug!(stream = %stream_id.0, "agent-ui: ignoring StreamItem (T4 envelope, consumed at T6)");
            }
            FromServer::StreamEnd { stream_id, reason } => {
                tracing::debug!(stream = %stream_id.0, ?reason, "agent-ui: ignoring StreamEnd (T4 envelope, consumed at T6)");
            }
        }
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use gpui::{AppContext as _, TestAppContext};
    use manox_protocol::{RpcConnection as _, ServerNote, in_process_pair};
    use manox_session_core::agent_client::AgentClient;
    use std::sync::Arc;

    use crate::multiplexer::SessionMultiplexer;

    /// A multiplexer backed by a raw connection pair so a test can inject
    /// `FromServer` frames from the server side without a live AgentServer.
    fn test_mux(
        cx: &mut TestAppContext,
    ) -> (
        gpui::Entity<SessionMultiplexer>,
        manox_protocol::InProcessConnection,
    ) {
        let (client_conn, server_conn) = in_process_pair();
        let client = Arc::new(AgentClient::from_conn(client_conn));
        let mux = cx.new(|cx| SessionMultiplexer::with_client(client, cx));
        (mux, server_conn)
    }

    #[gpui::test]
    async fn pump_feeds_server_note_to_store(cx: &mut TestAppContext) {
        let (mux, server_conn) = test_mux(cx);
        let handle = mux.update(cx, |m, cx| m.open_or_create("s1", "/w", false, cx));
        server_conn.send_to_client(FromServer::Notification {
            note: ServerNote::TurnStarted {
                session_id: "s1".into(),
            },
        });
        cx.run_until_parked();
        assert!(
            handle.update(cx, |h, _| h.store.running),
            "the multiplexer should route TurnStarted → running=true"
        );
    }

    #[gpui::test]
    async fn pump_applies_thread_info(cx: &mut TestAppContext) {
        let (mux, server_conn) = test_mux(cx);
        let handle = mux.update(cx, |m, cx| m.open_or_create("s1", "/w", false, cx));
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
            cwd_path: None,
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
        cx.run_until_parked();
        assert_eq!(
            handle.update(cx, |h, _| h.store.display_title.clone()),
            "Test"
        );
        assert_eq!(handle.update(cx, |h, _| h.store.cwd.clone()), "/proj");
        assert!(!handle.update(cx, |h, _| h.store.running));
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
