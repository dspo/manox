//! Client → server methods.
//!
//! [`ClientCall`] methods expect a [`crate::FromServer::Response`];
//! [`ClientNote`] methods are fire-and-forget. Variant names and field names
//! are camelCase on the wire.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::handshake::Initialize;

/// A base64-encoded image attachment (submit / steer payloads).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachment {
    /// base64-encoded image bytes.
    #[serde(with = "crate::base64_bytes")]
    #[ts(type = "string")]
    pub data: Vec<u8>,
    pub mime_type: String,
}

/// Client → server queries; each expects a [`crate::FromServer::Response`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(
    tag = "method",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientCall {
    /// Handshake; the response carries server capabilities + ack.
    Initialize(Initialize),
    /// Open (or idempotently re-open) a session; response carries the history
    /// snapshot.
    OpenSession {
        session_id: String,
    },
    ListThreads,
    ListModels,
    ListCommands,
    GetUsage {
        session_id: String,
    },
    GetCurrentModel {
        session_id: String,
    },
    ThreadInfo {
        session_id: String,
    },
    /// Attach to a terminal; response carries the scrollback snapshot.
    TerminalAttach {
        session: String,
        cols: u16,
        rows: u16,
    },
    TerminalSnapshot {
        terminal: String,
    },
    /// Bare-model completion (no agent loop).
    ModelChat {
        request_id: String,
        model: String,
        messages: serde_json::Value,
        tools: serde_json::Value,
    },
    // ── v2 write calls (§D.2, L7: writes answer with receipts only) ───────
    /// Create a session with intent; the response is `{session_id}` (id on
    /// an idempotent re-open of an existing session). `initial_model` is a
    /// canonical [`ModelRef`](crate::journal::ModelRef) (L8); approval mode
    /// and reasoning effort ride the server's wire vocabularies
    /// (`read-only`/`workspace-write`/`danger-full-access`,
    /// `low|medium|high`). Replaces the fire-and-forget
    /// [`ClientNote::CreateSession`] (kept as a compat entry through the
    /// migration window, §D.3).
    CreateSession {
        cwd: Option<String>,
        project: Option<String>,
        initial_model: Option<crate::journal::ModelRef>,
        approval_mode: Option<String>,
        reasoning_effort: Option<String>,
    },
    /// Submit a user message (starts a turn unless it is a slash command);
    /// the response is the receipt `{accepted, message_id?}` — the
    /// transcript itself arrives through the follow stream (L3/L7).
    /// `origin_rpc` echoes on the durable user-message entry's origin
    /// field once the kernel carries it (kernel-type change; T4 gap), so
    /// clients can retire their optimistic echo by correlation. Replaces
    /// the fire-and-forget [`ClientNote::Submit`] (compat entry kept).
    Submit {
        session_id: String,
        text: String,
        images: Vec<ImageAttachment>,
        origin_rpc: Option<String>,
    },
    /// Steer a message into the running turn; receipt response
    /// (`{accepted, message_id?}`). `message_id` identifies the steer
    /// (echo-retirement and `DropQueued` target); the durable steer entry
    /// carries it. Replaces [`ClientNote::Steer`] (compat entry kept).
    Steer {
        session_id: String,
        message_id: String,
        text: String,
        images: Vec<ImageAttachment>,
        origin_rpc: Option<String>,
    },
    /// Cold page-read of the journal (§D.2): reads straight from the stored
    /// active chain without activating the engine. `through_seq` is the
    /// inclusive tail (`-1` = latest); `before_seq` is an exclusive upper
    /// bound for backwards paging; `max_messages` bounds the page size.
    /// The response is `{records, has_more, cursor}` — `records` are
    /// [`JournalWireEntry`](crate::journal::JournalWireEntry) shapes.
    PageHistory {
        session_id: String,
        through_seq: i64,
        before_seq: Option<i64>,
        max_messages: Option<u32>,
    },
    /// On-demand conversation fold (§E.3, Q face): the server folds the
    /// journal (turns / messages / per-model usage), cached by
    /// `(thread_id, cursor)`. The response is the §E.3 payload; fields the
    /// fold cannot source yet are `null`.
    GetConversationInfo {
        session_id: String,
    },
}

