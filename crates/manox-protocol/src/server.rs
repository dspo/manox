//! Server → client methods.
//!
//! [`ServerCall`] methods need a [`crate::FromClient::Reply`] (adjudication /
//! capability); [`ServerNote`] are streaming notifications. Variant names and
//! field names are camelCase on the wire.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Server → client adjudication / capability calls; the client answers with a
/// [`crate::FromClient::Reply`]. Routed by session ownership ∩ declared
/// [`crate::HookKind`] capability; no capable owner fails closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(
    tag = "method",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerCall {
    /// Tool-call approval. Reply payload: `{ "allow": bool }`.
    Approve {
        session_id: String,
        auth_id: String,
        tool_name: String,
        summary: String,
        input: serde_json::Value,
    },
    /// Plan review verdict. Reply payload: `{ "choice": "execute_keep" |
    /// "execute_compact" | "refine" }`.
    PlanVerdict {
        session_id: String,
        plan_file: String,
        title: String,
        content: Option<String>,
    },
    /// Interactive question. Reply payload: `{ "answers": [[q, a], ...],
    /// "response": string | null }`.
    AskUserQuestion {
        session_id: String,
        auth_id: String,
        input: serde_json::Value,
    },
    /// Drive the built-in browser. Reply payload: op-specific.
    BrowserOp {
        session_id: String,
        op: serde_json::Value,
    },
    /// Read the client clipboard. Reply payload: `{ "data": base64,
    /// "mimeType": string }` or `null`.
    ClipboardRead { session_id: String },
    /// Open a URL / path in the OS default handler. Reply payload: `{}`.
    OpenExternal { session_id: String, url: String },
}

/// Typed thread metadata — the schema for [`ServerNote::ThreadInfo`].
/// Replaces the prior opaque `info: serde_json::Value` with a fixed contract
/// so the client store can project every field without a second implicit
/// protocol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "camelCase")]
pub struct ThreadInfoPayload {
    pub cwd: String,
    pub project: Option<String>,
    pub display_title: String,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    /// Full model descriptor serialized (provider, api, context_window, etc.).
    pub model: Option<serde_json::Value>,
    pub permission_mode: String,
    pub reasoning_effort: String,
    pub pinned: bool,
    pub archived: bool,
    pub depth: u32,
    pub agent_label: String,
    pub self_author: String,
    pub worktree_active: bool,
    pub worktree_path: Option<String>,
    pub branch: Option<String>,
    pub goal: Option<serde_json::Value>,
    pub goal_elapsed_seconds: Option<u64>,
    pub plan_mode: bool,
    pub browser_suites: Vec<String>,
    pub history_phase: String,
    pub running: bool,
    pub has_interacted: bool,
}

/// Typed token usage breakdown for [`ServerNote::UsageSnapshot`]. Defined in
/// the protocol crate (not re-exported from `agent`) so the client can
/// project every field without hardcoding JSON key names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageSnapshot {
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
}

