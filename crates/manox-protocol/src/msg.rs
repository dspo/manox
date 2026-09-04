//! Wire envelopes and shared types.
//!
//! [`FromClient`] / [`FromServer`] are internally tagged by `kind`
//! (`request` / `notification` / `reply` / `response`). The carried
//! [`crate::ClientCall`] / [`crate::ClientNote`] / [`crate::ServerCall`] /
//! [`crate::ServerNote`] are themselves internally tagged by `method`, nested
//! under the `call` / `note` field, so every wire message is fully
//! self-describing.
//!
//! Protocol v2 (§D.1) adds the stream classes — `FromClient::StreamOpen /
//! StreamCancel` and `FromServer::StreamItem / StreamEnd` — over the payload
//! vocabulary in [`crate::stream`] ([`StreamKind`](crate::stream::StreamKind),
//! [`StreamFrame`](crate::stream::StreamFrame),
//! [`StreamEndReason`](crate::stream::StreamEndReason),
//! [`HostEvent`](crate::stream::HostEvent)). The variants landed with the T4
//! stream services; v1 consumers tolerate them per L12 (unknown variants are
//! dropped + logged, never fatal), and the §D.5 `HostEvent` re-type of
//! `FromServer::Notification` still waits on the T5 consumer migration.

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

/// Stable application error codes for v2 failures (§D.7). The numeric `code`
/// stays an `i32` for wire-compat with v1 consumers; v2 servers carry one of
/// these strings in [`RpcError::data`] under the key `"code"` (e.g.
/// `RpcError::new(-1, msg).with_code(CODE_MODEL_UNRESOLVABLE)`).
pub const RPC_ERROR_CODES: &[&str] = &[
    "session/not-found",
    "session/busy",
    "gateway/bad-request",
    "gateway/internal",
    "resync-required",
    "model/unresolvable",
];

/// `session/not-found` (§D.7).
pub const CODE_SESSION_NOT_FOUND: &str = "session/not-found";
/// `session/busy` (§D.7).
pub const CODE_SESSION_BUSY: &str = "session/busy";
/// `gateway/bad-request` (§D.7).
pub const CODE_GATEWAY_BAD_REQUEST: &str = "gateway/bad-request";
/// `gateway/internal` (§D.7).
pub const CODE_GATEWAY_INTERNAL: &str = "gateway/internal";
/// `resync-required` (§D.7): follow stream must be re-opened from a fresh
/// snapshot (L5 companion of `StreamEndReason::Resync`).
pub const CODE_RESYNC_REQUIRED: &str = "resync-required";
/// `model/unresolvable` (§D.7): a [`ModelRef`](crate::journal::ModelRef) did
/// not resolve server-side (the single convergence point is
/// `resolve_model_ref`, L8).
pub const CODE_MODEL_UNRESOLVABLE: &str = "model/unresolvable";

impl RpcError {
    /// Builder: tag this error with a §D.7 stable code (stored in
    /// `data.code`).
    pub fn with_code(self, code: &'static str) -> Self {
        Self {
            data: Some(serde_json::json!({ "code": code })),
            ..self
        }
    }
}

/// Client → server message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
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
    /// Open a server→client stream (§D.1): the server answers with
    /// [`FromServer::StreamItem`] frames and exactly one terminal
    /// [`FromServer::StreamEnd`]. `stream_id` is client-minted, unique per
    /// connection. The kind field is `streamKind` on the wire — the §D.1
    /// field name `kind` would collide with this enum's internal `kind` tag
    /// (same envelope-key exclusivity rule as §C.1).
    StreamOpen {
        stream_id: crate::journal::StreamId,
        stream_kind: crate::stream::StreamKind,
    },
    /// Cancel a live stream (§D.1); the server closes it with
    /// `StreamEnd { reason: Cancelled }`.
    StreamCancel { stream_id: crate::journal::StreamId },
}

