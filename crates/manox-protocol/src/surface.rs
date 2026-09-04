//! The declaration surfaces (L12: "the declared surface is the public
//! contract").
//!
//! Four const tables name — verbatim, camelCase wire tags — everything a
//! consumer may rely on across the v2 boundary:
//!
//! | table | declares | spec |
//! |---|---|---|
//! | [`JOURNAL_ENTRIES`] | every `JournalWireEvent` variant (§C.2) | §C.2 |
//! | [`PROJECTION_KEYS`] | every projection key (§E.2, first version: 20) | §E.2 |
//! | [`HOST_EVENTS`] | every `HostEvent` variant (§D.5) | §D.5 |
//! | [`FRAMES`] | every stream frame + stream-end reason (§D.1) | §D.1 |
//!
//! Stability rules (L12): a name may be *added* at the tail of a group with a
//! protocol-epoch bump; it may never be renamed or removed until T10's
//! removal pass. The `surface_*_complete` tests in
//! `tests/surface_coverage.rs` + the unit test below fail if a Rust enum
//! gains a variant that is not declared — the tables and the types cannot
//! drift apart.
//!
//! The current call surface (the v1 `ClientCall` / `ClientNote` / `ServerCall`
//! variants that survive into v2 per §D.2–D.4) is declared as auxiliary
//! tables ([`CLIENT_CALLS`], [`CLIENT_NOTES`], [`SERVER_CALLS`],
//! [`SERVER_NOTES`] + [`DOOMED_SERVER_NOTES`]) so the same coverage test can
//! walk them; §D.2/D.3 v2 upgrades (`originRpc`, `PageHistory`, …) land with
//! the T4/T5 envelope migration (stop-rule notes in the T2 delivery report).

use crate::journal::{JournalWireEntry, JournalWireEvent, UsagePayload};
use crate::stream::{ProjectionsFrame, SessionSnapshot, StreamEndReason, StreamFrame, StreamKind};
use crate::wire::{ModelInfo, ThreadListItem};
use crate::{FromClient, FromServer, MsgId};

/// The §C.2 entry vocabulary (wire tags). Grouped by the §C.2 table rows;
/// order is stable, additions go at the tail of their group.
pub const JOURNAL_ENTRIES: &[&str] = &[
    // transcript
    "message",
    "uiNote",
    // lifecycle
    "turnStart",
    "turnFinish",
    "stop",
    "retry",
    "error",
    // streaming delta
    "agentTextDelta",
    "agentThinkingDelta",
    "toolCall",
    "toolResult",
    "toolOutputChunk",
    "subagentChild",
    "subagentProgress",
    // state change
    "modelChange",
    "cwdChange",
    "projectChange",
    "permissionModeChange",
    "reasoningEffortChange",
    "planModeChange",
    "planUpdate",
    "goal",
    "title",
    "browserSuites",
    "backgroundTask",
    "approval",
    "pinnedArchived",
    // compaction / tree
    "compaction",
    "compactionStarted",
    "branchSummary",
    "label",
    "sessionInfo",
    "leaf",
    // metrics
    "metrics",
];

/// The §E.2 projection key table (first version, 20 keys). Keys are wire
/// strings of `SessionSnapshot.projections` / `ProjectionsFrame.values`; the
/// fold sources are kernel-side (§E.1), so this table names the *values* the
/// client may read (L6: client never folds domain state itself).
pub const PROJECTION_KEYS: &[&str] = &[
    "title",
    "cwd",
    "project",
    "model",
    "permission_mode",
    "reasoning_effort",
    "plan_mode",
    "plan",
    "goal",
    "running",
    "has_interacted",
    "pinned",
    "archived",
    "depth",
    "branch",
    "browser_suites",
    "pending_auth",
    "background_tasks",
    "agent_label",
    "self_author",
];

/// The §D.5 host-event vocabulary (wire tags).
pub const HOST_EVENTS: &[&str] = &[
    "ready",
    "models",
    "commands",
    "threadsUpdated",
    "sessionStatus",
    "sessionCreated",
    "sessionDisposed",
    "error",
];

/// The §D.1 frame vocabulary: each `StreamFrame` variant plus the four
/// `StreamEndReason` arms (stream lifecycle closes the frame set).
pub const FRAMES: &[&str] = &[
    "snapshot",
    "entry",
    "projections",
    "closed",
    "cancelled",
    "resync",
    "failure",
];

