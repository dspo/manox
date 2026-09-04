//! T2 declaration-surface coverage harness (§J.4 / J.5, L12).
//!
//! Builds a scripted conversation (§D.1 follow stream: snapshot → journal
//! entry per declared `JournalWireEvent` → projections delta → stream end)
//! plus the scripted `HostEvent` sequence, and asserts that every name in
//! every declaration table of [`manox_protocol::surface`] (a) constructs,
//! (b) round-trips through serde, (c) serializes with exactly the declared
//! tag, and (d) appears inside some `FromServer` frame of the script.
//!
//! (d) rides frames through the v1 `FromServer::Response` outcome payload:
//! the §D.1 `StreamItem` / `StreamEnd` envelope variants cannot be added to
//! the live enums without breaking the exhaustive consumer matches in
//! `manox-session-core` / `agent-ui` (T2 stop-rule; see delivery report).
//! T4/T5 re-point the harness at the real envelope variants — only the
//! frame *construction* under test changes, never the tables.

use manox_protocol::journal::JournalWireEntry;
use manox_protocol::server::{ThreadInfoPayload, TokenUsageSnapshot};
use manox_protocol::stream::{HostEvent, StreamEndReason, StreamFrame};
use manox_protocol::surface::{
    CLIENT_CALLS, CLIENT_NOTES, DOOMED_SERVER_NOTES, FRAMES, HOST_EVENTS, JOURNAL_ENTRIES,
    PROJECTION_KEYS, SERVER_CALLS, SERVER_NOTES, frame_samples, host_samples, journal_samples,
    scripted_host_events, scripted_session, stream_end_samples,
};
use manox_protocol::wire::{ModelInfo, ThreadListItem};
use manox_protocol::{
    ClientCall, ClientNote, HookKind, ImageAttachment, Initialize, RpcError, ServerCall, ServerNote,
};

// ── builders for the current call surface (one instance per variant) ──────

fn client_call_samples() -> Vec<(String, ClientCall)> {
    vec![
        (
            "initialize".into(),
            ClientCall::Initialize(Initialize {
                client_id: "test".into(),
                capabilities: vec![HookKind::Approve],
                sessions: vec![],
            }),
        ),
        (
            "openSession".into(),
            ClientCall::OpenSession {
                session_id: "s1".into(),
            },
        ),
        ("listThreads".into(), ClientCall::ListThreads),
        ("listModels".into(), ClientCall::ListModels),
        ("listCommands".into(), ClientCall::ListCommands),
        (
            "getUsage".into(),
            ClientCall::GetUsage {
                session_id: "s1".into(),
            },
        ),
        (
            "getCurrentModel".into(),
            ClientCall::GetCurrentModel {
                session_id: "s1".into(),
            },
        ),
        (
            "threadInfo".into(),
            ClientCall::ThreadInfo {
                session_id: "s1".into(),
            },
        ),
        (
            "terminalAttach".into(),
            ClientCall::TerminalAttach {
                session: "s1".into(),
                cols: 80,
                rows: 24,
            },
        ),
        (
            "terminalSnapshot".into(),
            ClientCall::TerminalSnapshot {
                terminal: "t1".into(),
            },
        ),
        (
            "modelChat".into(),
            ClientCall::ModelChat {
                request_id: "r1".into(),
                model: "anthropic-main/claude-sonnet-4".into(),
                messages: serde_json::json!([]),
                tools: serde_json::json!([]),
            },
        ),
        (
            "createSession".into(),
            ClientCall::CreateSession {
                cwd: Some("/proj".into()),
                project: Some("/proj".into()),
                initial_model: Some(manox_protocol::ModelRef::new(
                    "DeepSeek-anthropic/deepseek-chat",
                )),
                approval_mode: Some("workspace-write".into()),
                reasoning_effort: Some("high".into()),
            },
        ),
        (
            "submit".into(),
            ClientCall::Submit {
                session_id: "s1".into(),
                text: "hello".into(),
                images: vec![ImageAttachment {
                    data: vec![1, 2, 3],
                    mime_type: "image/png".into(),
                }],
                origin_rpc: Some("rpc-echo-1".into()),
            },
        ),
        (
            "steer".into(),
            ClientCall::Steer {
                session_id: "s1".into(),
                message_id: "m-1".into(),
                text: "left".into(),
                images: vec![],
                origin_rpc: None,
            },
        ),
        (
            "pageHistory".into(),
            ClientCall::PageHistory {
                session_id: "s1".into(),
                through_seq: -1,
                before_seq: Some(40),
                max_messages: Some(32),
            },
        ),
        (
            "getConversationInfo".into(),
            ClientCall::GetConversationInfo {
                session_id: "s1".into(),
            },
        ),
    ]
}

