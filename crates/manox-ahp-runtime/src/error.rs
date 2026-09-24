//! The runtime's own error type.
//!
//! The AHP adapter reports failures as [`manox_ahp::error::HostError`], mapping a
//! runtime failure by taking the **message**. The runtime therefore needs an
//! error carrying a message and (while v2 lives) a stable wire code — not the v2
//! gateway's `RpcError`, which is the retiring protocol's vocabulary.
//!
//! Keeping its own type is what lets the v2 wire half be deleted without
//! touching the session lifecycle: the lifecycle speaks [`RuntimeError`], and the
//! v2 handlers convert at their own boundary.

/// A failure from the session runtime.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeError {
    /// Human-readable message. This is what the AHP adapter surfaces.
    pub message: String,
    /// The stable machine code, when one applies. The v2 gateway's codes are
    /// carried through unchanged so its clients keep seeing them while it lives;
    /// the AHP side ignores the field.
    pub code: Option<String>,
}

impl RuntimeError {
    /// A failure with a message and no stable code.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: None,
        }
    }

    /// Attach the stable code this failure reports to v2 clients.
    pub fn with_code(mut self, code: &str) -> Self {
        self.code = Some(code.to_string());
        self
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RuntimeError {}

/// Stable error codes the runtime reports.
///
/// These are the v2 gateway's wire codes, kept verbatim: while v2 lives its
/// clients match on them, and translating them now would break those clients for
/// no gain. When v2 is deleted the codes go with it — the AHP surface carries no
/// equivalent and does not need one.
pub mod codes {
    /// No session with that id.
    pub const SESSION_NOT_FOUND: &str = "session/not-found";
    /// Another process (or connection) owns the session's write lease.
    pub const SESSION_ALREADY_OWNED: &str = "session/already-owned";
    /// The request was malformed.
    pub const GATEWAY_BAD_REQUEST: &str = "gateway/bad-request";
    /// An internal invariant failed.
    pub const GATEWAY_INTERNAL: &str = "gateway/internal";
    /// The requested model does not resolve through the provider registry.
    pub const MODEL_UNRESOLVABLE: &str = "model/unresolvable";
}