/// Server → client streaming notifications.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(
    tag = "method",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerNote {
    Ready,
    SessionCreated {
        session_id: String,
    },
    SessionDisposed {
        session_id: String,
    },
    TurnStarted {
        session_id: String,
    },
    TurnFinished {
        session_id: String,
        cancelled: bool,
        failed: bool,
        stranded_steer_ids: Vec<String>,
    },
    Stop {
        session_id: String,
        reason: Option<String>,
    },
    AgentText {
        session_id: String,
        text: String,
    },
    AgentThinking {
        session_id: String,
        text: String,
    },
    ToolCall {
        session_id: String,
        id: String,
        name: String,
        title: String,
        status: String,
        input: Option<serde_json::Value>,
    },
    ToolResult {
        session_id: String,
        id: String,
        output: String,
        is_error: bool,
    },
    ToolOutput {
        session_id: String,
        id: String,
        chunk: String,
    },
    /// Authoritative history boundary. Carries the display sequence (messages
    /// interleaved with persisted UI annotation cards) — the client store's
    /// sole source for the conversation view, replacing direct `Thread::messages`
    /// / `display_history` reads. `restored = true` marks a reopen-from-disk
    /// boundary (was `ThreadEvent::HistoryRestored`); `loading = true` means the
    /// server is still streaming the preview and the client should gate input.
    ThreadHistory {
        session_id: String,
        messages: serde_json::Value,
        display_history: serde_json::Value,
        auto_approved_tools: Option<Vec<String>>,
        restored: bool,
        loading: bool,
    },
    /// Typed thread metadata snapshot — replaces the prior opaque
    /// `info: serde_json::Value`. Emitted on attach / model change / mode
    /// toggle / project bind. The client store projects every field directly.
    /// Boxed to keep the enum variant table reasonable (22 fields × ~24 bytes
    /// each would dominate the enum without boxing).
    ThreadInfo {
        session_id: String,
        info: Box<ThreadInfoPayload>,
    },
    ThreadsUpdated {
        threads: serde_json::Value,
    },
    Models {
        models: serde_json::Value,
    },
    /// Per-request token usage (incremental delta).
    Usage {
        session_id: String,
        usage: serde_json::Value,
        cost: f64,
    },
    /// Cumulative usage snapshot — aggregates the engine computes internally
    /// (`cumulative_token_usage` / `per_model_token_usage` / `cumulative_cost`
    /// / `per_model_cost`). The client cannot recompute these (no engine state
    /// machine), so the server must push them as a snapshot after each turn
    /// settles and on attach.
    UsageSnapshot {
        session_id: String,
        cumulative: TokenUsageSnapshot,
        per_model: HashMap<String, TokenUsageSnapshot>,
        cumulative_cost: f64,
        per_model_cost: HashMap<String, f64>,
    },
    CurrentModel {
        session_id: String,
        id: Option<String>,
        name: Option<String>,
    },
    PlanReady {
        session_id: String,
        plan_file: String,
        title: String,
        content: Option<String>,
    },
    PlanUpdated {
        session_id: String,
        snapshot: Option<serde_json::Value>,
    },
    PlanModeChanged {
        session_id: String,
        enabled: bool,
    },
    GoalChanged {
        session_id: String,
        snapshot: Option<serde_json::Value>,
    },
    WorktreeChanged {
        session_id: String,
        active: bool,
        path: Option<String>,
    },
    PermissionModeChanged {
        session_id: String,
        mode: String,
    },
    ReasoningEffortChanged {
        session_id: String,
        effort: String,
    },
    BrowserSuitesChanged {
        session_id: String,
        suites: Vec<String>,
    },
    CompactionStarted {
        session_id: String,
        tokens_before: u64,
    },
    Compaction {
        session_id: String,
        summary: String,
    },
    SubagentStarted {
        session_id: String,
        id: String,
        agent_type: String,
        description: String,
    },
    SubagentProgress {
        session_id: String,
        id: String,
        agent_type: String,
        tool_uses: u32,
        latest_activity: Option<String>,
        status: String,
    },
    SubagentChild {
        session_id: String,
        id: String,
        event: serde_json::Value,
    },
    BackgroundTaskUpdated {
        session_id: String,
        snapshot: serde_json::Value,
    },
    SteerPending {
        session_id: String,
        client_id: String,
        message_id: String,
    },
    SteerInjected {
        session_id: String,
        message_id: String,
    },
    ApprovalDecision {
        session_id: String,
        tool_call_id: String,
        tool_name: String,
        tool_title: String,
        verdict: String,
        reason: Option<String>,
    },
    Branch {
        session_id: String,
        branch: String,
    },
    GitStats {
        session_id: String,
        stats: serde_json::Value,
    },
    HistoryProgress {
        session_id: String,
    },
    Retry {
        session_id: String,
        attempt: u32,
        max_attempts: u32,
        delay_secs: u64,
        reason: String,
        detail: Option<String>,
    },
    PeerMessage {
        session_id: String,
        from: String,
        content: String,
    },
    ModelText {
        request_id: String,
        text: String,
    },
    ModelThinking {
        request_id: String,
        text: String,
    },
    ModelToolCall {
        request_id: String,
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ModelChatDone {
        request_id: String,
        stop: Option<String>,
        error: Option<String>,
    },
    TokenUsage {
        session_id: String,
        input: u64,
        output: u64,
        cache_creation: u64,
        cache_read: u64,
    },
    Error {
        session_id: Option<String>,
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approve_call_round_trips() {
        let call = ServerCall::Approve {
            session_id: "t1".into(),
            auth_id: "a1".into(),
            tool_name: "Bash".into(),
            summary: "run ls".into(),
            input: serde_json::json!({"cmd": "ls"}),
        };
        let json = serde_json::to_value(&call).unwrap();
        assert_eq!(json["method"], "approve");
        assert_eq!(json["authId"], "a1");
        let back: ServerCall = serde_json::from_value(json).unwrap();
        assert_eq!(call, back);
    }

    #[test]
    fn agent_text_note_round_trips() {
        let note = ServerNote::AgentText {
            session_id: "t1".into(),
            text: "hello".into(),
        };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(json["method"], "agentText");
        assert_eq!(json["text"], "hello");
        let back: ServerNote = serde_json::from_value(json).unwrap();
        assert_eq!(note, back);
    }

    #[test]
    fn turn_finished_empty_stranded_serializes_empty_array() {
        let note = ServerNote::TurnFinished {
            session_id: "t1".into(),
            cancelled: false,
            failed: false,
            stranded_steer_ids: vec![],
        };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(json["strandedSteerIds"], serde_json::json!([]));
    }

    #[test]
    fn error_note_allows_null_session() {
        let note = ServerNote::Error {
            session_id: None,
            message: "boom".into(),
        };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(json["method"], "error");
        assert!(json["sessionId"].is_null());
        let back: ServerNote = serde_json::from_value(json).unwrap();
        assert_eq!(note, back);
    }

    #[test]
    fn thread_info_payload_round_trips() {
        let payload = ThreadInfoPayload {
            cwd: "/tmp".into(),
            project: None,
            display_title: "Test".into(),
            model_id: Some("m1".into()),
            model_name: Some("Test Model".into()),
            model: None,
            permission_mode: "read-only".into(),
            reasoning_effort: "low".into(),
            pinned: false,
            archived: false,
            depth: 0,
            agent_label: "lead".into(),
            self_author: "captain".into(),
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
        let note = ServerNote::ThreadInfo {
            session_id: "t1".into(),
            info: Box::new(payload),
        };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(json["method"], "threadInfo");
        assert_eq!(json["info"]["displayTitle"], "Test");
        assert_eq!(json["info"]["permissionMode"], "read-only");
        assert_eq!(json["info"]["selfAuthor"], "captain");
        let back: ServerNote = serde_json::from_value(json).unwrap();
        assert_eq!(note, back);
    }

    #[test]
    fn usage_snapshot_round_trips() {
        let note = ServerNote::UsageSnapshot {
            session_id: "t1".into(),
            cumulative: TokenUsageSnapshot {
                input: 100,
                output: 50,
                cache_creation: 0,
                cache_read: 0,
            },
            per_model: HashMap::new(),
            cumulative_cost: 0.01,
            per_model_cost: HashMap::new(),
        };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(json["method"], "usageSnapshot");
        assert_eq!(json["cumulativeCost"], 0.01);
        assert_eq!(json["cumulative"]["input"], 100);
        let back: ServerNote = serde_json::from_value(json).unwrap();
        assert_eq!(note, back);
    }

    #[test]
    fn browser_suites_changed_round_trips() {
        let note = ServerNote::BrowserSuitesChanged {
            session_id: "t1".into(),
            suites: vec!["web_explore".into()],
        };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(json["method"], "browserSuitesChanged");
        assert_eq!(json["suites"][0], "web_explore");
        let back: ServerNote = serde_json::from_value(json).unwrap();
        assert_eq!(note, back);
    }

    #[test]
    fn thread_history_expanded_round_trips() {
        let note = ServerNote::ThreadHistory {
            session_id: "t1".into(),
            messages: serde_json::json!([]),
            display_history: serde_json::json!([]),
            auto_approved_tools: None,
            restored: true,
            loading: false,
        };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(json["method"], "threadHistory");
        assert_eq!(json["restored"], true);
        assert_eq!(json["loading"], false);
        let back: ServerNote = serde_json::from_value(json).unwrap();
        assert_eq!(note, back);
    }
}
