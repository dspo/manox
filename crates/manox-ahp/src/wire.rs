//! JSON-RPC framing helpers.
//!
//! AHP is JSON-RPC 2.0 with a `channel` on every command's and notification's
//! params, so dispatch is `(method, params.channel)` without per-method
//! knowledge. Transports own bytes; this module owns message construction and
//! the typed↔text boundary, in one place so in-process and WebSocket paths
//! cannot drift.

use ahp_types::actions::{ActionEnvelope, StateAction};
use ahp_types::messages::{
    JsonRpcError, JsonRpcErrorResponse, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest,
    JsonRpcSuccessResponse, JsonRpcVersion,
};
use serde_json::Value;

/// AHP's notification method that carries one [`ActionEnvelope`].
pub const ACTION_METHOD: &str = "action";
/// Client → host: dispatch a state mutation. A notification by spec (no ack);
/// the host answers with the echoed action envelope instead.
pub const DISPATCH_ACTION_METHOD: &str = "dispatchAction";

/// Build a request (both directions use this shape).
pub fn request(id: u64, method: &str, params: Value) -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest {
        jsonrpc: JsonRpcVersion::V2,
        id,
        method: method.to_string(),
        params: Some(params),
    })
}

/// Build a success response.
pub fn success(id: u64, result: Value) -> JsonRpcMessage {
    JsonRpcMessage::SuccessResponse(JsonRpcSuccessResponse {
        jsonrpc: JsonRpcVersion::V2,
        id,
        result,
    })
}

/// Build an error response.
pub fn error(id: u64, err: impl Into<JsonRpcError>) -> JsonRpcMessage {
    JsonRpcMessage::ErrorResponse(JsonRpcErrorResponse {
        jsonrpc: JsonRpcVersion::V2,
        id,
        error: err.into(),
    })
}

/// Build a notification (no id, no reply).
pub fn notification(method: &str, params: Value) -> JsonRpcMessage {
    JsonRpcMessage::Notification(JsonRpcNotification {
        jsonrpc: JsonRpcVersion::V2,
        method: method.to_string(),
        params: Some(params),
    })
}

/// Build the `action` notification for one envelope.
pub fn action_notification(envelope: ActionEnvelope) -> JsonRpcMessage {
    let params = serde_json::to_value(&envelope).unwrap_or(Value::Null);
    notification(ACTION_METHOD, params)
}

/// Parse one text frame. Returns the JSON-RPC error a peer would receive for a
/// malformed frame, so the caller can answer `-32700` instead of dropping it.
pub fn parse_text(text: &str) -> Result<JsonRpcMessage, JsonRpcError> {
    serde_json::from_str(text).map_err(|err| JsonRpcError {
        code: crate::codes::rpc::PARSE_ERROR,
        message: format!("malformed JSON-RPC frame: {err}"),
        data: None,
    })
}

/// Serialize one message into a text frame.
pub fn to_text(msg: &JsonRpcMessage) -> String {
    serde_json::to_string(msg).unwrap_or_else(|_| "{}".to_string())
}

/// The wire tag of an action (`"chat/turnStarted"`), for the acceptance table.
///
/// Derived from serialization rather than a hand-written match: the tag is what
/// actually travels, and AHP adds tags with newer protocol versions that this
/// build must still be able to name.
pub fn action_tag(action: &StateAction) -> String {
    serde_json::to_value(action)
        .ok()
        .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_owned))
        .unwrap_or_default()
}

/// The top-level `channel` of any command or notification params.
pub fn channel_of(params: &Value) -> Option<&str> {
    params.get("channel").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_tag_is_the_wire_discriminant() {
        let action: StateAction = serde_json::from_value(serde_json::json!({
            "type": "chat/pendingMessageRemoved",
            "kind": "steering",
            "id": "p-1",
        }))
        .expect("known action");
        assert_eq!(action_tag(&action), "chat/pendingMessageRemoved");
    }

    #[test]
    fn text_round_trip_preserves_the_message() {
        let msg = notification(
            "root/sessionAdded",
            serde_json::json!({"channel": "ahp-root://"}),
        );
        let text = to_text(&msg);
        assert_eq!(parse_text(&text).expect("parses"), msg);
    }

    #[test]
    fn malformed_frame_reports_parse_error() {
        let err = parse_text("{not json").expect_err("must fail");
        assert_eq!(err.code, crate::codes::rpc::PARSE_ERROR);
    }
}
