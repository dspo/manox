//! Kernel `ThreadEvent` → v2 `ServerCall` adjudication translation.
//!
//! The v2 half of what was one `translate` module. The journal projection
//! (`SessionTreeEntry` → §C.2 vocabulary) moved to `manox-ahp-runtime`, because
//! the AHP fold needs it and it must outlive this gateway; what remains is the
//! part that names the retiring protocol's vocabulary, so it goes when v2 does.

//! §二.10③ retired its never-constructed variant.)

use manox_protocol::ServerCall;

/// The translation result for one `ThreadEvent`.
pub enum Translated {
    /// An adjudication / capability call — the server issues it and awaits the
    /// client's [`FromClient::Reply`].
    Call(ServerCall),
    /// Not over the wire: diagnostic-only or carried by the v2 stream
    /// (journal entries / projections / host deltas).
    Skip,
}

/// Translate one kernel event into its protocol form.
pub fn translate(ev: &manox_agent::thread::ThreadEvent, session_id: &str) -> Translated {
    use manox_agent::thread::ThreadEvent;

    // T10 (§D.6): every session-scoped domain fact rides the journal —
    // entries for the transcript/delta stream, projections for hot state,
    // SessionStatus host deltas for list flags. The v1 note mirrors are
    // deleted; the ONLY kernel event that still crosses here is the
    // authorization request, which is an adjudication (ServerCall
    // waterfall, §D.4), not a state broadcast.
    match ev {
        ThreadEvent::ToolCallAuthorization {
            id,
            tool_name,
            summary,
            input,
        } => {
            // AskUserQuestion's authorization is an interactive question, not a
            // bare allow/deny: route it as its own ServerCall kind so the
            // client renders the ask card and returns structured answers.
            // GW3: `delivery_id` stays empty here — translate is a pure
            // projection; the gateway's single routing point (`route_call`)
            // stamps the delivery identity before the frame hits the wire.
            if tool_name == manox_agent::tools::ASK_USER_QUESTION {
                Call(ServerCall::AskUserQuestion {
                    delivery_id: String::new(),
                    session_id: session_id.into(),
                    auth_id: id.clone(),
                    input: input.clone(),
                })
            } else {
                Call(ServerCall::Approve {
                    delivery_id: String::new(),
                    session_id: session_id.into(),
                    auth_id: id.clone(),
                    tool_name: tool_name.clone(),
                    summary: summary.clone(),
                    input: input.clone(),
                })
            }
        }
        // Journal entries + projections + host deltas carry everything else.
        ThreadEvent::AgentText(_)
        | ThreadEvent::AgentThinking(_)
        | ThreadEvent::ToolCall { .. }
        | ThreadEvent::ToolResult { .. }
        | ThreadEvent::ToolOutput { .. }
        | ThreadEvent::TurnStarted
        | ThreadEvent::TurnFinished { .. }
        | ThreadEvent::Stop(_)
        | ThreadEvent::Retry { .. }
        | ThreadEvent::Error(_)
        | ThreadEvent::ModelChanged { .. }
        | ThreadEvent::TokenUsageUpdated(_)
        | ThreadEvent::PermissionModeChanged { .. }
        | ThreadEvent::ReasoningEffortChanged { .. }
        | ThreadEvent::BrowserSuitesChanged { .. }
        | ThreadEvent::PlanReady { .. }
        | ThreadEvent::PlanUpdated { .. }
        | ThreadEvent::PlanModeChanged { .. }
        | ThreadEvent::GoalChanged { .. }
        | ThreadEvent::CwdChanged { .. }
        | ThreadEvent::CompactionStarted { .. }
        | ThreadEvent::Compaction { .. }
        | ThreadEvent::SubagentStarted { .. }
        | ThreadEvent::SubagentProgress { .. }
        | ThreadEvent::SubagentChild { .. }
        | ThreadEvent::BackgroundTaskUpdated { .. }
        | ThreadEvent::SteerInjected { .. }
        // Client-fold vocabulary: the SERVER never emits it, so nothing can
        // arrive here to translate — the arm only keeps the match exhaustive
        // (v2 never carries it on the wire).
        | ThreadEvent::UserRowLanded { .. }
        | ThreadEvent::PeerMessage { .. }
        | ThreadEvent::HistoryProgress
        | ThreadEvent::HistoryRestored
        | ThreadEvent::TitleChanged { .. }
        | ThreadEvent::PrefixStability { .. }
        | ThreadEvent::SideCallMetricsUpdated(_)
        | ThreadEvent::MainCallMetricsUpdated(_)
        | ThreadEvent::CacheInvalidation { .. } => Skip,
    }
}