// ── auxiliary: the current call surface (v1 names, §D.2–D.4 survival set) ──

/// `ClientCall` wire methods. v2 (§D.2) upgrades `Submit`/`Steer`/
/// `CreateSession` to request form with `originRpc` + session intent fields
/// and adds `pageHistory` / `getConversationInfo`; those land with the
/// envelope migration (see T2 delivery report).
pub const CLIENT_CALLS: &[&str] = &[
    "initialize",
    "openSession",
    "listThreads",
    "listModels",
    "listCommands",
    "getUsage",
    "getCurrentModel",
    "threadInfo",
    "terminalAttach",
    "terminalSnapshot",
    "modelChat",
];

/// `ClientNote` wire methods; the §D.3 keep set (`DetachSession`,
/// `DisposeSession`, `DropQueued`, `CancelTurn`, `SetModel` …, `Shutdown`).
pub const CLIENT_NOTES: &[&str] = &[
    "createSession",
    "disposeSession",
    "detachSession",
    "submit",
    "steer",
    "dropQueued",
    "cancelTurn",
    "setModel",
    "setReasoningEffort",
    "setApprovalMode",
    "setCwd",
    "setPlanMode",
    "planSeedExecution",
    "compact",
    "goal",
    "stopBackgroundTask",
    "archiveThread",
    "pinThread",
    "focusThread",
    "terminalInput",
    "terminalResize",
    "cancelModelChat",
    "shutdown",
    "appendUserMessage",
    "appendUiNote",
];

/// `ServerCall` wire methods (§D.4: waterfall trio + directed capability
/// calls — all survive v2 unchanged).
pub const SERVER_CALLS: &[&str] = &[
    "approve",
    "planVerdict",
    "askUserQuestion",
    "browserOp",
    "clipboardRead",
    "openExternal",
];

/// The full current `ServerNote` method vocabulary (the migration window
/// keeps it; the §D.6 doomed arms are marked below).
pub const SERVER_NOTES: &[&str] = &[
    "ready",
    "sessionCreated",
    "sessionDisposed",
    "turnStarted",
    "turnFinished",
    "stop",
    "agentText",
    "agentThinking",
    "toolCall",
    "toolResult",
    "toolOutput",
    "threadHistory",
    "threadInfo",
    "threadsUpdated",
    "models",
    "commands",
    "usage",
    "usageSnapshot",
    "currentModel",
    "planReady",
    "planUpdated",
    "planModeChanged",
    "goalChanged",
    "cwdChanged",
    "permissionModeChanged",
    "reasoningEffortChanged",
    "browserSuitesChanged",
    "compactionStarted",
    "compaction",
    "cacheInvalidation",
    "subagentStarted",
    "subagentProgress",
    "subagentChild",
    "backgroundTaskUpdated",
    "steerPending",
    "steerInjected",
    "approvalDecision",
    "branch",
    "gitStats",
    "historyProgress",
    "retry",
    "peerMessage",
    "modelText",
    "modelThinking",
    "modelToolCall",
    "modelChatDone",
    "tokenUsage",
    "error",
];

/// The §D.6 death list: `ServerNote` arms whose durable successors are
/// journal entries / projections / host events / snapshot boundaries. They
/// stay functional through the migration window (T4–T9) and are deleted at
/// T10. `PlanReady` is listed in §D.6 with a "?" — kept *not* doomed per the
/// spec's own uncertainty marker (it has no journal/projection successor
/// today; T10 removes it only if §D.6 finalizes it).
pub const DOOMED_SERVER_NOTES: &[&str] = &[
    "agentText",
    "agentThinking",
    "toolCall",
    "toolResult",
    "toolOutput",
    "turnStarted",
    "turnFinished",
    "stop",
    "retry",
    "compactionStarted",
    "compaction",
    "subagentStarted",
    "subagentProgress",
    "subagentChild",
    "modelText",
    "modelThinking",
    "modelToolCall",
    "modelChatDone",
    "threadInfo",
    "threadHistory",
    "threadsUpdated",
    "models",
    "commands",
    "usage",
    "usageSnapshot",
    "tokenUsage",
    "currentModel",
    "planUpdated",
    "planModeChanged",
    "goalChanged",
    "cwdChanged",
    "permissionModeChanged",
    "reasoningEffortChanged",
    "browserSuitesChanged",
    "backgroundTaskUpdated",
    "steerPending",
    "steerInjected",
    "approvalDecision",
    "branch",
    "gitStats",
    "historyProgress",
    "cacheInvalidation",
    "peerMessage",
    "error",
];

