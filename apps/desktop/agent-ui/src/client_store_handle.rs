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
    Reopen {
        session_id: String,
        stream_id: StreamId,
    },
    /// Fetch a journal page ending at `through_seq` and deliver it back via
    /// [`ClientStoreHandle::apply_page_response`] correlated by `id`.
    PageHistory {
        id: MsgId,
        session_id: String,
        through_seq: u64,
    },
    /// §E.3 Q-face fetch: send `GetConversationInfo` and route the Response
    /// back by MsgId.
    ConversationInfo { id: MsgId, session_id: String },
}

/// A gpui entity that owns a single session's [`ClientStore`], the v2
/// [`JournalFold`] engine, and re-emits the live fold as `ThreadEvent`s. The
/// multiplexer is the sole writer (retained `ServerNote`s) and the sole
/// stream router (v2).
pub struct ClientStoreHandle {
    pub store: ClientStore,
    fold: JournalFold,
    session_id: String,
    outbound: Option<async_channel::Sender<LeafRequest>>,
    /// The MsgId of the `PageHistory` request currently in flight (so a late
    /// reply after a reset is dropped).
    pending_page: Option<MsgId>,
    /// In-flight `GetConversationInfo` correlation id (§E.3 Q face).
    pending_info: Option<MsgId>,
    /// Message-row count at the last info fetch (the committed edge).
    info_committed: usize,
}

impl EventEmitter<ThreadEvent> for ClientStoreHandle {}

