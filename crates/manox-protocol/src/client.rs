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
    Submit {
        session_id: String,
        text: String,
        images: Vec<ImageAttachment>,
        client_id: Option<String>,
    },
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