/// Construct one representative of every [`JOURNAL_ENTRIES`] tag (ordered
/// exactly as the table). The coverage harness walks
/// `JOURNAL_ENTRIES.iter().zip(journal_samples())` and asserts the emitted
/// wire tag equals the declared name — if the two ever drift, the zip
/// assertion or the unit test below fails.
pub fn journal_samples() -> Vec<JournalWireEvent> {
    use JournalWireEvent::*;
    vec![
        // transcript
        Message {
            role: "user".into(),
            content: vec![serde_json::json!({"type": "text", "text": "hello"})],
            usage: None,
            origin_rpc: Some("rpc-1".into()),
        },
        UiNote {
            kind: "error".into(),
            data: serde_json::json!({"text": "oops"}),
        },
        // lifecycle
        TurnStart,
        TurnFinish {
            cancelled: false,
            failed: true,
            stranded_steer_ids: vec!["st-1".into()],
        },
        Stop {
            reason: Some("user stop".into()),
        },
        Retry {
            attempt: 2,
            max_attempts: 5,
            delay_secs: 3,
            reason: "overloaded".into(),
        },
        Error {
            message: "provider exploded".into(),
        },
        // streaming delta
        AgentTextDelta { s: "to".into() },
        AgentThinkingDelta { s: "k".into() },
        ToolCall {
            call_id: "tc-1".into(),
            name: "Bash".into(),
            title: "run ls".into(),
            status: "running".into(),
            input: serde_json::json!({"command": "ls"}),
        },
        ToolResult {
            call_id: "tc-1".into(),
            output: "a  b\n".into(),
            is_error: false,
        },
        ToolOutputChunk {
            call_id: "tc-1".into(),
            chunk: "a".into(),
        },
        SubagentChild {
            agent_id: "sub-1".into(),
            event: serde_json::json!({"type": "message"}),
        },
        SubagentProgress {
            agent_id: "sub-1".into(),
            agent_type: "explore".into(),
            tool_uses: 4,
            latest_activity: Some("reading files".into()),
            status: "running".into(),
        },
        // state change
        ModelChange {
            from: Some(crate::journal::ModelRef::new(
                "anthropic-main/claude-sonnet-4",
            )),
            to: crate::journal::ModelRef::new("DeepSeek-anthropic/deepseek-chat"),
        },
        CwdChange {
            path: "/new".into(),
        },
        ProjectChange {
            path: Some("/proj".into()),
        },
        PermissionModeChange {
            mode: "accept-edit".into(),
        },
        ReasoningEffortChange {
            effort: "high".into(),
        },
        PlanModeChange { enabled: true },
        PlanUpdate {
            snapshot: serde_json::json!({"body": "# plan"}),
        },
        Goal {
            goal: Some(serde_json::json!({"objective": "ship v2", "budget": 10})),
        },
        Title {
            title: "v2 migration".into(),
        },
        BrowserSuites {
            suites: vec!["web_explore".into()],
        },
        BackgroundTask {
            snapshot: serde_json::json!({"tasks": []}),
        },
        Approval {
            kind: "request".into(),
            auth_id: "auth-1".into(),
            tool_name: Some("Bash".into()),
            tool_call_id: Some("tc-1".into()),
            verdict: None,
            reason: None,
        },
        PinnedArchived {
            pinned: true,
            archived: false,
        },
        // compaction / tree
        Compaction {
            summary: "…".into(),
            messages_compacted: 12,
            tokens_before: 40_000,
            retained_tail: vec![serde_json::json!({"role": "user"})],
            first_kept_entry_id: Some("e-30".into()),
        },
        CompactionStarted {
            tokens_before: 40_000,
        },
        BranchSummary {
            text: "fork note".into(),
        },
        Label {
            label: "checkpoint".into(),
        },
        SessionInfo {
            data: serde_json::json!({"k": "v"}),
        },
        Leaf {
            target_id: "e-12".into(),
        },
        // metrics
        Metrics {
            kind: "cache_invalidation".into(),
            data: serde_json::json!({"reprocessedTokens": 12345}),
        },
    ]
}