fn client_note_samples() -> Vec<(String, ClientNote)> {
    use ClientNote::*;
    vec![
        (
            "createSession".into(),
            CreateSession {
                session_id: "s1".into(),
                cwd: Some("/proj".into()),
            },
        ),
        (
            "disposeSession".into(),
            DisposeSession {
                session_id: "s1".into(),
            },
        ),
        (
            "detachSession".into(),
            DetachSession {
                session_id: "s1".into(),
            },
        ),
        (
            "submit".into(),
            Submit {
                session_id: "s1".into(),
                text: "hello".into(),
                images: vec![ImageAttachment {
                    data: b"ab".to_vec(),
                    mime_type: "image/png".into(),
                }],
                client_id: None,
            },
        ),
        (
            "steer".into(),
            Steer {
                session_id: "s1".into(),
                client_id: "c1".into(),
                text: "mid".into(),
                images: vec![],
            },
        ),
        (
            "dropQueued".into(),
            DropQueued {
                session_id: "s1".into(),
                client_id: "c1".into(),
            },
        ),
        (
            "cancelTurn".into(),
            CancelTurn {
                session_id: "s1".into(),
            },
        ),
        (
            "setModel".into(),
            SetModel {
                session_id: "s1".into(),
                id: "DeepSeek-anthropic/deepseek-chat".into(),
            },
        ),
        (
            "setReasoningEffort".into(),
            SetReasoningEffort {
                session_id: "s1".into(),
                effort: "high".into(),
            },
        ),
        (
            "setApprovalMode".into(),
            SetApprovalMode {
                session_id: "s1".into(),
                mode: "auto-edit".into(),
            },
        ),
        (
            "setCwd".into(),
            SetCwd {
                session_id: "s1".into(),
                cwd: "/new".into(),
            },
        ),
        (
            "setPlanMode".into(),
            SetPlanMode {
                session_id: "s1".into(),
                enabled: true,
            },
        ),
        (
            "planSeedExecution".into(),
            PlanSeedExecution {
                session_id: "s1".into(),
                plan_file: "/plan.md".into(),
            },
        ),
        (
            "compact".into(),
            Compact {
                session_id: "s1".into(),
                instructions: None,
            },
        ),
        (
            "goal".into(),
            Goal {
                session_id: "s1".into(),
                action: "create".into(),
                objective: Some("ship".into()),
                budget: Some(10),
                max_rounds: None,
            },
        ),
        (
            "stopBackgroundTask".into(),
            StopBackgroundTask {
                session_id: "s1".into(),
                task_id: "b1".into(),
            },
        ),
        (
            "archiveThread".into(),
            ArchiveThread {
                session_id: "s1".into(),
                archived: true,
            },
        ),
        (
            "pinThread".into(),
            PinThread {
                session_id: "s1".into(),
                pinned: true,
            },
        ),
        (
            "focusThread".into(),
            FocusThread {
                session_id: Some("s1".into()),
            },
        ),
        (
            "terminalInput".into(),
            TerminalInput {
                terminal: "t1".into(),
                bytes: b"x".to_vec(),
            },
        ),
        (
            "terminalResize".into(),
            TerminalResize {
                terminal: "t1".into(),
                cols: 80,
                rows: 24,
            },
        ),
        (
            "cancelModelChat".into(),
            CancelModelChat {
                request_id: "r1".into(),
            },
        ),
        ("shutdown".into(), Shutdown),
        (
            "appendUserMessage".into(),
            AppendUserMessage {
                session_id: "s1".into(),
                text: "queued".into(),
                images: vec![],
            },
        ),
        (
            "appendUiNote".into(),
            AppendUiNote {
                session_id: "s1".into(),
                kind: "notice".into(),
                data: serde_json::json!({"text": "n"}),
            },
        ),
    ]
}

