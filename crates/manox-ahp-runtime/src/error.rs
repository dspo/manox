//! The runtime's own error type.
//!
//! The AHP adapter reports failures as [`manox_ahp::error::HostError`]; the
//! runtime tells it *why* a call was refused as a typed [`RuntimeErrorKind`],
//! and the adapter maps that kind onto the AHP variants at the seam. The error
//! is not a wire shape: AHP carries no string code, and the message is what
//! reaches the client.

/// Why the runtime refused a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum RuntimeErrorKind {
    /// The runtime failed while serving the call (a broken invariant, IO).
    #[default]
    Backend,
    /// The referenced session, thread or entry does not exist.
    NotFound,
    /// Another process or connection owns the session's write lease.
    AlreadyOwned,
    /// The request is malformed (an unknown model, mode or effort).
    BadRequest,
}

/// A failure from the session runtime.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeError {
    /// Human-readable message. This is what the AHP adapter surfaces.
    pub message: String,
    /// Why the call was refused; the adapter maps it to a `HostError`.
    pub kind: RuntimeErrorKind,
}

impl RuntimeError {
    /// A backend failure with a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: RuntimeErrorKind::Backend,
        }
    }

    /// Mark why this failure refused the call.
    pub fn with_kind(mut self, kind: RuntimeErrorKind) -> Self {
        self.kind = kind;
        self
    }

    /// No such session, thread or entry.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(message).with_kind(RuntimeErrorKind::NotFound)
    }

    /// Another process or connection owns the session's write lease.
    pub fn already_owned(message: impl Into<String>) -> Self {
        Self::new(message).with_kind(RuntimeErrorKind::AlreadyOwned)
    }

    /// The request is malformed.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(message).with_kind(RuntimeErrorKind::BadRequest)
    }
}

impl std::fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RuntimeError {}