/// Construct one representative of every [`HOST_EVENTS`] tag, in order.
pub fn host_samples() -> Vec<crate::stream::HostEvent> {
    use crate::stream::HostEvent::*;
    vec![
        Ready { epoch: 2 },
        Models {
            models: vec![ModelInfo {
                id: "deepseek-chat".into(),
                name: "DeepSeek Chat".into(),
                provider: "DeepSeek-anthropic".into(),
                provider_name: Some("DeepSeek".into()),
                api: "anthropic".into(),
                context_window: 131_072,
                max_tokens: Some(8_192),
            }],
        },
        Commands {
            commands: serde_json::json!([{"name": "/compact"}]),
        },
        ThreadsUpdated {
            threads: vec![ThreadListItem {
                id: "s1".into(),
                title: "v2 migration".into(),
                updated_at: 1_756_947_200,
                running: true,
                unread: false,
                errored: false,
                pending_auth: false,
                pending_plan: false,
                background_work: false,
                model_id: "DeepSeek-anthropic/deepseek-chat".into(),
                pinned: false,
                archived: false,
                parent_id: None,
                depth: 0,
            }],
        },
        SessionStatus {
            session_id: "s1".into(),
            running: Some(true),
            errored: Some(false),
            unread: Some(true),
            pending_auth: Some(false),
            pending_plan: None,
            background_work: Some(false),
        },
        SessionCreated {
            session_id: "s1".into(),
            header: header_sample(),
        },
        SessionDisposed {
            session_id: "s1".into(),
        },
        Error {
            message: "gateway/internal".into(),
        },
    ]
}

/// The §D.1 frame sample set (one per [`FRAMES`] tag, in order): the three
/// `StreamFrame` arms followed by the four `StreamEndReason` arms.
pub fn frame_samples() -> Vec<StreamFrame> {
    vec![
        StreamFrame::Snapshot(snapshot_sample()),
        StreamFrame::Entry {
            seq: 2,
            event: JournalWireEvent::AgentTextDelta { s: "hi".into() },
        },
        StreamFrame::Projections(ProjectionsFrame {
            session_id: "s1".into(),
            as_of_seq: 3,
            values: std::collections::BTreeMap::from([(
                "title".into(),
                serde_json::json!("v2 migration"),
            )]),
        }),
    ]
}

/// The four stream-end reasons, in [`FRAMES`] tail order.
pub fn stream_end_samples() -> Vec<StreamEndReason> {
    vec![
        StreamEndReason::Closed,
        StreamEndReason::Cancelled,
        StreamEndReason::Resync,
        StreamEndReason::Failure {
            code: crate::msg::CODE_RESYNC_REQUIRED.into(),
            message: "journal gap".into(),
        },
    ]
}

/// `StreamKind` sample for the scripted conversation (§D.1).
pub fn stream_kind_follow(session_id: &str) -> StreamKind {
    StreamKind::FollowSession {
        session_id: session_id.into(),
        max_messages: Some(64),
    }
}

/// The scripted session's header.
pub fn header_sample() -> crate::journal::ThreadHeader {
    crate::journal::ThreadHeader {
        id: "s1".into(),
        cwd: "/proj".into(),
        parent_session: None,
        metadata: None,
        created_at: "2026-09-04T00:00:00Z".into(),
    }
}

/// The scripted session's opening snapshot (one `message` record + a
/// projection baseline covering the §E.2 key set; L6 client reads, never
/// folds).
pub fn snapshot_sample() -> SessionSnapshot {
    let mut projections = std::collections::BTreeMap::new();
    for key in PROJECTION_KEYS {
        projections.insert((*key).to_string(), serde_json::json!(key));
    }
    SessionSnapshot {
        session_id: "s1".into(),
        header: header_sample(),
        cursor: 1,
        records: vec![JournalWireEntry {
            seq: 0,
            id: "e-0".into(),
            parent_id: None,
            timestamp: "2026-09-04T00:00:00Z".into(),
            event: JournalWireEvent::Message {
                role: "user".into(),
                content: vec![serde_json::json!({"type": "text", "text": "hello"})],
                usage: Some(UsagePayload {
                    input: 10,
                    output: 2,
                    cache_read: 0,
                    cache_write: 0,
                    reasoning: 0,
                }),
                origin_rpc: Some("rpc-echo-1".into()),
            },
        }],
        has_more: false,
        projections,
        projections_as_of_seq: 0,
    }
}