// ── v4 journal → wire mapping (§C.2 / §D.1, T4) ─────────────────────────────
//
// The kernel `SessionTreeEntry` vocabulary (38 variants, journal v4) projects
// onto the wire [`manox_protocol::JournalWireEvent`] (38 variants). The
// projection is TOTAL: every kernel variant has a §C.2 wire row — including
// the wire-opaque extension kinds (`ActiveToolsChange`, `Custom`,
// `CustomMessage`), which ride verbatim. Totality is a hard §F.1 requirement:
// the client fold's `assertPage` adjacency demands a seq-dense wire page, so a
// wire-less kind would punch a hole in the follow snapshot and loop the client
// on snapshot → Resync. The two new wire fields carried by every entry
// (`id`/`parentId`) and the `seq` stamp ride [`wire_entry`]; `id → callId` /
// `agentId` renames follow the §C.1 envelope-key exclusivity rule.

/// One journal record as the wire carries it (§C.1 entry envelope). The
/// projection is total — `None` is unreachable and kept only for signature
/// stability across the T-wave.
use Translated::*;

#[cfg(test)]
mod tests {
    use manox_harness::types::AgentMessage;
    use manox_journal::{JournalWireEntry, JournalWireEvent};

    /// T10 (§D.6): the v1 `Compaction` note mirror is gone — the transcript
    /// replace signal is the §C.2 `compaction` journal row, forwarded over the
    /// follow stream. The tail must flow through the REAL wire mapping, not a
    /// synthetic empty array: the client store replaces its transcript with
    /// `summary + retainedTail`, so an empty-by-default field would wipe every
    /// client's transcript on each real compaction. (The `None`-tail round-trip
    /// is pinned by the coverage test below; this one pins tail survival.)
    #[test]
    fn compaction_wire_row_carries_the_retained_tail() {
        let now = chrono::Utc::now();
        let tail = vec![
            AgentMessage::Assistant {
                content: vec![manox_harness::types::ContentBlock::Text {
                    text: "kept answer".into(),
                    signature: None,
                }],
                model: "m".into(),
                provider: "p".into(),
                api: "anthropic".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                stop_reason: Some(manox_harness::types::StopReason::Stop),
                raw_stop_reason: None,
                usage: Box::default(),
                error_message: None,
                timestamp: now,
            },
            AgentMessage::User {
                content: vec![manox_harness::types::ContentBlock::Text {
                    text: "kept follow-up".into(),
                    signature: None,
                }],
                timestamp: now,
                id: None,
            },
        ];
        let entry = manox_ahp_runtime::translate::wire_entry(
            7,
            &manox_harness::session::SessionTreeEntry::Compaction {
                id: "c1".into(),
                parent_id: None,
                timestamp: now,
                summary: "folded".into(),
                first_kept_entry_id: None,
                tokens_before: 100_000,
                retained_tail: Some(tail.clone()),
                usage: None,
                details: None,
                from_hook: None,
            },
        )
        .expect("compaction maps to a wire row");
        let JournalWireEvent::Compaction {
            summary,
            tokens_before,
            retained_tail,
            ..
        } = &entry.event
        else {
            panic!("expected a compaction wire row");
        };
        assert_eq!(summary, "folded");
        assert_eq!(*tokens_before, 100_000);
        assert_eq!(retained_tail.len(), 2, "the tail survives the wire form");
        // And it survives the JSON form the follow stream actually carries
        // (§C.1: the event is flattened into the entry envelope).
        let json = serde_json::to_value(&entry).expect("row serializes");
        assert_eq!(json["type"].as_str(), Some("compaction"));
        assert_eq!(
            json["retainedTail"].as_array().map(Vec::len),
            Some(2),
            "retained tail is present, not empty-by-default"
        );
        let back: JournalWireEntry = serde_json::from_value(json).expect("row parses back");
        assert_eq!(entry, back);
        // Each carried block round-trips to the kernel message shape the
        // transcript rebuild consumes.
        let round: Vec<AgentMessage> = retained_tail
            .iter()
            .map(|v| serde_json::from_value(v.clone()).expect("retained message deserializes back"))
            .collect();
        assert_eq!(round.len(), tail.len());
    }