fn server_call_samples() -> Vec<(String, ServerCall)> {
    vec![
        (
            "approve".into(),
            ServerCall::Approve {
                session_id: "s1".into(),
                auth_id: "a1".into(),
                tool_name: "Bash".into(),
                summary: "ls".into(),
                input: serde_json::json!({"command": "ls"}),
            },
        ),
        (
            "planVerdict".into(),
            ServerCall::PlanVerdict {
                session_id: "s1".into(),
                plan_file: "/p.md".into(),
                title: "P".into(),
                content: Some("# P".into()),
            },
        ),
        (
            "askUserQuestion".into(),
            ServerCall::AskUserQuestion {
                session_id: "s1".into(),
                auth_id: "a2".into(),
                input: serde_json::json!({}),
            },
        ),
        (
            "browserOp".into(),
            ServerCall::BrowserOp {
                session_id: "s1".into(),
                op: serde_json::json!({"op": "navigate"}),
            },
        ),
        (
            "clipboardRead".into(),
            ServerCall::ClipboardRead {
                session_id: "s1".into(),
            },
        ),
        (
            "openExternal".into(),
            ServerCall::OpenExternal {
                session_id: "s1".into(),
                url: "https://x".into(),
            },
        ),
    ]
}

fn thread_info_stub() -> Box<ThreadInfoPayload> {
    Box::new(ThreadInfoPayload {
        cwd: "/proj".into(),
        project: None,
        display_title: "t".into(),
        model_id: Some("m".into()),
        model_name: None,
        model: None,
        permission_mode: "read-only".into(),
        reasoning_effort: "low".into(),
        pinned: false,
        archived: false,
        depth: 0,
        agent_label: "lead".into(),
        self_author: "captain".into(),
        cwd_path: None,
        branch: None,
        goal: None,
        goal_elapsed_seconds: None,
        plan_mode: false,
        browser_suites: vec![],
        history_phase: "ready".into(),
        running: false,
        has_interacted: false,
    })
}

fn model_stub() -> ModelInfo {
    ModelInfo {
        id: "deepseek-chat".into(),
        name: "DeepSeek Chat".into(),
        provider: "DeepSeek-anthropic".into(),
        provider_name: None,
        api: "anthropic".into(),
        context_window: 131_072,
        max_tokens: None,
    }
}

fn thread_stub() -> ThreadListItem {
    ThreadListItem {
        id: "s1".into(),
        title: "t".into(),
        updated_at: 0,
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
    }
}

