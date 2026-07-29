// Provider layer — real `StreamFn` implementations backed by LLM APIs.
//
// Each provider lives in its own submodule with wire types that mirror that
// provider's protocol exactly. Wire types are never shared across providers;
// the cross-provider representation is the domain types in `crate::types`.
// The SSE parser (`sse`) is transport-level and is shared, as are the
// handshake retry loop (`retry`) and the context-overflow classifier
// (`overflow`) — both shape-agnostic.

pub mod anthropic;
pub mod openai;
pub mod overflow;
pub mod retry;
pub mod sse;

use thiserror::Error;

/// Errors a provider can surface while streaming.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The API returned a non-2xx status. `body` holds the error envelope.
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },

    /// The request input exceeds the model's context window. Deterministic —
    /// the loop layer answers with compact-and-retry, not a plain retry.
    #[error("context overflow: {0}")]
    Overflow(String),

    /// An SSE frame could not be parsed.
    #[error("malformed sse frame: {line}")]
    Sse { line: String },

    /// A protocol payload failed to deserialize.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// The request was aborted via its cancellation token.
    #[error("aborted")]
    Aborted,

    /// A transport-level failure (connect, timeout, reset).
    #[error("transport error: {0}")]
    Transport(String),

    /// The API signalled an error mid-stream: a 2xx response whose event
    /// stream carries an `{"error": ...}` payload instead of protocol events.
    #[error("provider error mid-stream: {0}")]
    MidStream(String),
}
