//! Per-session gpui leaf over a [`ClientStore`] + the v2 journal fold.
//!
//! T-D: the handle is a pure leaf (store + emitter) fed by the app-level
//! [`crate::multiplexer::SessionMultiplexer`]. T6 adds the v2 stream side:
//! `StreamItem` frames (Snapshot / Entry / Projections) feed the per-session
//! [`crate::journal_fold::JournalFold`] engine; the committed window changes
//! fold into the store, live deltas re-emit as `ThreadEvent`s, and the
//! optimistic echo retires on the durable row. `StreamEnd{Resync}` (and any
//! engine violation) requests a seamless re-open through the multiplexer
//! [`Self::outbound`] channel; gap-repair `PageHistory` requests ride the same
//! channel and their `Response`s are correlated by MsgId through
//! [`Self::apply_page_response`].

use gpui::{Context, EventEmitter};
use manox_agent::ThreadEvent;
use manox_protocol::{FromServer, MsgId, RpcError, StreamFrame, StreamId};

use crate::client_store::ClientStore;
use crate::journal_fold::{FoldOut, JournalFold, WindowChange};
use crate::journal_translate;
use crate::server_note_translate::{server_call_to_thread_event, server_note_to_thread_event};

/// A signal the leaf asks the multiplexer to carry on the shared connection.
#[derive(Debug, Clone)]
pub enum LeafRequest {
    /// Open (re-open) the follow stream for this session.
    Reopen { session_id: String, stream_id: StreamId },
    /// Fetch a journal page ending at `through_seq` and deliver it back via
    /// [`ClientStoreHandle::apply_page_response`] correlated by `id`.
    PageHistory { id: MsgId, session_id: String, through_seq: u64 },
}

/// A gpui entity that owns a single session's [`ClientStore`], the v2
/// [`JournalFold`] engine, and re-emits the live fold as `ThreadEvent`s. The
/// multiplexer is the sole writer (v1 notes) and the sole stream router (v2).
pub struct ClientStoreHandle {
    pub store: ClientStore,
    fold: JournalFold,
    session_id: String,
    outbound: Option<async_channel::Sender<LeafRequest>>,
    /// The MsgId of the `PageHistory` request currently in flight (so a late
    /// reply after a reset is dropped).
    pending_page: Option<MsgId>,
}

impl EventEmitter<ThreadEvent> for ClientStoreHandle {}

impl ClientStoreHandle {
    /// A fresh leaf for `session_id`. The store's `id` is set when the
    /// `SessionCreated` note (routed by the multiplexer) lands.
    pub fn leaf(session_id: &str, _cx: &mut Context<Self>) -> Self {
        Self {
            store: ClientStore::default(),
            fold: JournalFold::new(),
            session_id: session_id.to_string(),
            outbound: None,
            pending_page: None,
        }
    }

