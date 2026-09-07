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

use gpui::{Context, EventEmitter, Task};
use manox_agent::ThreadEvent;
use manox_protocol::{FromServer, MsgId, RpcError, StreamFrame, StreamId};

use crate::client_store::ClientStore;
use crate::journal_fold::{FoldOut, JournalFold, WindowChange};
use crate::journal_translate;
use crate::server_note_translate::{server_call_to_thread_event, server_note_to_thread_event};

/// §E.3: Q-face fetches are debounced — a dense committed burst (an
/// assistant message plus its tool rows settling within milliseconds)
/// fires one trailing fetch instead of one `GetConversationInfo` per row.
const INFO_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(120);

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
    /// The materialization edge fired once (sidebar refresh, see
    /// [`Self::apply_change`]).
    materialized_notified: bool,
    /// The pending debounced Q-face fetch (§E.3); replacing it cancels the
    /// previous one, coalescing a committed burst into a trailing request.
    info_debounce: Option<Task<()>>,
    /// Client-owned focus (§F.2/GW5): while this leaf's session is the
    /// attached one, an `unread` rise is suppressed — the user is watching
    /// it. Driven by [`Self::set_active`] (the multiplexer owns the
    /// transitions); activation also runs the focus clear.
    active: bool,
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
            materialized_notified: false,
            info_debounce: None,
            active: false,
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
                    // GW5: a focused session never lights up — the unread
                    // rise is suppressed client-side (webui parity: its
                    // mirror gates `unread === true && !active`).
                    let unread = if self.active { None } else { unread };
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
            StreamFrame::Entry {
                seq,
                id,
                parent_id,
                timestamp,
                event,
            } => self.fold.entry(manox_protocol::JournalWireEntry {
                seq,
                id,
                parent_id,
                timestamp,
                event,
            }),
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
        // The session file materializes on the FIRST assistant message (the
        // deferred-first-assistant contract) — before that boundary a fresh
        // thread has no file for the sidebar scan to list, so the submit-time
        // refresh misses it and the running conversation is invisible (the
        // round-11 "this thread never appeared in the list" repro). The first
        // assistant row landing in the window IS the materialization edge:
        // refresh the thread list exactly once per session.
        if !self.materialized_notified
            && live_events.iter().any(|e| {
                matches!(
                    &e.event,
                    manox_protocol::JournalWireEvent::Message { role, .. } if role == "assistant"
                )
            })
        {
            self.materialized_notified = true;
            manox_agent::thread_store::refresh_thread_list();
        }
        // §E.3 Q face: a message row landing in the window is the committed
        // edge — refresh the usage panel (per-turn frequency; the wire usage
        // rows themselves ride the transcript). The counter is maintained
        // incrementally (U7): an Append contributes exactly its own row, so
        // the streaming hot path is O(1) per frame; only structural window
        // changes (snapshot Replace, gap-repair merge, Prepend) recount the
        // fresh window. The former full-window scan ran on every delta
        // frame — O(window) per frame, O(n²) across a session.
        let committed = if structural {
            self.store
                .window
                .iter()
                .filter(|e| matches!(&e.event, manox_protocol::JournalWireEvent::Message { .. }))
                .count()
        } else {
            self.info_committed
                + live_events
                    .iter()
                    .filter(|e| {
                        matches!(&e.event, manox_protocol::JournalWireEvent::Message { .. })
                    })
                    .count()
        };
        if committed != self.info_committed {
            self.info_committed = committed;
            self.schedule_info_fetch(cx);
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

    /// §E.3 debounce: schedule (or replace) the trailing Q-face fetch.
    /// Dropping the previous `Task` cancels it, so a burst of committed
    /// edges coalesces into one request keyed by the latest count. The
    /// timer rides the gpui background executor — never tokio time on the
    /// gpui thread.
    fn schedule_info_fetch(&mut self, cx: &mut Context<Self>) {
        self.info_debounce = Some(cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            cx.background_executor().timer(INFO_DEBOUNCE).await;
            let _ = this.update(cx, |h, _| h.fire_info_fetch());
        }));
    }

    /// Send the Q-face request for the current committed count (the
    /// debounce trailing edge).
    fn fire_info_fetch(&mut self) {
        let Some(outbound) = self.outbound.clone() else {
            return;
        };
        let id = MsgId::new(format!("info-{}-{}", self.session_id, self.info_committed));
        self.pending_info = Some(id.clone());
        let _ = outbound.try_send(LeafRequest::ConversationInfo {
            id,
            session_id: self.session_id.clone(),
        });
    }

    /// Client-side focus transition (§F.2/GW5). Activating clears the
    /// monotonic unread/errored mirrors (the user is now watching); the
    /// return reports whether anything cleared (badge refresh).
    pub fn set_active(&mut self, active: bool, cx: &mut Context<Self>) -> bool {
        self.active = active;
        if active {
            let cleared = self.store.focus_cleared();
            if cleared {
                cx.notify();
            }
            return cleared;
        }
        false
    }

    /// Raise the client-owned unread mirror from local knowledge (GW5):
    /// parked-thread facts the server deltas do not carry (a parked error,
    /// a background-task update). Suppressed while active — the user is
    /// watching, so nothing is "unread".
    pub fn note_local_unread(&mut self, cx: &mut Context<Self>) {
        if self.active {
            return;
        }
        self.store
            .apply_session_status(None, None, Some(true), None, None, None);
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
        // §E.3 debounce: the fetch fires at the trailing edge of the window.
        cx.executor()
            .advance_clock(INFO_DEBOUNCE + std::time::Duration::from_millis(10));
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

    /// U7 (§E.3): the committed-message counter is maintained
    /// incrementally — an Append contributes exactly its own row, and only
    /// structural window changes (snapshot Replace, gap-repair merge)
    /// recount. Behavior is identical to the former full-window scan:
    /// requests fire only on committed edges with `info-<session>-<count>`
    /// ids, non-message appends fire nothing, and the counter equals a
    /// full recount at every boundary.
    #[gpui::test]
    async fn q_face_committed_counter_is_incremental_and_exact(cx: &mut TestAppContext) {
        let (mux, server_conn) = test_mux(cx);
        let handle = mux.update(cx, |m, cx| m.open_or_create("s1", "/w", false, cx));
        cx.run_until_parked();
        let rx = server_conn.client_rx();
        // Drain GetConversationInfo request ids; other frames (the follow
        // StreamOpen) are discarded — the raw pair's server side answers
        // nothing in this test.
        let drain_info = |rx: &async_channel::Receiver<manox_protocol::FromClient>| -> Vec<String> {
            let mut ids = Vec::new();
            while let Ok(m) = rx.try_recv() {
                if let manox_protocol::FromClient::Request { id, call } = m
                    && matches!(call, manox_protocol::ClientCall::GetConversationInfo { .. })
                {
                    ids.push(id.0.clone());
                }
            }
            ids
        };
        let msg_ev = |role: &str| JournalWireEvent::Message {
            role: role.into(),
            content: vec![serde_json::json!({"type": "text", "text": "x"})],
            usage: None,
            origin_rpc: None,
        };

        // Snapshot: two message rows plus one delta → Replace → committed
        // recounts to 2 and fires one request.
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                item(
                    "s1",
                    snapshot(
                        "s1",
                        2,
                        vec![
                            wire(0, msg_ev("user")),
                            wire(1, JournalWireEvent::AgentTextDelta { s: "d".into() }),
                            wire(2, msg_ev("assistant")),
                        ],
                    ),
                ),
                cx,
            )
        });
        cx.run_until_parked();
        cx.executor()
            .advance_clock(INFO_DEBOUNCE + std::time::Duration::from_millis(10));
        cx.run_until_parked();
        assert_eq!(drain_info(&rx), vec!["info-s1-2".to_string()]);

        // Delta and tool appends: window rows land but the committed edge
        // does not move — no request (the hot path the former code rescanned
        // in full on every frame).
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                item(
                    "s1",
                    StreamFrame::Entry {
                        seq: 3,
                        id: "w3".to_string(),
                        parent_id: None,
                        timestamp: String::new(),
                        event: JournalWireEvent::AgentTextDelta { s: "a".into() },
                    },
                ),
                cx,
            )
        });
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                item(
                    "s1",
                    StreamFrame::Entry {
                        seq: 4,
                        id: "w4".to_string(),
                        parent_id: None,
                        timestamp: String::new(),
                        event: JournalWireEvent::ToolCall {
                            call_id: "c1".into(),
                            name: "Bash".into(),
                            title: "t".into(),
                            status: "running".into(),
                            input: serde_json::json!({}),
                        },
                    },
                ),
                cx,
            )
        });
        cx.run_until_parked();
        assert!(
            drain_info(&rx).is_empty(),
            "non-message appends must not fire the Q face"
        );

        // A message append moves the edge to 3 — exactly one request.
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                item(
                    "s1",
                    StreamFrame::Entry {
                        seq: 5,
                        id: "w5".to_string(),
                        parent_id: None,
                        timestamp: String::new(),
                        event: msg_ev("user"),
                    },
                ),
                cx,
            )
        });
        cx.run_until_parked();
        cx.executor()
            .advance_clock(INFO_DEBOUNCE + std::time::Duration::from_millis(10));
        cx.run_until_parked();
        assert_eq!(drain_info(&rx), vec!["info-s1-3".to_string()]);

        // U5: the live frame's durable envelope flows through verbatim —
        // the fold no longer synthesizes `e-{seq}` ids, which drifted from
        // the snapshot records' real uuids at every Replace (usage keys,
        // bubble identity, list keys).
        handle.update(cx, |h, _| {
            let row = h
                .store
                .window
                .iter()
                .find(|e| e.seq == 5)
                .expect("seq 5 lands in the window");
            assert_eq!(row.id, "w5", "the frame's durable id flows through");
        });

        // Structural boundary: a gap (seq 7 after tail 5) buffers the entry
        // and requests the missing page; answering it merges into a Replace
        // whose recount must be exact (messages 0, 2, 5, 6, 7 → 5).
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                item(
                    "s1",
                    StreamFrame::Entry {
                        seq: 7,
                        id: "w7".to_string(),
                        parent_id: None,
                        timestamp: String::new(),
                        event: msg_ev("assistant"),
                    },
                ),
                cx,
            )
        });
        cx.run_until_parked();
        let mut page_id: Option<MsgId> = None;
        while let Ok(m) = rx.try_recv() {
            if let manox_protocol::FromClient::Request { id, call } = m
                && matches!(call, manox_protocol::ClientCall::PageHistory { .. })
            {
                page_id = Some(id);
            }
        }
        let page_id = page_id.expect("the gap must request a page");
        // The repair page follows the production PageHistory contract: an
        // unbounded read from the chain start through `through_seq` (the
        // offending entry's own seq) — the engine publishes the repair page
        // AS the whole window ("exactly the repair page plus the queued
        // entries", journal_stream replaceThrough), so a bounded page would
        // truncate the transcript. The queued seq-7 entry merges as a stale
        // duplicate (the page already contains it).
        let full_chain: Vec<serde_json::Value> = vec![
            serde_json::to_value(wire(0, msg_ev("user"))).unwrap(),
            serde_json::to_value(wire(1, JournalWireEvent::AgentTextDelta { s: "d".into() }))
                .unwrap(),
            serde_json::to_value(wire(2, msg_ev("assistant"))).unwrap(),
            serde_json::to_value(wire(3, JournalWireEvent::AgentTextDelta { s: "a".into() }))
                .unwrap(),
            serde_json::to_value(wire(
                4,
                JournalWireEvent::ToolCall {
                    call_id: "c1".into(),
                    name: "Bash".into(),
                    title: "t".into(),
                    status: "running".into(),
                    input: serde_json::json!({}),
                },
            ))
            .unwrap(),
            serde_json::to_value(wire(5, msg_ev("user"))).unwrap(),
            serde_json::to_value(wire(6, msg_ev("user"))).unwrap(),
            serde_json::to_value(wire(7, msg_ev("assistant"))).unwrap(),
        ];
        handle.update(cx, |h, cx| {
            h.apply_page_response(
                page_id,
                Ok(serde_json::json!({ "records": full_chain })),
                cx,
            )
        });
        cx.run_until_parked();
        cx.executor()
            .advance_clock(INFO_DEBOUNCE + std::time::Duration::from_millis(10));
        cx.run_until_parked();
        assert_eq!(drain_info(&rx), vec!["info-s1-5".to_string()]);

        // §E.3 debounce: two committed rows inside one window coalesce into
        // a single trailing fetch keyed by the latest count (messages are
        // now {0,2,5,6,7,8,9} = 7).
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                item(
                    "s1",
                    StreamFrame::Entry {
                        seq: 8,
                        id: "w8".to_string(),
                        parent_id: None,
                        timestamp: String::new(),
                        event: msg_ev("user"),
                    },
                ),
                cx,
            )
        });
        // NB: a "user" row, not "assistant" — an appended assistant row
        // fires the materialization edge (sidebar refresh through the
        // process-global ThreadStore), which this hermetic leaf test must
        // not touch; the Q-face count is role-independent.
        handle.update(cx, |h, cx| {
            h.apply_from_server(
                item(
                    "s1",
                    StreamFrame::Entry {
                        seq: 9,
                        id: "w9".to_string(),
                        parent_id: None,
                        timestamp: String::new(),
                        event: msg_ev("user"),
                    },
                ),
                cx,
            )
        });
        // Inside the debounce window nothing has been sent yet — the
        // trailing-edge fetch is the only request the burst produces.
        cx.run_until_parked();
        assert!(
            drain_info(&rx).is_empty(),
            "the debounce window must hold the trailing fetch"
        );
        cx.executor()
            .advance_clock(INFO_DEBOUNCE + std::time::Duration::from_millis(10));
        cx.run_until_parked();
        assert_eq!(drain_info(&rx), vec!["info-s1-7".to_string()]);

        handle.update(cx, |h, _| {
            let exact = h
                .store
                .window
                .iter()
                .filter(|e| matches!(&e.event, JournalWireEvent::Message { .. }))
                .count();
            assert_eq!(
                h.info_committed, exact,
                "the incremental counter equals the full recount"
            );
            assert_eq!(exact, 7);
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
                        id: "w1".to_string(),
                        parent_id: None,
                        timestamp: String::new(),
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
                        id: "w1".to_string(),
                        parent_id: None,
                        timestamp: String::new(),
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
    /// GW5 regression: the client owns unread (§F.2) — the focused leaf
    /// never lights up, activation clears the monotonic mirrors, and the
    /// local-knowledge rise (parked error / background task) obeys the
    /// same gate.
    #[gpui::test]
    fn active_leaf_suppresses_unread_and_focus_clears(cx: &mut TestAppContext) {
        let handle = cx.new(|cx| ClientStoreHandle::leaf("s1", cx));
        let delta = |unread: Option<bool>| FromServer::Host {
            host: HostEvent::SessionStatus {
                session_id: "s1".into(),
                running: None,
                errored: None,
                unread,
                pending_auth: None,
                pending_plan: None,
                background_work: None,
            },
        };
        handle.update(cx, |h, cx| h.apply_from_server(delta(Some(true)), cx));
        assert!(
            handle.read_with(cx, |h, _| h.store.unread),
            "an unfocused leaf lights up"
        );
        handle.update(cx, |h, cx| {
            h.set_active(true, cx);
        });
        assert!(
            !handle.read_with(cx, |h, _| h.store.unread),
            "activation clears the unread mirror (focus_cleared)"
        );
        handle.update(cx, |h, cx| h.apply_from_server(delta(Some(true)), cx));
        assert!(
            !handle.read_with(cx, |h, _| h.store.unread),
            "GW5: a focused leaf never lights up"
        );
        handle.update(cx, |h, cx| {
            h.set_active(false, cx);
        });
        handle.update(cx, |h, cx| h.apply_from_server(delta(Some(true)), cx));
        assert!(
            handle.read_with(cx, |h, _| h.store.unread),
            "the unfocused leaf lights up again"
        );
        handle.update(cx, |h, cx| {
            h.set_active(true, cx);
        });
        handle.update(cx, |h, cx| h.note_local_unread(cx));
        assert!(
            !handle.read_with(cx, |h, _| h.store.unread),
            "the local rise is suppressed while focused"
        );
    }

    /// GW5 regression: multiplexer focus transitions drive the leaves'
    /// active gates; `unread_map` is the sidebar badge source; a leaf
    /// created while its session is focused starts active.
    #[gpui::test]
    fn multiplexer_focus_transitions_gate_the_leaf_mirrors(cx: &mut TestAppContext) {
        let (mux, server_conn) = test_mux(cx);
        let leaf_a = mux.update(cx, |m, cx| m.open_or_create("s-a", "/p", false, cx));
        let leaf_b = mux.update(cx, |m, cx| m.open_or_create("s-b", "/p", false, cx));
        let delta = |sid: &str| FromServer::Host {
            host: HostEvent::SessionStatus {
                session_id: sid.into(),
                running: None,
                errored: None,
                unread: Some(true),
                pending_auth: None,
                pending_plan: None,
                background_work: None,
            },
        };
        mux.update(cx, |m, cx| m.set_focused(Some("s-a"), cx));
        server_conn.send_to_client(delta("s-a"));
        server_conn.send_to_client(delta("s-b"));
        cx.run_until_parked();
        assert!(
            !leaf_a.read_with(cx, |h, _| h.store.unread),
            "the focused leaf stays dark"
        );
        assert!(
            leaf_b.read_with(cx, |h, _| h.store.unread),
            "the parked leaf lights up"
        );
        let (a, b) = mux.read_with(cx, |m, cx| {
            let map = m.unread_map(cx);
            (map.get("s-a").copied(), map.get("s-b").copied())
        });
        assert_eq!(a, Some(false), "unread_map is the sidebar badge source");
        assert_eq!(b, Some(true));
        // Switching focus clears the new foreground and re-arms the old.
        mux.update(cx, |m, cx| m.set_focused(Some("s-b"), cx));
        assert!(
            !leaf_b.read_with(cx, |h, _| h.store.unread),
            "activation clears the mirror"
        );
        server_conn.send_to_client(delta("s-a"));
        cx.run_until_parked();
        assert!(
            leaf_a.read_with(cx, |h, _| h.store.unread),
            "the deprioritized leaf re-arms"
        );
        // Local-knowledge rise: parked lights, focused is a no-op.
        mux.update(cx, |m, cx| m.note_unread("s-a", cx));
        assert!(leaf_a.read_with(cx, |h, _| h.store.unread));
        mux.update(cx, |m, cx| m.note_unread("s-b", cx));
        assert!(
            !leaf_b.read_with(cx, |h, _| h.store.unread),
            "note_unread on the focused leaf is suppressed"
        );
        // A leaf created while its session is focused starts active.
        mux.update(cx, |m, cx| m.set_focused(Some("s-c"), cx));
        let leaf_c = mux.update(cx, |m, cx| m.open_or_create("s-c", "/p", false, cx));
        server_conn.send_to_client(delta("s-c"));
        cx.run_until_parked();
        assert!(
            !leaf_c.read_with(cx, |h, _| h.store.unread),
            "ensure_leaf auto-activates the focused session's leaf"
        );
    }
}