    /// §J.4 (d) coverage: the kernel `SessionTreeEntry` vocabulary has 38
    /// variants and `wire_event` maps every one of them — the projection is
    /// TOTAL (§F.1 density: the follow snapshot page must be seq-dense, so no
    /// kernel kind may be wire-less). Two locks, each covering what the
    /// other cannot: the exhaustive `match` in `wire_event` makes a NEW
    /// variant without a wire arm a compile error (but says nothing about
    /// the sample list), and the length cross-check against
    /// `JOURNAL_ENTRIES` makes a new variant WITHOUT a sample here a test
    /// failure (round 3 §二.7④ — PlanReview sat exactly in that gap: armed,
    /// declared, unsampled, self-asserted "37"). The `dropped` leg pins the
    /// value-level half: no EXISTING arm may map to `None`.
    #[test]
    fn wire_projection_covers_every_kernel_variant_totally() {
        use manox_harness::session::SessionTreeEntry as E;
        let now = chrono::Utc::now();
        let id = || "e".to_string();
        let pid = || Option::<String>::None;
        let all = vec![
            E::Message {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                message: AgentMessage::User {
                    content: vec![],
                    timestamp: now,
                    id: None,
                },
                origin: None,
            },
            E::Compaction {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                summary: "s".into(),
                first_kept_entry_id: None,
                tokens_before: 0,
                retained_tail: None,
                usage: None,
                details: None,
                from_hook: None,
            },
            E::ModelChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                provider: "p".into(),
                model_id: "m".into(),
            },
            E::ThinkingLevelChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                thinking_level: "high".into(),
            },
            E::CwdChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                cwd: "/x".into(),
            },
            E::ActiveToolsChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                active_tool_names: vec![],
            },
            E::BranchSummary {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                from_id: "f".into(),
                summary: "s".into(),
                details: None,
                usage: None,
                from_hook: None,
            },
            E::Custom {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                custom_type: "c".into(),
                data: None,
            },
            E::CustomMessage {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                custom_type: "c".into(),
                content: vec![],
                details: None,
                display: false,
            },
            E::Label {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                target_id: "t".into(),
                label: Some("l".into()),
            },
            E::SessionInfo {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                name: Some("n".into()),
            },
            E::Leaf {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                target_id: Some("t".into()),
            },
            // Round 3 §二.7④: PlanReview was the missing 38th sample —
            // the wire arm existed (see `wire_event`), the builder census
            // had it, only this list lagged at 37.
            E::PlanReview {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                state: "proposed".into(),
                plan_file: None,
            },
            E::UiNote {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                note: serde_json::json!({"kind": "error"}),
            },
            E::TurnStart {
                id: id(),
                parent_id: pid(),
                timestamp: now,
            },
            E::TurnFinish {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                cancelled: false,
                failed: false,
                stranded_steer_ids: vec![],
            },
            E::Stop {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                reason: None,
            },
            E::Retry {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                attempt: 1,
                max_attempts: 2,
                delay_secs: 3,
                reason: "r".into(),
                detail: None,
            },
            E::ErrorEvent {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                message: "m".into(),
            },
            E::AgentTextDelta {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                delta: "d".into(),
            },
            E::AgentThinkingDelta {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                delta: "d".into(),
            },
            E::ToolCall {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                call_id: "c".into(),
                name: "n".into(),
                title: "t".into(),
                status: "running".into(),
                input: None,
            },
            E::ToolResult {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                call_id: "c".into(),
                output: "o".into(),
                is_error: false,
            },
            E::ToolOutputChunk {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                call_id: "c".into(),
                chunk: "x".into(),
            },
            E::SubagentChild {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                agent_id: "a".into(),
                event: serde_json::json!({}),
            },
            E::SubagentProgress {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                agent_id: "a".into(),
                agent_type: "t".into(),
                tool_uses: 0,
                latest_activity: None,
                status: "running".into(),
            },
            E::ProjectChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                path: None,
            },
            E::PermissionModeChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                mode: "read-only".into(),
            },
            E::PlanModeChange {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                enabled: true,
            },
            E::PlanModeRequest {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                enabled: true,
            },
            E::PlanUpdate {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                snapshot: serde_json::json!({}),
            },
            E::Goal {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                goal: None,
            },
            E::Title {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                title: "t".into(),
            },
            E::BrowserSuites {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                suites: vec![],
            },
            E::BackgroundTask {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                snapshot: serde_json::json!({}),
            },
            E::Approval {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                kind: "request".into(),
                auth_id: "a".into(),
                payload: serde_json::json!({}),
            },
            E::Question {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                kind: "request".into(),
                auth_id: "a".into(),
                payload: serde_json::json!({}),
            },
            E::PinnedArchived {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                pinned: false,
                archived: false,
            },
            E::CompactionStarted {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                tokens_before: 0,
            },
            E::Metrics {
                id: id(),
                parent_id: pid(),
                timestamp: now,
                metric_type: "side_call".into(),
                data: serde_json::json!({}),
            },
        ];
        assert_eq!(
            all.len(),
            manox_protocol::surface::JOURNAL_ENTRIES.len(),
            "the kernel sample must stay 1:1 with the wire declaration table \
             (a new kernel variant forces a wire arm to compile, the \
             wire_surface! macro forces its JOURNAL_ENTRIES row, and this \
             cross-check forces its sample here — round 3 §二.7④ replaced \
             the stale self-asserted 37 with this cross-face lock)"
        );
        // The projection is TOTAL (§F.1 density): no kernel kind may be
        // wire-less — a hole in the follow snapshot page violates the client
        // fold's `assertPage` adjacency and loops snapshot → Resync (the
        // #765 real-data regression: legacy `custom` / `active_tools_change`
        // rows in the user's sessions).
        let dropped: Vec<&str> = all
            .iter()
            .filter(|e| manox_ahp_runtime::translate::wire_event(e).is_none())
            .map(|e| {
                // Kernel entry type tag (matches §C.1 `entry_kind`).
                manox_harness::session::entry_kind(e).as_str()
            })
            .collect();
        assert_eq!(
            dropped,
            Vec::<&str>::new(),
            "no kernel variant may lack a wire row"
        );
        // All of them project (and round-trip through the wire enum).
        for e in &all {
            if manox_ahp_runtime::translate::wire_event(e).is_none() {
                continue;
            }
            let entry = manox_ahp_runtime::translate::wire_entry(0, e)
                .expect("mapped variant yields an entry");
            let json = serde_json::to_value(&entry).expect("entry serializes");
            let back: JournalWireEntry = serde_json::from_value(json).expect("entry parses");
            assert_eq!(
                entry,
                back,
                "wire round-trip for {}",
                manox_harness::session::entry_kind(e).as_str()
            );
        }
        // The legacy extension kinds land on their dedicated wire rows.
        let custom = all
            .iter()
            .find(|e| matches!(e, E::Custom { .. }))
            .expect("custom sample present");
        assert!(matches!(
            manox_ahp_runtime::translate::wire_event(custom),
            Some(JournalWireEvent::Custom { .. })
        ));
        let tools = all
            .iter()
            .find(|e| matches!(e, E::ActiveToolsChange { .. }))
            .expect("active-tools sample present");
        assert!(matches!(
            manox_ahp_runtime::translate::wire_event(tools),
            Some(JournalWireEvent::ActiveToolsChange { .. })
        ));
    }
}
