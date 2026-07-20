//! Context-overflow classification for provider errors.
//!
//! A provider rejecting a request because the serialized input exceeds the
//! model's context window is a *deterministic* failure: re-sending the
//! identical request can never succeed. Providers report it with wildly
//! different statuses and message shapes (400 vs 413, structured codes vs
//! free-form prose), so this module maps them all onto one marker error,
//! [`ContextOverflow`], which `thread.rs` routes to a single
//! compact-and-retry instead of the generic recovery nudge.

use std::fmt;

/// Marker error for "the request input exceeds the model's context window".
/// Wraps the provider's message verbatim so the UI shows the original text.
#[derive(Debug)]
pub struct ContextOverflow(pub String);

impl fmt::Display for ContextOverflow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ContextOverflow {}

/// Lowercase message fragments meaning "input too large", gathered across
/// providers (Anthropic / OpenAI / Gemini / 百炼 / Ollama / llama.cpp-style
/// servers / Bedrock). Kept as plain substrings — no regex dependency.
const OVERFLOW_PATTERNS: &[&str] = &[
    "context_length_exceeded",
    "context length exceeded",
    "prompt is too long",
    "prompt too long",
    "request_too_large",
    "request too large",
    "payload too large",
    // 百炼 InvalidParameter: "Range of input length should be [1, N]"
    "range of input length should be",
    "input length should be",
    "exceeds the context window",
    "exceed the context window",
    "context window exceeded",
    "maximum context length",
    "exceeds the maximum",
    "too many tokens",
    "token limit exceeded",
    "reduce the length",
    "reduce your prompt",
    "input is too long",
    "input too long",
];

/// Lowercase fragments that look overflow-adjacent but mean throttling or
/// quota — checked first so a transient rate limit is never misrouted into
/// the (non-retryable) overflow path. Bedrock throttles with "Too many
/// tokens, please wait", which would otherwise match `too many tokens`.
const EXCLUSION_PATTERNS: &[&str] = &[
    "rate limit",
    "rate_limit",
    "too many requests",
    "throttl",
    "please wait",
    "quota",
    "insufficient",
];

/// Whether a provider failure describes a context-window overflow.
pub fn classify(status: Option<http::StatusCode>, body: &str) -> bool {
    let body = body.to_lowercase();
    if EXCLUSION_PATTERNS.iter().any(|p| body.contains(p)) {
        return false;
    }
    if status == Some(http::StatusCode::PAYLOAD_TOO_LARGE) {
        return true;
    }
    OVERFLOW_PATTERNS.iter().any(|p| body.contains(p))
}

/// Build the terminal error for a rejected request, classifying overflow.
/// Single construction point shared by every wire (anthropic / completions /
/// responses) so the marker type is uniform downstream.
pub fn terminal_error(label: &str, status: http::StatusCode, body: &str) -> anyhow::Error {
    let msg = format!("{label} returned {status}: {body}");
    if classify(Some(status), body) {
        anyhow::Error::new(ContextOverflow(msg))
    } else {
        anyhow::anyhow!(msg)
    }
}

/// Wrap a mid-stream SSE error event, classifying overflow by message text.
pub fn stream_error(message: String) -> anyhow::Error {
    if classify(None, &message) {
        anyhow::Error::new(ContextOverflow(message))
    } else {
        anyhow::anyhow!(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bailian_invalid_parameter_is_overflow() {
        // The incident string: 百炼 rejects oversized input with a 400 and
        // this exact InvalidParameter body.
        let body = r#"event:error
data:{"code":"InvalidParameter","message":"<400> InternalError.Algo.InvalidParameter: Range of input length should be [1, 983616]","request_id":"bd41"}"#;
        assert!(classify(Some(http::StatusCode::BAD_REQUEST), body));
    }

    #[test]
    fn anthropic_prompt_too_long_is_overflow() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 213462 tokens > 200000 maximum"}}"#;
        assert!(classify(Some(http::StatusCode::BAD_REQUEST), body));
    }

    #[test]
    fn openai_context_length_is_overflow() {
        let body = "This model's maximum context length is 128000 tokens. However, your messages resulted in 130001 tokens.";
        assert!(classify(Some(http::StatusCode::BAD_REQUEST), body));
    }

    #[test]
    fn status_413_is_overflow_regardless_of_body() {
        assert!(classify(Some(http::StatusCode::PAYLOAD_TOO_LARGE), ""));
    }

    #[test]
    fn bedrock_throttling_is_not_overflow() {
        // "Too many tokens" alone would match; the throttling context wins.
        let body = "Throttling error: Too many tokens, please wait before trying again.";
        assert!(!classify(Some(http::StatusCode::BAD_REQUEST), body));
    }

    #[test]
    fn rate_limit_is_not_overflow() {
        let body = "Rate limit exceeded, please retry later";
        assert!(!classify(Some(http::StatusCode::TOO_MANY_REQUESTS), body));
    }

    #[test]
    fn unrelated_400_is_not_overflow() {
        let body =
            r#"{"error":{"message":"invalid temperature: only 0.6 is allowed for this model"}}"#;
        assert!(!classify(Some(http::StatusCode::BAD_REQUEST), body));
    }

    #[test]
    fn classified_errors_carry_the_marker_type() {
        let err = terminal_error(
            "Anthropic API",
            http::StatusCode::BAD_REQUEST,
            "prompt is too long",
        );
        assert!(err.downcast_ref::<ContextOverflow>().is_some());
        let err = stream_error("Range of input length should be [1, 983616]".to_string());
        assert!(err.downcast_ref::<ContextOverflow>().is_some());
        let err = stream_error("connection reset by peer".to_string());
        assert!(err.downcast_ref::<ContextOverflow>().is_none());
    }
}