/// One instance per declared `ServerNote` arm, in table order.
fn server_note_samples() -> Vec<(String, ServerNote)> {
    use ServerNote::*;
    vec![
        ("ready".into(), Ready),
        (
            "sessionCreated".into(),
            SessionCreated {
                session_id: "s1".into(),
            },
        ),
        (
            "sessionDisposed".into(),
            SessionDisposed {
                session_id: "s1".into(),
            },
        ),
        (
            "turnStarted".into(),
            TurnStarted {
                session_id: "s1".into(),
            },
        ),
        (
            "turnFinished".into(),
            TurnFinished {
                session_id: "s1".into(),
                cancelled: false,
                failed: false,
                stranded_steer_ids: vec![],
            },
        ),
        (
            "stop".into(),
            Stop {
                session_id: "s1".into(),
                reason: None,
            },
        ),
        (
            "agentText".into(),
            AgentText {
                session_id: "s1".into(),
                text: "x".into(),
            },
        ),
        (
            "agentThinking".into(),
            AgentThinking {
                session_id: "s1".into(),
                text: "x".into(),
            },
        ),
        (
            "toolCall".into(),
            ToolCall {
                session_id: "s1".into(),
                id: "tc".into(),
                name: "Bash".into(),
                title: "ls".into(),
                status: "pending".into(),
                input: None,
            },
        ),
        (
            "toolResult".into(),
            ToolResult {
                session_id: "s1".into(),
                id: "tc".into(),
                output: "o".into(),
                is_error: false,
            },
        ),
        (
            "toolOutput".into(),
            ToolOutput {
                session_id: "s1".into(),
                id: "tc".into(),
                chunk: "c".into(),
            },
        ),
        (
            "threadHistory".into(),
            ThreadHistory {
                session_id: "s1".into(),
                messages: vec![],
                display_history: serde_json::json!([]),
                auto_approved_tools: None,
                restored: true,
                loading: false,
            },
        ),
        (
            "threadInfo".into(),
            ThreadInfo {
                session_id: "s1".into(),
                info: thread_info_stub(),
            },
        ),
        (
            "threadsUpdated".into(),
            ThreadsUpdated {
                threads: vec![thread_stub()],
            },
        ),
        (
            "models".into(),
            Models {
                models: vec![model_stub()],
            },
        ),
        (
            "commands".into(),
            Commands {
                commands: serde_json::json!([]),
            },
        ),
        (
            "usage".into(),
            Usage {
                session_id: "s1".into(),
                usage: serde_json::json!({}),
                cost: 0.0,
            },
        ),
        (
            "usageSnapshot".into(),
            UsageSnapshot {
                session_id: "s1".into(),
                cumulative: TokenUsageSnapshot {
                    input: 0,
                    output: 0,
                    cache_creation: 0,
                    cache_read: 0,
                },
                per_model: Default::default(),
                cumulative_cost: 0.0,
                per_model_cost: Default::default(),
                per_request: Default::default(),
            },
        ),
        (
            "currentModel".into(),
            CurrentModel {
                session_id: "s1".into(),
                id: None,
                name: None,
            },
        ),
        (
            "planReady".into(),
            PlanReady {
                session_id: "s1".into(),
                plan_file: "/p.md".into(),
                title: "P".into(),
                content: None,
            },
        ),
        (
            "planUpdated".into(),
            PlanUpdated {
                session_id: "s1".into(),
                snapshot: None,
            },
        ),
        (
            "planModeChanged".into(),
            PlanModeChanged {
                session_id: "s1".into(),
                enabled: false,
            },
        ),
        (
            "goalChanged".into(),
            GoalChanged {
                session_id: "s1".into(),
                snapshot: None,
            },
        ),
        (
            "cwdChanged".into(),
            CwdChanged {
                session_id: "s1".into(),
                path: "/p".into(),
            },
        ),
        (
            "permissionModeChanged".into(),
            PermissionModeChanged {
                session_id: "s1".into(),
                mode: "m".into(),
            },
        ),
        (
            "reasoningEffortChanged".into(),
            ReasoningEffortChanged {
                session_id: "s1".into(),
                effort: "low".into(),
            },
        ),
        (
            "browserSuitesChanged".into(),
            BrowserSuitesChanged {
                session_id: "s1".into(),
                suites: vec![],
            },
        ),
        (
            "compactionStarted".into(),
            CompactionStarted {
                session_id: "s1".into(),
                tokens_before: 1,
            },
        ),
        (
            "compaction".into(),
            Compaction {
                session_id: "s1".into(),
                summary: "s".into(),
                retained: serde_json::json!([]),
            },
        ),
        (
            "cacheInvalidation".into(),
            CacheInvalidation {
                session_id: "s1".into(),
                reprocessed_tokens: 2,
            },
        ),
        (
            "subagentStarted".into(),
            SubagentStarted {
                session_id: "s1".into(),
                id: "sa".into(),
                agent_type: "explore".into(),
                description: "d".into(),
            },
        ),
        (
            "subagentProgress".into(),
            SubagentProgress {
                session_id: "s1".into(),
                id: "sa".into(),
                agent_type: "explore".into(),
                tool_uses: 1,
                latest_activity: None,
                status: "running".into(),
            },
        ),
        (
            "subagentChild".into(),
            SubagentChild {
                session_id: "s1".into(),
                id: "sa".into(),
                event: serde_json::json!({}),
            },
        ),
        (
            "backgroundTaskUpdated".into(),
            BackgroundTaskUpdated {
                session_id: "s1".into(),
                snapshot: serde_json::json!({}),
            },
        ),
        (
            "steerPending".into(),
            SteerPending {
                session_id: "s1".into(),
                client_id: "c".into(),
                message_id: "m".into(),
            },
        ),
        (
            "steerInjected".into(),
            SteerInjected {
                session_id: "s1".into(),
                message_id: "m".into(),
            },
        ),
        (
            "approvalDecision".into(),
            ApprovalDecision {
                session_id: "s1".into(),
                tool_call_id: "tc".into(),
                tool_name: "Bash".into(),
                tool_title: "ls".into(),
                verdict: "allow".into(),
                reason: None,
            },
        ),
        (
            "branch".into(),
            Branch {
                session_id: "s1".into(),
                branch: "main".into(),
            },
        ),
        (
            "gitStats".into(),
            GitStats {
                session_id: "s1".into(),
                stats: serde_json::json!({}),
            },
        ),
        (
            "historyProgress".into(),
            HistoryProgress {
                session_id: "s1".into(),
            },
        ),
        (
            "retry".into(),
            Retry {
                session_id: "s1".into(),
                attempt: 1,
                max_attempts: 2,
                delay_secs: 1,
                reason: "overloaded".into(),
                detail: None,
            },
        ),
        (
            "peerMessage".into(),
            PeerMessage {
                session_id: "s1".into(),
                from: "p".into(),
                content: "c".into(),
            },
        ),
        (
            "modelText".into(),
            ModelText {
                request_id: "r".into(),
                text: "t".into(),
            },
        ),
        (
            "modelThinking".into(),
            ModelThinking {
                request_id: "r".into(),
                text: "t".into(),
            },
        ),
        (
            "modelToolCall".into(),
            ModelToolCall {
                request_id: "r".into(),
                id: "tc".into(),
                name: "Bash".into(),
                input: serde_json::json!({}),
            },
        ),
        (
            "modelChatDone".into(),
            ModelChatDone {
                request_id: "r".into(),
                stop: None,
                error: None,
            },
        ),
        (
            "tokenUsage".into(),
            TokenUsage {
                session_id: "s1".into(),
                input: 1,
                output: 1,
                cache_creation: 0,
                cache_read: 0,
            },
        ),
        (
            "error".into(),
            Error {
                session_id: None,
                message: "boom".into(),
            },
        ),
    ]
}