    /// Wire the leaf's outbound channel to the multiplexer (the multiplexer
    /// calls this when it creates the leaf so the v2 re-open / page fetch can
    /// ride the shared connection).
    pub fn set_outbound(&mut self, outbound: async_channel::Sender<LeafRequest>) {
        self.outbound = Some(outbound);
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Apply one routed `FromServer` frame.
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
                self.apply_server_request(id, &call, cx);
            }
            FromServer::Response { .. } => {
                // Responses are correlated at the multiplexer and delivered
                // via `apply_page_response`; a raw Response reaching the leaf
                // means no page was awaited — drop (readiness derives from
                // push delivery, as in v1).
            }
            FromServer::StreamItem { frame, .. } => self.apply_stream_frame(frame, cx),
            FromServer::StreamEnd { reason, .. } => self.apply_stream_end(reason, cx),
            // §D.5 host event routed by session: `SessionStatus` mirrors into
            // the store under the monotonic rules (the multiplexer broadcasts
            // to every leaf, including parked ones).
            FromServer::Host { host } => {
                if let manox_protocol::stream::HostEvent::SessionStatus {
                    session_id,
                    running,
                    errored,
                    unread,
                    pending_auth,
                    pending_plan,
                    background_work,
                } = host
                    && session_id == self.session_id
                {
                    self.store.apply_session_status(
                        running,
                        errored,
                        unread,
                        pending_auth,
                        pending_plan,
                        background_work,
                    );
                    cx.notify();
                }
            }
        }
    }

    /// A v2 follow-stream frame: Snapshot opens/replaces the window; Entry
    /// feeds the live journal; Projections merge the P-face.
    fn apply_stream_frame(&mut self, frame: StreamFrame, cx: &mut Context<Self>) {
        let outs = match frame {
            StreamFrame::Snapshot(snap) => {
                if snap.session_id != self.session_id {
                    return;
                }
                let outs = self
                    .fold
                    .snapshot(snap.cursor, snap.records.clone());
                // The snapshot's full projection baseline (§D.1) seeds the
                // P-face; merge it before folding so materialized fields are
                // current for the rebuild.
                self.store
                    .merge_projection_baseline(&snap.projections, snap.projections_as_of_seq);
                outs
            }
            StreamFrame::Entry { seq, event } => self.fold.entry(seq, event),
            StreamFrame::Projections(frame) => {
                if frame.session_id != self.session_id {
                    return;
                }
                self.store.merge_projections(&frame);
                cx.notify();
                return;
            }
        };
        self.handle_fold_outs(outs, cx);
    }

    fn apply_stream_end(
        &mut self,
        reason: manox_protocol::StreamEndReason,
        cx: &mut Context<Self>,
    ) {
        match reason {
            manox_protocol::StreamEndReason::Resync => self.request_reopen(cx),
            // Cancelled/Closed: the multiplexer is tearing the stream down on
            // our behalf (detach/dispose) — no seamless re-open.
            manox_protocol::StreamEndReason::Cancelled
            | manox_protocol::StreamEndReason::Closed => {}
            manox_protocol::StreamEndReason::Failure { code, message } => {
                tracing::warn!(code = %code, message = %message, "follow stream failed; re-opening");
                self.request_reopen(cx);
            }
        }
    }

    fn handle_fold_outs(&mut self, outs: Vec<FoldOut>, cx: &mut Context<Self>) {
        for out in outs {
            match out {
                FoldOut::Change(change) => self.apply_change(change, cx),
                FoldOut::NeedPage(req) => {
                    let Some(outbound) = self.outbound.clone() else {
                        // No multiplexer wired (unit test path): the window
                        // is already gap-free; drop the request.
                        continue;
                    };
                    let id = MsgId::new(format!("page-{}-{}", self.session_id, req.through_seq));
                    self.pending_page = Some(id.clone());
                    let _ = outbound.try_send(LeafRequest::PageHistory {
                        id,
                        session_id: self.session_id.clone(),
                        through_seq: req.through_seq,
                    });
                }
                FoldOut::Resync => {
                    // Seamless reconnect: re-open the follow stream (the next
                    // frame is a fresh Snapshot at a cursor >= our tail, the
                    // engine keeps the old window until it lands).
                    self.request_reopen(cx);
                }
            }
        }
    }

    fn apply_change(&mut self, change: WindowChange, cx: &mut Context<Self>) {
        // Live rendering still rides the v1 note path during the dual-protocol
        // window (§K.5); the v2 fold additionally emits live events for the
        // rows the notes also carry, but the conversation rebuild is triggered
        // by structural window changes only (Replace/Prepend).
        let live_events = match &change {
            WindowChange::Append(entry) => vec![entry.clone()],
            _ => Vec::new(),
        };
        let structural = self.store.apply_window_change(change);
        for entry in live_events {
            if let Some(ev) = journal_translate::thread_event_of(&entry) {
                cx.emit(ev);
            }
        }
        if structural {
            cx.emit(ThreadEvent::HistoryRestored);
        }
        cx.notify();
    }

    /// Deliver a `PageHistory` response the leaf requested. Correlated by
    /// MsgId; a late/foreign reply is dropped.
    pub fn apply_page_response(
        &mut self,
        id: MsgId,
        outcome: Result<serde_json::Value, RpcError>,
        cx: &mut Context<Self>,
    ) {
        if self.pending_page.as_ref() != Some(&id) {
            return;
        }
        self.pending_page = None;
        let records = match outcome {
            Ok(v) => v
                .get("records")
                .and_then(|r| {
                    serde_json::from_value::<Vec<manox_protocol::JournalWireEntry>>(r.clone()).ok()
                })
                .unwrap_or_default(),
            Err(e) => {
                tracing::warn!(error = %e, "PageHistory failed; re-opening follow stream");
                self.request_reopen(cx);
                return;
            }
        };
        let outs = self.fold.deliver_page(records);
        self.handle_fold_outs(outs, cx);
    }

    fn request_reopen(&mut self, cx: &mut Context<Self>) {
        self.fold.generation();
        let Some(outbound) = self.outbound.clone() else {
            return;
        };
        let stream_id = StreamId::new(uuid::Uuid::new_v4().to_string());
        let _ = outbound.try_send(LeafRequest::Reopen {
            session_id: self.session_id.clone(),
            stream_id,
        });
        cx.notify();
    }

    /// Adjudication `ServerCall`: record the MsgId for the reply path (the
    /// workspace answers against it) and re-emit the card as a `ThreadEvent`.
    /// The waterfall fan-out (spec T6-5): the deterministic MsgId is
    /// `auth_id`/`session_id`, so the SAME auth id can be delivered to this
    /// leaf more than once across re-connections; each delivery is recorded
    /// independently (the last wins for the reply path) and re-emitted so the
    /// card surfaces without depending on the v1 note accumulation.
    fn apply_server_request(
        &mut self,
        id: MsgId,
        call: &manox_protocol::ServerCall,
        cx: &mut Context<Self>,
    ) {
        if let Some(auth_id) = auth_id_of(call) {
            self.store.pending_auth.insert(auth_id, id.clone());
            cx.notify();
        }
        if let Some(plan_file) = plan_file_of(call) {
            self.store
                .pending_plan_verdict
                .insert(plan_file, id.clone());
            cx.notify();
        }
        if let Some(ev) = server_call_to_thread_event(call) {
            cx.emit(ev);
        }
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

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Entity, TestAppContext};
    use manox_protocol::{ServerNote, in_process_pair};
    use manox_session_core::agent_client::AgentClient;
    use std::sync::Arc;

    use crate::multiplexer::SessionMultiplexer;

    /// A multiplexer backed by a raw connection pair so a test can inject
    /// `FromServer` frames from the server side without a live AgentServer.
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
