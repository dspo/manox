//! Connection handshake and capability declaration.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Capabilities a client can answer when the server issues a [`super::ServerCall`].
/// Declared in [`ClientHello`] so the server routes each call only to clients
/// able to fulfil it; a call with no capable owner fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "camelCase")]
pub enum HookKind {
    Approve,
    PlanVerdict,
    AskUserQuestion,
    BrowserOp,
    ClipboardRead,
    OpenExternal,
}

/// First client→server request. Declares who the client is, which
/// [`HookKind`]s it can answer, and which sessions it initially owns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "camelCase")]
pub struct Initialize {
    pub client_id: String,
    pub capabilities: Vec<HookKind>,
    pub sessions: Vec<String>,
}

/// Client identity + capability declaration carried on connect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "protocol.ts")]
#[serde(rename_all = "camelCase")]
pub struct ClientHello {
    pub client_id: String,
    pub capabilities: Vec<HookKind>,
    pub sessions: Vec<String>,
}

impl ClientHello {
    pub fn can(&self, kind: HookKind) -> bool {
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
            capabilities: vec![HookKind::Approve, HookKind::PlanVerdict],
            sessions: vec!["t1".into()],
        };
        assert!(hello.can(HookKind::Approve));
        assert!(!hello.can(HookKind::BrowserOp));
        assert!(hello.owns("t1"));
        assert!(!hello.owns("t2"));
    }

    #[test]
    fn initialize_round_trips() {
        let init = Initialize {
            client_id: "webui".into(),
            capabilities: vec![HookKind::Approve],
            sessions: vec![],
        };
        let json = serde_json::to_string(&init).unwrap();
        let back: Initialize = serde_json::from_str(&json).unwrap();
        assert_eq!(init, back);
    }

    #[test]
    fn hook_kind_serializes_camel_case() {
        let json = serde_json::to_value(HookKind::AskUserQuestion).unwrap();
        assert_eq!(json, serde_json::json!("askUserQuestion"));
    }
}
