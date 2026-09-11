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

use crate::wire::{ModelInfo, ThreadListItem};

/// Server → client adjudication / capability calls; the client answers with a
/// [`crate::FromClient::Reply`]. Routed by session ownership ∩ declared
/// [`crate::HookKind`] capability; no capable owner fails closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "method",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ServerCall {
    /// Tool-call approval. Reply payload: `{ "allow": bool }`.
    Approve {
        /// GW3 (§D.4): stable identity of THIS delivery — the handle a
        /// [`CancelDelivery`](crate::ClientCall::CancelDelivery) call
        /// references to withdraw a pending adjudication (e.g. the client
        /// navigated away from the session). Minted per delivery by the
        /// gateway's single stamping point (`route_call`); the same
        /// fan-out delivery to N owners carries the SAME id, so the
        /// waterfall converges through the existing expire path when any
        /// recipient withdraws.
        delivery_id: String,
        session_id: String,
        auth_id: String,
        tool_name: String,
        summary: String,
        input: serde_json::Value,
    },
    /// Plan review verdict. Reply payload: `{ "choice": "execute_keep" |
    /// "execute_compact" | "refine" }`.
    PlanVerdict {
        /// GW3 (§D.4): stable delivery identity — see [`Self::Approve`].
        delivery_id: String,
        session_id: String,
        plan_file: String,
        title: String,
        content: Option<String>,
    },
    /// Interactive question. Reply payload: `{ "answers": [[q, a], ...],
    /// "response": string | null }`.
    AskUserQuestion {
        /// GW3 (§D.4): stable delivery identity — see [`Self::Approve`].
        delivery_id: String,
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
    /// Execute one of the client's registered tools
    /// ([`crate::ClientCall::RegisterSessionTools`]). Routed to the
    /// registering client. Reply payload:
    /// `{ "content": string, "isError": bool }` (or RPC `Err`).
    InvokeClientTool {
        /// Stable delivery identity — withdrawable via
        /// [`crate::ClientCall::CancelDelivery`], like `Approve`.
        delivery_id: String,
        session_id: String,
        client_id: String,
        tool_call_id: String,
        name: String,
        input: serde_json::Value,
    },
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
            | ServerCall::OpenExternal { session_id, .. }
            | ServerCall::InvokeClientTool { session_id, .. } => session_id,
        }
    }

    /// GW3 (§D.4): the withdrawable-delivery identity of the waterfall trio;
    /// `None` for the directed capability calls (they are single-target RPCs
    /// with a timeout, not cancellable fan-out deliveries).
    pub fn delivery_id(&self) -> Option<&str> {
        match self {
            ServerCall::Approve { delivery_id, .. }
            | ServerCall::PlanVerdict { delivery_id, .. }
            | ServerCall::AskUserQuestion { delivery_id, .. }
            | ServerCall::InvokeClientTool { delivery_id, .. } => Some(delivery_id),
            ServerCall::BrowserOp { .. }
            | ServerCall::ClipboardRead { .. }
            | ServerCall::OpenExternal { .. } => None,
        }
    }
}

/// Server → client notifications (the retained §D.6 surface — see the
/// module docs for the per-group rationale).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
            delivery_id: "dlv-t1-1".into(),
            session_id: "t1".into(),
            auth_id: "a1".into(),
            tool_name: "Bash".into(),
            summary: "run ls".into(),
            input: serde_json::json!({"cmd": "ls"}),
        };
        let json = serde_json::to_value(&call).unwrap();
        assert_eq!(json["method"], "approve");
        assert_eq!(json["authId"], "a1");
        // GW3: the delivery identity rides the wire in camelCase.
        assert_eq!(json["deliveryId"], "dlv-t1-1");
        let back: ServerCall = serde_json::from_value(json).unwrap();
        assert_eq!(call, back);
        assert_eq!(call.delivery_id(), Some("dlv-t1-1"));
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

    /// GW3 (§D.4): the adjudication trio carries a stable `deliveryId` on the
    /// wire — the handle a client's `cancelDelivery` call references to
    /// withdraw a pending delivery. Parsed from wire JSON so the pin holds
    /// against pre-GW3 enums too (they ignore the unknown field, and the
    /// re-serialization then lacks it — the red evidence).
    #[test]
    fn adjudication_calls_carry_delivery_id_on_the_wire() {
        let approve: ServerCall = serde_json::from_value(serde_json::json!({
            "method": "approve",
            "sessionId": "s1",
            "deliveryId": "dlv-s1-1",
            "authId": "a1",
            "toolName": "Bash",
            "summary": "ls",
            "input": {"command": "ls"},
        }))
        .expect("Approve with deliveryId parses");
        assert_eq!(
            serde_json::to_value(&approve).unwrap()["deliveryId"],
            serde_json::json!("dlv-s1-1"),
            "GW3: Approve carries deliveryId"
        );
        assert_eq!(approve.delivery_id(), Some("dlv-s1-1"));

        let verdict: ServerCall = serde_json::from_value(serde_json::json!({
            "method": "planVerdict",
            "sessionId": "s1",
            "deliveryId": "dlv-s1-2",
            "planFile": "/p.md",
            "title": "P",
            "content": null,
        }))
        .expect("PlanVerdict with deliveryId parses");
        assert_eq!(
            serde_json::to_value(&verdict).unwrap()["deliveryId"],
            serde_json::json!("dlv-s1-2"),
            "GW3: PlanVerdict carries deliveryId"
        );

        let ask: ServerCall = serde_json::from_value(serde_json::json!({
            "method": "askUserQuestion",
            "sessionId": "s1",
            "deliveryId": "dlv-s1-3",
            "authId": "q1",
            "input": {},
        }))
        .expect("AskUserQuestion with deliveryId parses");
        assert_eq!(
            serde_json::to_value(&ask).unwrap()["deliveryId"],
            serde_json::json!("dlv-s1-3"),
            "GW3: AskUserQuestion carries deliveryId"
        );

        // Directed capability calls carry no delivery identity (§D.4: only
        // the waterfall trio is withdrawable).
        let browser: ServerCall = serde_json::from_value(serde_json::json!({
            "method": "browserOp",
            "sessionId": "s1",
            "op": {},
        }))
        .expect("BrowserOp parses");
        assert_eq!(browser.delivery_id(), None);
    }
}
