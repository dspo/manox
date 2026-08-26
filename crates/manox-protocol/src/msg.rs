//! Wire envelopes and shared types.
//!
//! [`FromClient`] / [`FromServer`] are internally tagged by `kind`
//! (`request` / `notification` / `reply` / `response`). The carried
//! [`crate::ClientCall`] / [`crate::ClientNote`] / [`crate::ServerCall`] /
//! [`crate::ServerNote`] are themselves internally tagged by `method`, nested
//! under the `call` / `note` field, so every wire message is fully
//! self-describing.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Correlation id for a request/response or call/reply pair. Opaque to the
/// transport; minted by the caller and echoed verbatim by the responder.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub struct MsgId(pub String);

impl MsgId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

/// Error carried in a `Response`/`Reply` `Err` outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
pub struct RpcError {
    /// Application-defined code; non-zero always means failure.
    pub code: i32,
    pub message: String,
    /// Optional structured detail.
    pub data: Option<serde_json::Value>,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// Client → server message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FromClient {
    /// A query needing a [`FromServer::Response`].
    Request {
        id: MsgId,
        call: crate::client::ClientCall,
    },
    /// Fire-and-forget command.
    Notification { note: crate::client::ClientNote },
    /// The client's answer to a [`FromServer::Request`] (`ServerCall`).
    Reply {
        id: MsgId,
        outcome: Result<serde_json::Value, RpcError>,
    },
}

/// Server → client message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FromServer {
    /// The server's answer to a [`FromClient::Request`] (`ClientCall`).
    Response {
        id: MsgId,
        outcome: Result<serde_json::Value, RpcError>,
    },
    /// Adjudication / capability call needing a [`FromClient::Reply`].
    Request {
        id: MsgId,
        call: crate::server::ServerCall,
    },
    /// Streaming update.
    Notification { note: crate::server::ServerNote },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msg_id_round_trips() {
        let id = MsgId::new("req-1");
        let json = serde_json::to_string(&id).unwrap();
        let back: MsgId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, back);
    }

    #[test]
    fn rpc_error_display_and_round_trip() {
        let err = RpcError::new(-32000, "boom");
        assert_eq!(err.to_string(), "[-32000] boom");
        let json = serde_json::to_string(&err).unwrap();
        let back: RpcError = serde_json::from_str(&json).unwrap();
        assert_eq!(err, back);
    }

    #[test]
    fn rpc_error_none_data_serializes_null() {
        let err = RpcError::new(1, "x");
        let json = serde_json::to_value(&err).unwrap();
        assert!(json["data"].is_null());
    }

    #[test]
    fn from_client_reply_round_trips() {
        let msg = FromClient::Reply {
            id: MsgId::new("c-1"),
            outcome: Ok(serde_json::json!({"allow": true})),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: FromClient = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn from_server_response_round_trips() {
        let msg = FromServer::Response {
            id: MsgId::new("s-1"),
            outcome: Err(RpcError::new(2, "nope")),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: FromServer = serde_json::from_str(&json).unwrap();
        assert_eq!(msg, back);
    }
}
