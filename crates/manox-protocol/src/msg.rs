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

    /// ε: transmission consistency — verify every `FromClient` and
    /// `FromServer` variant survives a serde JSON round-trip. The single
    /// protocol surface guarantee means in-process (typed) and serde
    /// (JSON) paths must produce identical messages.
    #[test]
    fn all_from_client_variants_serde_round_trip() {
        let msgs = vec![
            FromClient::Request {
                id: MsgId::new("r-1"),
                call: crate::client::ClientCall::Initialize(crate::handshake::Initialize {
                    client_id: "test".into(),
                    capabilities: vec![crate::handshake::HookKind::Approve],
                    sessions: vec![],
                }),
            },
            FromClient::Request {
                id: MsgId::new("r-2"),
                call: crate::client::ClientCall::ListThreads,
            },
            FromClient::Request {
                id: MsgId::new("r-3"),
                call: crate::client::ClientCall::ThreadInfo {
                    session_id: "s1".into(),
                },
            },
            FromClient::Notification {
                note: crate::client::ClientNote::CreateSession {
                    session_id: "s1".into(),
                    cwd: Some("/proj".into()),
                },
            },
            FromClient::Notification {
                note: crate::client::ClientNote::Submit {
                    session_id: "s1".into(),
                    text: "hello".into(),
                    images: vec![],
                    client_id: None,
                },
            },
            FromClient::Notification {
                note: crate::client::ClientNote::CancelTurn {
                    session_id: "s1".into(),
                },
            },
            FromClient::Notification {
                note: crate::client::ClientNote::SetModel {
                    session_id: "s1".into(),
                    id: "claude-sonnet-4".into(),
                },
            },
            FromClient::Notification {
                note: crate::client::ClientNote::SetCwd {
                    session_id: "s1".into(),
                    cwd: "/new".into(),
                },
            },
            FromClient::Notification {
                note: crate::client::ClientNote::AppendUserMessage {
                    session_id: "s1".into(),
                    text: "batched".into(),
                    images: vec![],
                },
            },
            FromClient::Notification {
                note: crate::client::ClientNote::AppendUiNote {
                    session_id: "s1".into(),
                    kind: "error".into(),
                    data: serde_json::json!({"text": "oops"}),
                },
            },
            FromClient::Notification {
                note: crate::client::ClientNote::ArchiveThread {
                    session_id: "s1".into(),
                    archived: true,
                },
            },
            FromClient::Notification {
                note: crate::client::ClientNote::Goal {
                    session_id: "s1".into(),
                    action: "create".into(),
                    objective: Some("do thing".into()),
                    budget: Some(1000),
                    max_rounds: Some(10),
                },
            },
            FromClient::Reply {
                id: MsgId::new("rep-1"),
                outcome: Ok(serde_json::json!({"allow": true})),
            },
            FromClient::Reply {
                id: MsgId::new("rep-2"),
                outcome: Err(RpcError::new(-1, "denied")),
            },
        ];
        for msg in &msgs {
            let json = serde_json::to_string(msg).unwrap();
            let back: FromClient = serde_json::from_str(&json).unwrap();
            assert_eq!(msg, &back, "FromClient serde round-trip failed: {json}");
        }
    }

    #[test]
    fn all_from_server_variants_serde_round_trip() {
        let msgs = vec![
            FromServer::Response {
                id: MsgId::new("r-1"),
                outcome: Ok(serde_json::json!({"threads": []})),
            },
            FromServer::Response {
                id: MsgId::new("r-2"),
                outcome: Err(RpcError::new(1, "bad request")),
            },
            FromServer::Request {
                id: MsgId::new("adj-1"),
                call: crate::server::ServerCall::Approve {
                    session_id: "s1".into(),
                    auth_id: "auth-1".into(),
                    tool_name: "Bash".into(),
                    summary: "rm -rf".into(),
                    input: serde_json::json!({"command": "rm"}),
                },
            },
            FromServer::Request {
                id: MsgId::new("adj-2"),
                call: crate::server::ServerCall::AskUserQuestion {
                    session_id: "s1".into(),
                    auth_id: "auth-2".into(),

                    input: serde_json::json!({}),
                },
            },
            FromServer::Request {
                id: MsgId::new("adj-3"),
                call: crate::server::ServerCall::PlanVerdict {
                    session_id: "s1".into(),
                    plan_file: "/plan.md".into(),
                    title: "Plan".into(),
                    content: Some("# Plan".into()),
                },
            },
            FromServer::Notification {
                note: crate::server::ServerNote::Ready,
            },
            FromServer::Notification {
                note: crate::server::ServerNote::SessionCreated {
                    session_id: "s1".into(),
                },
            },
            FromServer::Notification {
                note: crate::server::ServerNote::TurnStarted {
                    session_id: "s1".into(),
                },
            },
            FromServer::Notification {
                note: crate::server::ServerNote::CacheInvalidation {
                    session_id: "s1".into(),
                    reprocessed_tokens: 12345,
                },
            },
            FromServer::Notification {
                note: crate::server::ServerNote::TurnFinished {
                    cancelled: false,
                    failed: false,
                    stranded_steer_ids: vec![],
                    session_id: "s1".into(),
                },
            },
            FromServer::Notification {
                note: crate::server::ServerNote::UsageSnapshot {
                    session_id: "s1".into(),
                    cumulative: crate::server::TokenUsageSnapshot {
                        input: 100,
                        output: 50,
                        cache_creation: 0,
                        cache_read: 0,
                    },
                    per_model: std::collections::HashMap::new(),
                    cumulative_cost: 0.01,
                    per_model_cost: std::collections::HashMap::new(),
                    per_request: std::collections::HashMap::new(),
                },
            },
        ];
        for msg in &msgs {
            let json = serde_json::to_string(msg).unwrap();
            let back: FromServer = serde_json::from_str(&json).unwrap();
            assert_eq!(msg, &back, "FromServer serde round-trip failed: {json}");
        }
    }
}
