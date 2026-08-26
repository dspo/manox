//! Server → client methods.
//!
//! [`ServerCall`] methods need a [`crate::FromClient::Reply`] (adjudication /
//! capability); [`ServerNote`] are streaming notifications. Variant names and
//! field names are camelCase on the wire.

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
    /// Restored history snapshot (authoritative boundary).
    ThreadHistory {
        session_id: String,
        messages: serde_json::Value,
        auto_approved_tools: Option<Vec<String>>,
    },
    ThreadInfo {
        session_id: String,
        info: serde_json::Value,
    },
    ThreadsUpdated {
        threads: serde_json::Value,
    },
    Models {
        models: serde_json::Value,
    },
    Usage {
        session_id: String,
        usage: serde_json::Value,
        cost: f64,
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
}