// ── generic walk: construct + declared tag + serde round-trip ─────────────

fn walk<T: PartialEq + std::fmt::Debug>(
    table: &[&str],
    tag_field: &str,
    samples: Vec<(String, T)>,
    ser: impl Fn(&T) -> serde_json::Value,
    de: impl Fn(serde_json::Value) -> T,
) {
    let names: Vec<&str> = samples.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names, table,
        "sample set drifted from the declared table (field {tag_field:?})"
    );
    for (name, value) in &samples {
        let json = ser(value);
        assert_eq!(
            json[tag_field],
            serde_json::Value::String(name.clone()),
            "wire tag mismatch for {name}: {json}"
        );
        let back = de(json);
        assert_eq!(value, &back, "round-trip failed for {name}");
    }
}

/// Derive the wire tag of an internally tagged value by reading the field.
fn tag_of(value: &serde_json::Value, field: &str) -> String {
    value[field].as_str().expect("string tag").to_string()
}

#[test]
fn journal_entries_surface_is_complete() {
    let samples = journal_samples();
    let pairs: Vec<(String, _)> = samples
        .iter()
        .cloned()
        .map(|e| {
            let v = serde_json::to_value(&e).unwrap();
            (tag_of(&v, "type"), e)
        })
        .collect();
    walk(
        JOURNAL_ENTRIES,
        "type",
        pairs,
        |e| serde_json::to_value(e).unwrap(),
        |v| serde_json::from_value(v).unwrap(),
    );
    // The §C.1 entry envelope also round-trips every declared event.
    for (seq, event) in journal_samples().into_iter().enumerate() {
        let entry = JournalWireEntry {
            seq: seq as u64,
            id: format!("e-{seq}"),
            parent_id: Some(format!("e-{}", seq.saturating_sub(1))),
            timestamp: "2026-09-04T00:00:00Z".into(),
            event,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: JournalWireEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back, "entry envelope round-trip failed: {json}");
    }
}