/// The scripted conversation: a follow request, the snapshot frame, one entry
/// frame per declared journal event (dense seq), the projections delta, and
/// the end reason. Drives the T4/T5-extendable coverage harness; on the v1
/// wire the frames ride as `FromServer::Response` outcome payloads because
/// the envelope cannot yet carry `StreamItem` (stop-rule notes, T2 report).
pub fn scripted_session() -> Vec<FromServer> {
    let mut out = Vec::new();
    for (seq, frame) in frame_samples().into_iter().enumerate() {
        let payload = serde_json::json!({
            "streamId": "stream-1",
            "frame": frame,
        });
        out.push(FromServer::Response {
            id: MsgId::new(format!("stream-item-{seq}")),
            outcome: Ok(payload),
        });
    }
    // Then every journal entry, one frame each, dense continuing seq.
    for (i, event) in journal_samples().into_iter().enumerate() {
        let payload = serde_json::json!({
            "streamId": "stream-1",
            "frame": StreamFrame::Entry {
                seq: (10 + i) as u64,
                event,
            },
        });
        out.push(FromServer::Response {
            id: MsgId::new(format!("journal-{i}")),
            outcome: Ok(payload),
        });
    }
    // Stream lifecycle endings (§D.1 StreamEnd analogue) — one per declared
    // reason, each on its own stream id so the emit-coverage walk sees every
    // `FRAMES` tail name in a single scripted session.
    let endings = [
        ("stream-1", StreamEndReason::Closed),
        ("stream-2", StreamEndReason::Cancelled),
        ("stream-3", StreamEndReason::Resync),
        (
            "stream-4",
            StreamEndReason::Failure {
                code: "gateway/internal".into(),
                message: "scripted failure".into(),
            },
        ),
    ];
    for (i, (stream, reason)) in endings.into_iter().enumerate() {
        out.push(FromServer::Response {
            id: MsgId::new(format!("stream-end-{i}")),
            outcome: Ok(serde_json::json!({
                "streamId": stream,
                "reason": reason,
            })),
        });
    }
    out
}

/// The scripted `HostEvent` sequence as v1-compatible frames (host events
/// ride `FromServer::Response` outcome payloads until the §D.5 envelope
/// swap).
pub fn scripted_host_events() -> Vec<FromServer> {
    host_samples()
        .into_iter()
        .enumerate()
        .map(|(i, host)| FromServer::Response {
            id: MsgId::new(format!("host-{i}")),
            outcome: Ok(serde_json::json!({ "host": host })),
        })
        .collect()
}

/// The scripted `FromClient` opening: the follow request as a v1 `Request`
/// envelope carrying a `StreamKind` payload (§D.1 `StreamOpen` analogue).
pub fn scripted_stream_open() -> FromClient {
    FromClient::Request {
        id: MsgId::new("open-1"),
        call: crate::client::ClientCall::OpenSession {
            session_id: "s1".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_key_table_has_exactly_20_entries() {
        assert_eq!(PROJECTION_KEYS.len(), 20);
    }

    #[test]
    fn journal_table_matches_sample_count() {
        assert_eq!(JOURNAL_ENTRIES.len(), journal_samples().len());
    }

    #[test]
    fn host_event_table_matches_sample_count() {
        assert_eq!(HOST_EVENTS.len(), host_samples().len());
    }

    #[test]
    fn frame_table_matches_samples() {
        assert_eq!(
            FRAMES.len(),
            frame_samples().len() + stream_end_samples().len()
        );
    }

    #[test]
    fn doomed_notes_are_a_subset_of_server_notes() {
        for name in DOOMED_SERVER_NOTES {
            assert!(
                SERVER_NOTES.contains(name),
                "doomed note {name} not in SERVER_NOTES"
            );
        }
    }

    #[test]
    fn tables_have_no_duplicate_names() {
        let tables: &[&[&str]] = &[
            JOURNAL_ENTRIES,
            PROJECTION_KEYS,
            HOST_EVENTS,
            FRAMES,
            CLIENT_CALLS,
            CLIENT_NOTES,
            SERVER_CALLS,
            SERVER_NOTES,
        ];
        for table in tables {
            let mut sorted = table.to_vec();
            sorted.sort_unstable();
            let n = sorted.len();
            sorted.dedup();
            assert_eq!(sorted.len(), n, "duplicate name in table {table:?}");
        }
    }
}