impl ClientStoreHandle {
    /// A fresh leaf for `session_id`. The store's `id` is set when the
    /// `SessionCreated` note (routed by the multiplexer) lands.
    pub fn leaf(session_id: &str, _cx: &mut Context<Self>) -> Self {
        Self {
            // The leaf exists for exactly this session (L11: the thread id IS
            // the session id), so the mirror carries it from construction —
            // binding only on the v1 `SessionCreated` note left opened
            // sessions with an empty `store.id`, and every same-frame
            // read-back (attach's sidebar selection) saw "".
            store: ClientStore {
                id: manox_agent::ThreadId(session_id.to_string()),
                ..ClientStore::default()
            },
            fold: JournalFold::new(),
            session_id: session_id.to_string(),
            outbound: None,
            pending_page: None,
            pending_info: None,
            info_committed: 0,
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
                let outs = self.fold.snapshot(snap.cursor, snap.records.clone());
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
        // T10c (§K.5 closeout): the v2 fold is the sole render source.
        // Structural window changes (the snapshot `Replace` and gap-repair
        // `Prepend`) re-arm the conversation rebuild — the §C.2-era
        // authoritative-history-boundary role of the deleted `ThreadHistory`
        // note, now triggered off the §D.1 snapshot itself. Appends emit
        // their per-entry live events below (the same rows feed the store's
        // `display` fold first).
        let structural = matches!(
            &change,
            WindowChange::Replace { .. } | WindowChange::Prepend { .. }
        );
        let live_events = match &change {
            WindowChange::Append(entry) => vec![entry.clone()],
            _ => Vec::new(),
        };
        self.store.apply_window_change(change);
        // §E.3 Q face: a message row landing in the window is the committed
        // edge — refresh the usage panel (per-turn frequency, no debounce
        // needed; the wire usage rows themselves ride the transcript).
        let committed = self
            .store
            .window
            .iter()
            .filter(|e| matches!(&e.event, manox_protocol::JournalWireEvent::Message { .. }))
            .count();
        if committed != self.info_committed {
            self.info_committed = committed;
            if let Some(outbound) = self.outbound.clone() {
                let id = MsgId::new(format!("info-{}-{}", self.session_id, committed));
                self.pending_info = Some(id.clone());
                let _ = outbound.try_send(LeafRequest::ConversationInfo {
                    id,
                    session_id: self.session_id.clone(),
                });
            }
        }
        if self.store.stream_drives_render {
            if structural {
                cx.emit(ThreadEvent::HistoryRestored);
            }
            for entry in live_events {
                if let Some(ev) = journal_translate::thread_event_of(&entry) {
                    cx.emit(ev);
                }
            }
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

    /// Deliver a `GetConversationInfo` response (§E.3) the leaf requested.
    /// Correlated by MsgId; a late/foreign reply is dropped.
    pub fn apply_conversation_info_response(
        &mut self,
        id: MsgId,
        outcome: Result<serde_json::Value, manox_protocol::RpcError>,
        cx: &mut Context<Self>,
    ) {
        if self.pending_info.as_ref() != Some(&id) {
            return;
        }
        self.pending_info = None;
        if let Ok(payload) = outcome {
            self.store.apply_conversation_info(&payload);
            cx.notify();
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext as _, Entity, TestAppContext};
    use manox_protocol::{
        FromClient, RpcConnection as _, ServerNote, in_process_pair, journal::ThreadHeader,
    };
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

    /// §E.3 Q face wiring: a message row landing in the window (the
    /// committed edge) triggers a `GetConversationInfo` request whose
    /// Response fills the usage panel fields (per-model rows + totals).
    #[gpui::test]
    async fn conversation_info_fills_usage_panel_on_committed_edge(cx: &mut TestAppContext) {
        use manox_protocol::journal::JournalWireEntry;
        let (mux, server_conn) = test_mux(cx);
        let handle = mux.update(cx, |m, cx| m.open_or_create("s1", "/w", false, cx));
        cx.run_until_parked();

        // A snapshot carrying one user + one assistant message row (the
        // assistant with a usage payload) lands: committed = 2.
        let entry = |seq: u64, role: &str, usage: Option<manox_protocol::journal::UsagePayload>| {
            let mut value = serde_json::json!({
                "seq": seq,
                "id": format!("e-{seq}"),
                "parentId": if seq == 0 { serde_json::Value::Null } else { serde_json::json!(format!("e-{}", seq - 1)) },
                "timestamp": "2026-09-05T00:00:00Z",
                "type": "message",
                "role": role,
                "content": [],
                "usage": usage,
                "originRpc": serde_json::Value::Null,
            });
            serde_json::from_value::<JournalWireEntry>(value.take()).unwrap()
        };
        let usage = manox_protocol::journal::UsagePayload {
            input: 100,
            output: 40,
            cache_read: 10,
            cache_write: 5,
            reasoning: 0,
        };
        let frame =
            manox_protocol::StreamFrame::Snapshot(manox_protocol::stream::SessionSnapshot {
                session_id: "s1".into(),
                header: ThreadHeader {
                    id: "s1".into(),
                    cwd: "/w".into(),
                    parent_session: None,
                    metadata: None,
                    created_at: "2026-09-05T00:00:00Z".into(),
                },
                cursor: 1,
                records: vec![entry(0, "user", None), entry(1, "assistant", Some(usage))],
                has_more: false,
                projections: Default::default(),
                projections_as_of_seq: 1,
            });
        // Drive the snapshot directly through the leaf (the sibling stream
        // tests' pattern); the outbound LeafRequest channel still runs
        // through the real multiplexer to the raw pair.
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                FromServer::StreamItem {
                    stream_id: StreamId::new("s1"),
                    frame,
                },
                cx,
            )
        });
        cx.run_until_parked();

        // The committed edge fired: the server side of the pair must have
        // received a GetConversationInfo Request for s1.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut info_id = None;
        let rx = server_conn.client_rx();
        while info_id.is_none() && std::time::Instant::now() < deadline {
            while let Ok(msg) = rx.try_recv() {
                if let FromClient::Request { id, call } = msg
                    && matches!(call, manox_protocol::ClientCall::GetConversationInfo { .. })
                {
                    info_id = Some(id);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let info_id = info_id.expect("committed edge must issue GetConversationInfo");

        // The Q-face answer fills the panel fields (mechanical fold).
        server_conn.send_to_client(FromServer::Response {
            id: info_id,
            outcome: Ok(serde_json::json!({
                "cumulativeUsage": {"input": 100, "output": 40, "cacheWrite": 5, "cacheRead": 10},
                "cumulativeCost": 0.42,
                "models": [
                    {"provider": "P", "model": "m", "input": 100, "output": 40,
                     "cacheRead": 10, "cacheWrite": 5}
                ],
                "perModelCost": {"P/m": 0.42},
            })),
        });
        cx.run_until_parked();
        handle.update(cx, |h, _| {
            let st = &h.store;
            let cumulative = st.cumulative_usage.as_ref().expect("cumulative filled");
            assert_eq!((cumulative.input, cumulative.output), (100, 40));
            assert_eq!(st.per_model_usage.len(), 1);
            assert!((st.cumulative_cost - 0.42).abs() < 1e-9);
            assert_eq!(st.per_model_cost.get("P/m"), Some(&0.42));
        });
    }

    #[gpui::test]
    async fn pump_feeds_retained_notes_to_store(cx: &mut TestAppContext) {
        let (mux, server_conn) = test_mux(cx);
        let handle = mux.update(cx, |m, cx| m.open_or_create("s1", "/w", false, cx));
        server_conn.send_to_client(FromServer::Notification {
            note: ServerNote::SessionCreated {
                session_id: "s1".into(),
            },
        });
        cx.run_until_parked();
        assert_eq!(
            handle.update(cx, |h, _| h.store.id.0.clone()),
            "s1",
            "the multiplexer should route SessionCreated → store.id"
        );
        // A global note must not touch the mirror's session fields.
        server_conn.send_to_client(FromServer::Notification {
            note: ServerNote::Ready,
        });
        cx.run_until_parked();
        assert_eq!(handle.update(cx, |h, _| h.store.id.0.clone()), "s1");
    }

    /// T10c e2e restore regression (the gap the v1-fold deletion exposed):
    /// a reopen's follow-stream `Snapshot` carrying multiple history rows
    /// must land in the leaf's `display` fold (the sole render source),
    /// transcribe to `derived_messages`, and re-arm the conversation
    /// rebuild via `HistoryRestored` — the v1 `ThreadHistory` note's old
    /// job. At HEAD this rendered an empty transcript (the restore reads
    /// still pointed at the note-fed `display_entries` field).
    #[gpui::test]
    async fn reopen_snapshot_restores_transcript_and_rearms_rebuild(cx: &mut TestAppContext) {
        use std::cell::Cell;
        let (mux, _server_conn) = test_mux(cx);
        let handle = mux.update(cx, |m, cx| m.open_or_create("s1", "/w", true, cx));
        let rearmed = std::rc::Rc::new(Cell::new(false));
        let sink = rearmed.clone();
        let subscribed = handle.clone();
        let _sub = handle.update(cx, move |_, cx| {
            cx.subscribe(&subscribed, move |_, _, ev: &ThreadEvent, _| {
                if matches!(ev, ThreadEvent::HistoryRestored) {
                    sink.set(true);
                }
            })
        });
        let records = vec![
            wire(
                0,
                JournalWireEvent::Message {
                    role: "user".into(),
                    content: vec![serde_json::json!({"type": "text", "text": "one"})],
                    usage: None,
                    origin_rpc: None,
                },
            ),
            wire(
                1,
                JournalWireEvent::Message {
                    role: "assistant".into(),
                    content: vec![serde_json::json!({"type": "text", "text": "two"})],
                    usage: None,
                    origin_rpc: None,
                },
            ),
            wire(
                2,
                JournalWireEvent::Message {
                    role: "user".into(),
                    content: vec![serde_json::json!({"type": "text", "text": "three"})],
                    usage: None,
                    origin_rpc: Some("rpc-9".into()),
                },
            ),
        ];
        handle.update(cx, |h, cx| {
            h.apply_from_server(item("s1", snapshot("s1", 2, records)), cx)
        });
        cx.run_until_parked();
        handle.update(cx, |h, _| {
            assert_eq!(h.store.window.len(), 3, "the window holds the chain");
            assert_eq!(
                h.store.display.len(),
                3,
                "the restored transcript must be non-empty and complete"
            );
            let msgs = h.store.derived_messages();
            assert_eq!(msgs.len(), 3, "display rows transcribe to messages");
            assert_eq!(
                msgs.iter().map(|m| m.role).collect::<Vec<_>>(),
                vec![
                    manox_agent::language_model::Role::User,
                    manox_agent::language_model::Role::Assistant,
                    manox_agent::language_model::Role::User,
                ]
            );
        });
        assert!(
            rearmed.get(),
            "the snapshot Replace must re-arm the rebuild (HistoryRestored)"
        );
    }

    // ── v2 stream fold / echo / resync / status (spec T6-6) ────────────────

    use manox_protocol::journal::{JournalWireEntry, JournalWireEvent};
    use manox_protocol::stream::{HostEvent, ProjectionsFrame, SessionSnapshot};
    use std::collections::BTreeMap;

    fn wire(seq: u64, event: JournalWireEvent) -> JournalWireEntry {
        JournalWireEntry {
            seq,
            id: format!("w{seq}"),
            parent_id: None,
            timestamp: "2026-09-04T00:00:00.000Z".into(),
            event,
        }
    }

    fn snapshot(session_id: &str, cursor: u64, records: Vec<JournalWireEntry>) -> StreamFrame {
        StreamFrame::Snapshot(SessionSnapshot {
            session_id: session_id.into(),
            header: ThreadHeader {
                id: session_id.into(),
                cwd: "/p".into(),
                parent_session: None,
                metadata: None,
                created_at: "2026-09-04T00:00:00.000Z".into(),
            },
            cursor,
            records,
            has_more: false,
            projections: BTreeMap::new(),
            projections_as_of_seq: cursor,
        })
    }

    fn item(session_id: &str, frame: StreamFrame) -> FromServer {
        FromServer::StreamItem {
            stream_id: StreamId::new(session_id),
            frame,
        }
    }

    #[gpui::test]
    async fn stream_snapshot_entry_projections_fold_store(cx: &mut TestAppContext) {
        let (mux, _conn) = test_mux(cx);
        let handle = mux.update(cx, |m, cx| m.open_or_create("s1", "/p", true, cx));
        let user = wire(
            0,
            JournalWireEvent::Message {
                role: "user".into(),
                content: vec![serde_json::json!({"type": "text", "text": "hi"})],
                usage: None,
                origin_rpc: None,
            },
        );
        handle.update(cx, |h, cx| {
            h.apply_from_server(item("s1", snapshot("s1", 0, vec![user.clone()])), cx)
        });
        cx.run_until_parked();
        // Snapshot folded into window + display.
        handle.update(cx, |h, _| {
            assert_eq!(h.store.window.len(), 1);
            assert_eq!(
                h.store.display.len(),
                1,
                "user message projected to display"
            );
        });
        // A live assistant delta appends to the window (no display item).
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                item(
                    "s1",
                    StreamFrame::Entry {
                        seq: 1,
                        event: JournalWireEvent::AgentTextDelta { s: "yo".into() },
                    },
                ),
                cx,
            )
        });
        handle.update(cx, |h, _| assert_eq!(h.store.window.len(), 2));
        // A Projections frame merges (higher-seq-wins) and materializes.
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                item(
                    "s1",
                    StreamFrame::Projections(ProjectionsFrame {
                        session_id: "s1".into(),
                        as_of_seq: 1,
                        values: BTreeMap::from([(
                            "title".to_string(),
                            serde_json::json!("Renamed"),
                        )]),
                    }),
                ),
                cx,
            )
        });
        handle.update(cx, |h, _| {
            assert_eq!(h.store.display_title, "Renamed");
            assert_eq!(
                h.store.projection("title").unwrap().value,
                serde_json::json!("Renamed")
            );
        });
    }

    #[gpui::test]
    async fn stream_resync_reopens_from_snapshot(cx: &mut TestAppContext) {
        let (mux, _conn) = test_mux(cx);
        let handle = mux.update(cx, |m, cx| m.open_or_create("s1", "/p", true, cx));
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                item(
                    "s1",
                    snapshot("s1", 0, vec![wire(0, JournalWireEvent::TurnStart)]),
                ),
                cx,
            )
        });
        // A gap opens (seq 5 after tail 0) with no page source wired (the raw
        // pair's server never replies), so the leaf stays repairing; instead
        // drive the resync terminal frame directly.
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                FromServer::StreamEnd {
                    stream_id: StreamId::new("s1"),
                    reason: manox_protocol::StreamEndReason::Resync,
                },
                cx,
            )
        });
        cx.run_until_parked();
        // After the re-open, a fresh contiguous snapshot replaces the window
        // seamlessly (cursor >= tail).
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                item(
                    "s1",
                    snapshot(
                        "s1",
                        2,
                        vec![
                            wire(0, JournalWireEvent::TurnStart),
                            wire(
                                1,
                                JournalWireEvent::TurnFinish {
                                    cancelled: false,
                                    failed: false,
                                    stranded_steer_ids: vec![],
                                },
                            ),
                            wire(2, JournalWireEvent::TurnStart),
                        ],
                    ),
                ),
                cx,
            )
        });
        handle.update(cx, |h, _| {
            assert_eq!(
                h.store.window.len(),
                3,
                "re-open converged to the full chain"
            );
        });
    }

    #[gpui::test]
    async fn echo_retires_on_durable_origin_rpc(cx: &mut TestAppContext) {
        let (mux, _conn) = test_mux(cx);
        let handle = mux.update(cx, |m, cx| m.open_or_create("s1", "/p", true, cx));
        handle.update(cx, |h, _| h.store.push_echo("rpc-42", "hello"));
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                item(
                    "s1",
                    snapshot(
                        "s1",
                        0,
                        vec![wire(
                            0,
                            JournalWireEvent::Message {
                                role: "user".into(),
                                content: vec![serde_json::json!({"type": "text", "text": "hello"})],
                                usage: None,
                                origin_rpc: Some("rpc-42".into()),
                            },
                        )],
                    ),
                ),
                cx,
            )
        });
        // Snapshot (Replace) does not run the per-entry echo retirement; only
        // Append does, matching §F.2 (a durable row arriving live). Feed the
        // same row as an Append to exercise the retire.
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                item(
                    "s1",
                    StreamFrame::Entry {
                        seq: 1,
                        event: JournalWireEvent::Message {
                            role: "user".into(),
                            content: vec![serde_json::json!({"type": "text", "text": "hello"})],
                            usage: None,
                            origin_rpc: Some("rpc-42".into()),
                        },
                    },
                ),
                cx,
            )
        });
        handle.update(cx, |h, _| {
            assert!(
                h.store.echo.is_empty(),
                "durable originRpc retired the echo"
            );
        });
    }

    #[gpui::test]
    async fn session_status_mirrors_monotonically(cx: &mut TestAppContext) {
        let (mux, _conn) = test_mux(cx);
        let handle = mux.update(cx, |m, cx| m.open_or_create("s1", "/p", true, cx));
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                FromServer::Host {
                    host: HostEvent::SessionStatus {
                        session_id: "s1".into(),
                        running: None,
                        errored: None,
                        unread: Some(true),
                        pending_auth: None,
                        pending_plan: None,
                        background_work: None,
                    },
                },
                cx,
            )
        });
        handle.update(cx, |h, _| assert!(h.store.unread));
        // A later `unread=false` delta does not clear it (only focus does).
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                FromServer::Host {
                    host: HostEvent::SessionStatus {
                        session_id: "s1".into(),
                        running: Some(true),
                        errored: None,
                        unread: Some(false),
                        pending_auth: None,
                        pending_plan: None,
                        background_work: None,
                    },
                },
                cx,
            )
        });
        handle.update(cx, |h, _| {
            assert!(h.store.unread, "unread survives until focus");
            assert!(h.store.running, "running takes the latest value");
        });
    }
}
