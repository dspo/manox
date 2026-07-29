// Provider layer — real `StreamFn` implementations backed by LLM APIs.
//
// Each provider lives in its own submodule with wire types that mirror that
// provider's protocol exactly. Wire types are never shared across providers;
// the cross-provider representation is the domain types in `crate::types`.
// The SSE parser (`sse`) is transport-level and is shared.

pub mod anthropic;
pub mod sse;

use thiserror::Error;

/// Errors a provider can surface while streaming.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The API returned a non-2xx status. `body` holds the error envelope.
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },

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
}