#[test]
fn host_events_surface_is_complete() {
    let samples = host_samples();
    let pairs: Vec<(String, _)> = samples
        .iter()
        .cloned()
        .map(|h| {
            let v = serde_json::to_value(&h).unwrap();
            (tag_of(&v, "type"), h)
        })
        .collect();
    walk(
        HOST_EVENTS,
        "type",
        pairs,
        |h| serde_json::to_value(h).unwrap(),
        |v| serde_json::from_value::<HostEvent>(v).unwrap(),
    );
}

#[test]
fn frames_surface_is_complete() {
    let samples = frame_samples();
    let pairs: Vec<(String, _)> = samples
        .iter()
        .cloned()
        .map(|f| {
            let v = serde_json::to_value(&f).unwrap();
            (tag_of(&v, "type"), f)
        })
        .collect();
    walk(
        &FRAMES[..3],
        "type",
        pairs,
        |f| serde_json::to_value(f).unwrap(),
        |v| serde_json::from_value::<StreamFrame>(v).unwrap(),
    );
    let end_names: Vec<String> = stream_end_samples()
        .iter()
        .map(|r| tag_of(&serde_json::to_value(r).unwrap(), "type"))
        .collect();
    let declared: Vec<&str> = end_names.iter().map(String::as_str).collect();
    assert_eq!(declared, &FRAMES[3..], "stream-end tag drift");
}

#[test]
fn current_call_surface_is_complete() {
    walk(
        CLIENT_CALLS,
        "method",
        client_call_samples(),
        |c| serde_json::to_value(c).unwrap(),
        |v| serde_json::from_value::<ClientCall>(v).unwrap(),
    );
    walk(
        CLIENT_NOTES,
        "method",
        client_note_samples(),
        |c| serde_json::to_value(c).unwrap(),
        |v| serde_json::from_value::<ClientNote>(v).unwrap(),
    );
    walk(
        SERVER_CALLS,
        "method",
        server_call_samples(),
        |c| serde_json::to_value(c).unwrap(),
        |v| serde_json::from_value::<ServerCall>(v).unwrap(),
    );
    walk(
        SERVER_NOTES,
        "method",
        server_note_samples(),
        |c| serde_json::to_value(c).unwrap(),
        |v| serde_json::from_value::<ServerNote>(v).unwrap(),
    );
}