/// Client → server fire-and-forget commands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(
    tag = "method",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ClientNote {
    /// Compat entry (§D.3 dual-protocol window): superseded by
    /// [`ClientCall::CreateSession`] (§D.2), which carries the session
    /// intent and answers with the id receipt. The server forwards this
    /// note internally to the request path; the receipt is discarded.
    /// Removal is scheduled at T10.
    CreateSession {
        session_id: String,
        cwd: Option<String>,
    },
    DisposeSession {
        session_id: String,
    },
    DetachSession {
        session_id: String,
    },
    /// Compat entry (§D.3 window): superseded by [`ClientCall::Submit`]
    /// (§D.2 receipt + `originRpc` echo retirement). Forwarded internally
    /// to the request path; removal at T10.
    Submit {
        session_id: String,
        text: String,
        images: Vec<ImageAttachment>,
        client_id: Option<String>,
    },
    /// Compat entry (§D.3 window): superseded by [`ClientCall::Steer`].
    /// Forwarded internally (the note's `client_id` becomes the call's
    /// `message_id`); removal at T10.
    Steer {
        session_id: String,
        client_id: String,
        text: String,
        images: Vec<ImageAttachment>,
    },
    DropQueued {
        session_id: String,
        client_id: String,
    },
    CancelTurn {
        session_id: String,
    },
    SetModel {
        session_id: String,
        id: String,
    },
    SetReasoningEffort {
        session_id: String,
        effort: String,
    },
    SetApprovalMode {
        session_id: String,
        mode: String,
    },
    SetCwd {
        session_id: String,
        cwd: String,
    },
    SetPlanMode {
        session_id: String,
        enabled: bool,
    },
    PlanSeedExecution {
        session_id: String,
        plan_file: String,
    },
    Compact {
        session_id: String,
        instructions: Option<String>,
    },
    Goal {
        session_id: String,
        action: String,
        objective: Option<String>,
        budget: Option<u64>,
        max_rounds: Option<u64>,
    },
    StopBackgroundTask {
        session_id: String,
        task_id: String,
    },
    ArchiveThread {
        session_id: String,
        archived: bool,
    },
    PinThread {
        session_id: String,
        pinned: bool,
    },
    FocusThread {
        session_id: Option<String>,
    },
    TerminalInput {
        terminal: String,
        #[serde(with = "crate::base64_bytes")]
        #[ts(type = "string")]
        bytes: Vec<u8>,
    },
    TerminalResize {
        terminal: String,
        cols: u16,
        rows: u16,
    },
    CancelModelChat {
        request_id: String,
    },
    Shutdown,
    /// Insert a user message without starting a turn (for batched flush:
    /// multiple `AppendUserMessage` followed by one `Submit`).
    AppendUserMessage {
        session_id: String,
        text: String,
        images: Vec<ImageAttachment>,
    },
    /// Persist a UI annotation (error/notice/plan-review) as a custom entry
    /// in the session jsonl at the current leaf.
    AppendUiNote {
        session_id: String,
        kind: String,
        data: serde_json::Value,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_session_call_serializes_with_method_tag() {
        let call = ClientCall::OpenSession {
            session_id: "t1".into(),
        };
        let json = serde_json::to_value(&call).unwrap();
        assert_eq!(json["method"], "openSession");
        assert_eq!(json["sessionId"], "t1");
        let back: ClientCall = serde_json::from_value(json).unwrap();
        assert_eq!(call, back);
    }

    #[test]
    fn submit_note_round_trips_with_images() {
        let note = ClientNote::Submit {
            session_id: "t1".into(),
            text: "hello".into(),
            images: vec![ImageAttachment {
                data: b"hi".to_vec(),
                mime_type: "image/png".into(),
            }],
            client_id: Some("c1".into()),
        };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(json["method"], "submit");
        assert_eq!(json["sessionId"], "t1");
        assert_eq!(json["images"][0]["mimeType"], "image/png");
        let back: ClientNote = serde_json::from_value(json).unwrap();
        assert_eq!(note, back);
    }

    #[test]
    fn submit_note_empty_images_serializes_empty_array() {
        let note = ClientNote::Submit {
            session_id: "t1".into(),
            text: "x".into(),
            images: vec![],
            client_id: None,
        };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(json["images"], serde_json::json!([]));
    }

    #[test]
    fn terminal_input_bytes_are_base64() {
        let note = ClientNote::TerminalInput {
            terminal: "term-1".into(),
            bytes: b"abc".to_vec(),
        };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(json["bytes"], serde_json::json!("YWJj"));
        let back: ClientNote = serde_json::from_value(json).unwrap();
        assert_eq!(note, back);
    }
}
