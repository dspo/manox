//! Connection handshake and capability declaration.

use serde::{Deserialize, Serialize};

use crate::answer_kind::AnswerKind;

/// The protocol epoch this crate's wire vocabulary belongs to (L12/§D.2:
/// "Initialize carries protocol epoch"). A client declares the highest epoch
/// it speaks in [`Initialize::protocol_epoch`]; the server refuses epochs it
/// cannot interpret with the stable §D.7 code
/// [`CODE_PROTOCOL_UNSUPPORTED_EPOCH`](crate::msg::CODE_PROTOCOL_UNSUPPORTED_EPOCH)
/// and echoes the accepted epoch in `HostEvent::Ready`. Bumping this constant
/// is a deliberate spec revision (L12: names are added at group tails with an
/// epoch bump), never a silent drift.
pub const PROTOCOL_EPOCH: u32 = 6;

/// First client→server request. Declares who the client is, which
/// [`AnswerKind`]s it can answer, and which sessions it initially owns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Initialize {
    pub client_id: String,
    pub capabilities: Vec<AnswerKind>,
    pub sessions: Vec<String>,
    /// The protocol epoch the client speaks (C1, L12). Serde default 0: a v1
    /// client that predates epoch negotiation omits the field and is accepted
    /// at the compat level; the server refuses epochs it does not recognize
    /// (`protocol/unsupported-epoch`, §D.7) and echoes the accepted epoch in
    /// `HostEvent::Ready`.
    #[serde(default)]
    pub protocol_epoch: u32,
}

/// Client identity + capability declaration carried on connect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientHello {
    pub client_id: String,
    pub capabilities: Vec<AnswerKind>,
    pub sessions: Vec<String>,
}

impl ClientHello {
    pub fn can(&self, kind: AnswerKind) -> bool {
        self.capabilities.contains(&kind)
    }

    pub fn owns(&self, session: &str) -> bool {
        self.sessions.iter().any(|s| s == session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_capability_and_ownership() {
        let hello = ClientHello {
            client_id: "gpui-desktop".into(),
            capabilities: vec![AnswerKind::Approve, AnswerKind::PlanVerdict],
            sessions: vec!["t1".into()],
        };
        assert!(hello.can(AnswerKind::Approve));
        assert!(!hello.can(AnswerKind::BrowserOp));
        assert!(hello.owns("t1"));
        assert!(!hello.owns("t2"));
    }

    #[test]
    fn initialize_round_trips() {
        let init = Initialize {
            client_id: "client-a".into(),
            capabilities: vec![AnswerKind::Approve],
            sessions: vec![],
            protocol_epoch: PROTOCOL_EPOCH,
        };
        let json = serde_json::to_string(&init).unwrap();
        let back: Initialize = serde_json::from_str(&json).unwrap();
        assert_eq!(init, back);
    }

    /// C1 (L12 epoch negotiation): a v1 client omits `protocolEpoch`; the
    /// field's serde default is 0 and re-serialization carries it explicitly,
    /// so the server can distinguish "declared 0" from "declared N".
    #[test]
    fn initialize_protocol_epoch_defaults_to_zero() {
        let init: Initialize = serde_json::from_value(serde_json::json!({
            "clientId": "v1-client",
            "capabilities": [],
            "sessions": [],
        }))
        .expect("a v1 Initialize without protocolEpoch parses");
        let back = serde_json::to_value(&init).unwrap();
        assert_eq!(
            back["protocolEpoch"],
            serde_json::json!(0),
            "C1: Initialize carries protocolEpoch with serde default 0"
        );
        let declared: Initialize = serde_json::from_value(serde_json::json!({
            "clientId": "v2-client",
            "capabilities": [],
            "sessions": [],
            "protocolEpoch": 1,
        }))
        .expect("a v2 Initialize with protocolEpoch parses");
        assert_eq!(
            serde_json::to_value(&declared).unwrap()["protocolEpoch"],
            serde_json::json!(1)
        );
    }
}
