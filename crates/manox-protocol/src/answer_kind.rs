//! The answerer-capability vocabulary: which [`crate::ServerCall`] kinds a
//! client can adjudicate/answer. Lives in the interaction/answer vocabulary,
//! not the handshake transport layer — the handshake structs in
//! [`crate::handshake`] merely carry it as a declared capability list.

use serde::{Deserialize, Serialize};

/// Capabilities a client can answer when the server issues a [`crate::ServerCall`].
/// Declared in [`crate::handshake::ClientHello`] so the server routes each call
/// only to clients able to fulfil it; a call with no capable owner fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AnswerKind {
    Approve,
    PlanVerdict,
    AskUserQuestion,
    BrowserOp,
    ClipboardRead,
    OpenExternal,
    ClientTool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire tag strings are derived by `#[serde(rename_all = "camelCase")]`
    /// from the variant *names*; the enum's Rust type name (`AnswerKind`) never
    /// appears on the wire. Renaming a variant here is a breaking wire change,
    /// so every tag is pinned to its exact literal below.
    #[test]
    fn answer_kind_wire_tags_are_pinned() {
        let cases = [
            (AnswerKind::Approve, "approve"),
            (AnswerKind::PlanVerdict, "planVerdict"),
            (AnswerKind::AskUserQuestion, "askUserQuestion"),
            (AnswerKind::BrowserOp, "browserOp"),
            (AnswerKind::ClipboardRead, "clipboardRead"),
            (AnswerKind::OpenExternal, "openExternal"),
            (AnswerKind::ClientTool, "clientTool"),
        ];
        for (kind, tag) in cases {
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json, serde_json::json!(tag), "wire tag for {kind:?}");
            let back: AnswerKind = serde_json::from_value(json).unwrap();
            assert_eq!(back, kind);
        }
    }
}