/// §J.4 (d): every declared name appears, serialized, inside some scripted
/// `FromServer` frame — the emit-side guarantee of the harness (T4/T5 will
/// drive a real server instead of the script).
#[test]
fn every_declared_surface_name_appears_in_scripted_frames() {
    let session: Vec<String> = scripted_session()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();
    let host: Vec<String> = scripted_host_events()
        .iter()
        .map(|m| serde_json::to_string(m).unwrap())
        .collect();

    for name in JOURNAL_ENTRIES {
        assert!(
            session
                .iter()
                .any(|s| s.contains(&format!("\"type\":\"{name}\""))),
            "journal entry {name} never emitted in the scripted session"
        );
    }
    for name in HOST_EVENTS {
        assert!(
            host.iter()
                .any(|s| s.contains(&format!("\"type\":\"{name}\""))),
            "host event {name} never emitted in the scripted host stream"
        );
    }
    for name in FRAMES {
        // Key-order-independent containment: serde's internally tagged
        // serialization does not place `type` first (struct-variant fields
        // may sort ahead of the tag — Snapshot and Failure both do), so never
        // needle across a `{"type":…` boundary. FRAMES names collide with
        // neither journal-entry nor host-event tags.
        let needle = format!("\"type\":\"{name}\"");
        assert!(
            session.iter().any(|s| s.contains(&needle)),
            "frame {name} never emitted in the scripted session"
        );
    }
    assert!(
        session
            .iter()
            .any(|s| s.contains("\"streamId\":\"stream-1\"")),
        "stream_id is part of the §D.1 frame shape"
    );
}

/// L5 / §D.7: the frame policy classes and the §D.7 error code set are
/// asserted through the public API (backpressure harness).
#[test]
fn backpressure_classes_and_resync_plumbing() {
    for f in frame_samples() {
        match &f {
            StreamFrame::Entry { .. } => {
                assert_eq!(
                    f.backpressure_policy(),
                    manox_protocol::transport::BackpressurePolicy::BoundedResync
                );
            }
            _ => {
                assert_eq!(
                    f.backpressure_policy(),
                    manox_protocol::transport::BackpressurePolicy::NeverDrop
                );
            }
        }
    }
    // The resync signal is expressible as a stream end + an error code.
    let reason = StreamEndReason::Resync;
    let err =
        RpcError::new(1, "queue overflow").with_code(manox_protocol::msg::CODE_RESYNC_REQUIRED);
    assert_eq!(err.data.as_ref().unwrap()["code"], "resync-required");
    assert_eq!(
        serde_json::to_value(&reason).unwrap()["type"],
        "resync",
        "resync tag drift"
    );
}

/// §E.2: the snapshot baseline carries exactly the 20 declared projection
/// keys (the scripted conversation's snapshot is the client-visible proof).
#[test]
fn snapshot_projects_every_declared_projection_key() {
    let snap = frame_samples()
        .into_iter()
        .find_map(|f| match f {
            StreamFrame::Snapshot(s) => Some(s),
            _ => None,
        })
        .expect("scripted session opens with a snapshot");
    for key in PROJECTION_KEYS {
        assert!(
            snap.projections.contains_key(*key),
            "projection key {key} missing from the snapshot baseline"
        );
    }
    assert_eq!(snap.projections.len(), PROJECTION_KEYS.len());
}

/// §D.6: the doomed set stays a strict subset of the live server-note
/// vocabulary, and survives the full walk above (they still work — T10
/// deletes them).
#[test]
fn doomed_server_notes_are_declared_and_still_functional() {
    assert!(DOOMED_SERVER_NOTES.len() < SERVER_NOTES.len());
    for name in DOOMED_SERVER_NOTES {
        assert!(SERVER_NOTES.contains(name));
    }
    // A doomed arm still round-trips (migration window keeps it honest).
    let sample = ServerNote::AgentText {
        session_id: "s1".into(),
        text: "x".into(),
    };
    let back: ServerNote = serde_json::from_value(serde_json::to_value(&sample).unwrap()).unwrap();
    assert_eq!(sample, back);
}
