//! Server → client methods.
//!
//! [`ServerCall`] methods need a [`crate::FromClient::Reply`] (adjudication /
//! capability); [`ServerNote`] are notifications. Variant names and
//! field names are camelCase on the wire.
//!
//! T10 (§D.6): the v1 session-domain note arms are gone. The surviving
//! `ServerNote` surface is the owner-control set (`Ready`,
//! `SessionCreated`/`SessionDisposed`), the transitional registry-push list
//! channel (`ThreadsUpdated`/`Models`/`Commands` — the §D.5 host-event
//! equivalents ride `FromServer::Host` and clients fold both), the
//! server-originated `Error`, and the bare-model completion side-stream
//! (`ModelText`/`ModelThinking`/`ModelToolCall`/`ModelChatDone` — the
//! `model_chat` domain, retired later under §K.6). Everything the doomed
//! arms carried now travels on the v2 journal stream, projections, and
//! host events.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::wire::{ModelInfo, ThreadListItem};

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

impl ServerCall {
    /// Every adjudication / capability call is scoped to a session; a
    /// multiplexed client routes the reply back along the same connection.
    pub fn session_id(&self) -> &str {
        match self {
            ServerCall::Approve { session_id, .. }
            | ServerCall::PlanVerdict { session_id, .. }
            | ServerCall::AskUserQuestion { session_id, .. }
            | ServerCall::BrowserOp { session_id, .. }
            | ServerCall::ClipboardRead { session_id, .. }
            | ServerCall::OpenExternal { session_id, .. } => session_id,
        }
    }
}

/// Server → client notifications (the retained §D.6 surface — see the
/// module docs for the per-group rationale).
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
    /// Transitional list channel (§D.5 mirror): registry snapshots also ride
    /// `HostEvent::{ThreadsUpdated, Models, Commands}`; clients fold both
    /// envelopes until the note arms retire with the §K.5 closeout.
    ThreadsUpdated {
        threads: Vec<ThreadListItem>,
    },
    Models {
        models: Vec<ModelInfo>,
    },
    /// Slash-command / skill list snapshot. Pushed after a `ListCommands`
    /// call so clients that read push delivery (not the Response body) stay
    /// consistent with the `Models` / `ThreadsUpdated` notification pattern.
    Commands {
        commands: serde_json::Value,
    },
    /// Server-originated transport / lifecycle error (not a turn-domain
    /// mirror — the turn `error` journal entry is the §C.2 successor for
    /// engine errors).
    Error {
        session_id: Option<String>,
        message: String,
    },
    /// `model_chat` side-stream (§D.1; the terminal/ModelChat merge is
    /// scoped later, §K.6). Keyed by `request_id`, not session.
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
    fn session_created_note_round_trips() {
        let note = ServerNote::SessionCreated {
            session_id: "t1".into(),
        };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(json["method"], "sessionCreated");
        assert_eq!(json["sessionId"], "t1");
        let back: ServerNote = serde_json::from_value(json).unwrap();
        assert_eq!(note, back);
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
    fn model_chat_done_round_trips() {
        let note = ServerNote::ModelChatDone {
            request_id: "r1".into(),
            stop: Some("end_turn".into()),
            error: None,
        };
        let json = serde_json::to_value(&note).unwrap();
        assert_eq!(json["method"], "modelChatDone");
        assert_eq!(json["stop"], "end_turn");
        let back: ServerNote = serde_json::from_value(json).unwrap();
        assert_eq!(note, back);
    }
}