/// Server → client message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
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
    /// Streaming update. v2 (§D.5) re-types this payload as
    /// [`HostEvent`](crate::stream::HostEvent) — the global host vocabulary
    /// that replaces the doomed `ServerNote` domain arms; the swap waits on
    /// the consumer migration (see module docs).
    Notification { note: crate::server::ServerNote },
    /// v2 host event (§D.5): global, change-driven broadcasts
    /// (`SessionStatus` deltas, `Models`/`Commands` refresh pushes, …)
    /// addressed to every connected client, not to a session's owners.
    Host { host: crate::stream::HostEvent },
    /// One frame of a live stream (§D.1). `Snapshot` / `Projections` and the
    /// terminal `StreamEnd` never drop under backpressure (L5 / §D.7);
    /// `Entry` frames ride a bounded queue that resyncs on overflow.
    StreamItem {
        stream_id: crate::journal::StreamId,
        frame: crate::stream::StreamFrame,
    },
    /// The terminal frame of a stream (§D.1): exactly one per opened stream,
    /// never dropped (§D.7). After it the `stream_id` may be reopened.
    StreamEnd {
        stream_id: crate::journal::StreamId,
        reason: crate::stream::StreamEndReason,
    },
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
            FromClient::StreamOpen {
                stream_id: crate::journal::StreamId::new("stream-1"),
                stream_kind: crate::stream::StreamKind::FollowSession {
                    session_id: "s1".into(),
                    max_messages: Some(64),
                },
            },
            FromClient::StreamCancel {
                stream_id: crate::journal::StreamId::new("stream-1"),
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
                note: crate::server::ServerNote::ThreadsUpdated { threads: vec![] },
            },
            FromServer::Notification {
                note: crate::server::ServerNote::Models {
                    models: vec![crate::wire::ModelInfo {
                        id: "deepseek-chat".into(),
                        name: "DeepSeek Chat".into(),
                        provider: "DeepSeek-anthropic".into(),
                        provider_name: None,
                        api: "anthropic".into(),
                        context_window: 131_072,
                        max_tokens: None,
                    }],
                },
            },
            FromServer::Notification {
                note: crate::server::ServerNote::Commands {
                    commands: serde_json::json!([]),
                },
            },
            FromServer::Notification {
                note: crate::server::ServerNote::Error {
                    session_id: Some("s1".into()),
                    message: "boom".into(),
                },
            },
            FromServer::Notification {
                note: crate::server::ServerNote::ModelText {
                    request_id: "r1".into(),
                    text: "delta".into(),
                },
            },
            FromServer::Notification {
                note: crate::server::ServerNote::ModelChatDone {
                    request_id: "r1".into(),
                    stop: Some("end_turn".into()),
                    error: None,
                },
            },
            FromServer::StreamItem {
                stream_id: crate::journal::StreamId::new("stream-1"),
                frame: crate::stream::StreamFrame::Entry {
                    seq: 3,
                    event: crate::journal::JournalWireEvent::AgentTextDelta { s: "tok".into() },
                },
            },
            FromServer::StreamItem {
                stream_id: crate::journal::StreamId::new("stream-2"),
                frame: crate::stream::StreamFrame::Snapshot(crate::surface::snapshot_sample()),
            },
            FromServer::StreamEnd {
                stream_id: crate::journal::StreamId::new("stream-1"),
                reason: crate::stream::StreamEndReason::Resync,
            },
        ];
        for msg in &msgs {
            let json = serde_json::to_string(msg).unwrap();
            let back: FromServer = serde_json::from_str(&json).unwrap();
            assert_eq!(msg, &back, "FromServer serde round-trip failed: {json}");
        }
    }

    /// L12 / §D.8: an unknown journal entry tag must be *tolerable* on the
    /// read side — clients probe the `type` against
    /// [`crate::surface::JOURNAL_ENTRIES`] and drop + log the frame without
    /// erroring the connection (the strict parse is `is_err`, the tolerant
    /// path never panics).
    #[test]
    fn unknown_journal_entry_tag_is_tolerated_not_fatal() {
        let unknown = serde_json::json!({
            "seq": 5,
            "id": "x-1",
            "parentId": null,
            "timestamp": "2026-09-04T00:00:00Z",
            "type": "someFutureEntry"
        });
        // Strict typed parse fails cleanly (an error value, not a panic)…
        assert!(
            serde_json::from_value::<crate::journal::JournalWireEntry>(unknown.clone()).is_err()
        );
        // …and the tolerant client path: the tag is absent from the declared
        // surface, so the frame is dropped + logged; the connection stays up.
        let tag = unknown["type"].as_str().unwrap();
        assert!(!crate::surface::JOURNAL_ENTRIES.contains(&tag));
        // A declared tag, by contrast, parses.
        let known = serde_json::json!({
            "seq": 6,
            "id": "x-2",
            "parentId": null,
            "timestamp": "2026-09-04T00:00:00Z",
            "type": "turnStart"
        });
        assert!(crate::surface::JOURNAL_ENTRIES.contains(&"turnStart"));
        assert!(serde_json::from_value::<crate::journal::JournalWireEntry>(known).is_ok());
    }

    /// §D.7: the v2 stable error-code set.
    #[test]
    fn rpc_error_code_set_is_wired_into_data() {
        let err = RpcError::new(1, "gone").with_code(CODE_SESSION_NOT_FOUND);
        assert_eq!(err.data.as_ref().unwrap()["code"], "session/not-found");
        assert!(RPC_ERROR_CODES.contains(&"resync-required"));
        assert!(RPC_ERROR_CODES.contains(&CODE_MODEL_UNRESOLVABLE));
    }
}
