//! Context-overflow classification for provider errors.
//!
//! A provider rejecting a request because the serialized input exceeds the
//! model's context window is a deterministic failure: re-sending the identical
//! request can never succeed, and a plain retry would burn the whole attempt
//! budget for nothing. Providers report it with wildly different statuses and
//! message shapes (400 vs 413, structured codes vs free-form prose), so this
//! module maps them all onto [`ProviderError::Overflow`], which the loop layer
//! answers with a single compact-and-retry.

use crate::provider::ProviderError;

/// Lowercase message fragments meaning "input too large", gathered across
/// providers (Anthropic / OpenAI / Gemini / DashScope / Ollama /
/// llama.cpp-style servers / Bedrock). Plain substrings — no regex dependency.
const OVERFLOW_PATTERNS: &[&str] = &[
    "context_length_exceeded",
    "context length exceeded",
    "prompt is too long",
    "prompt too long",
    "request_too_large",
    "request too large",
    "payload too large",
    // DashScope InvalidParameter: "Range of input length should be [1, N]".
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
pub fn classify(status: Option<u16>, body: &str) -> bool {
    let body = body.to_lowercase();
    if EXCLUSION_PATTERNS.iter().any(|p| body.contains(p)) {
        return false;
    }
    if status == Some(413) {
        return true;
    }
    OVERFLOW_PATTERNS.iter().any(|p| body.contains(p))
}

/// Build the terminal error for a rejected handshake, classifying overflow.
/// Single construction point shared by every wire so the error variant
/// routing downstream is uniform.
pub fn terminal(status: u16, body: String) -> ProviderError {
    if classify(Some(status), &body) {
        ProviderError::Overflow(format!("http {status}: {body}"))
    } else {
        ProviderError::Http { status, body }
    }
}

/// Build the error for a provider-supplied mid-stream failure message,
/// classifying overflow by the message text.
pub fn mid_stream(message: String) -> ProviderError {
    if classify(None, &message) {
        ProviderError::Overflow(message)
    } else {
        ProviderError::MidStream(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashscope_invalid_parameter_is_overflow() {
        // DashScope rejects oversized input with a 400 and this exact
        // InvalidParameter body.
        let body = r#"{"code":"InvalidParameter","message":"<400> InternalError.Algo.InvalidParameter: Range of input length should be [1, 983616]","request_id":"bd41"}"#;
        assert!(classify(Some(400), body));
    }

    #[test]
    fn anthropic_prompt_too_long_is_overflow() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 213462 tokens > 200000 maximum"}}"#;
        assert!(classify(Some(400), body));
    }

    #[test]
    fn openai_context_length_is_overflow() {
        let body = "This model's maximum context length is 128000 tokens. However, your messages resulted in 130001 tokens.";
        assert!(classify(Some(400), body));
    }

    #[test]
    fn status_413_is_overflow_regardless_of_body() {
        assert!(classify(Some(413), ""));
    }

    #[test]
    fn bedrock_throttling_is_not_overflow() {
        // "Too many tokens" alone would match; the throttling context wins.
        let body = "Throttling error: Too many tokens, please wait before trying again.";
        assert!(!classify(Some(400), body));
    }

    #[test]
    fn rate_limit_is_not_overflow() {
        let body = "Rate limit exceeded, please retry later";
        assert!(!classify(Some(429), body));
    }

    #[test]
    fn unrelated_400_is_not_overflow() {
        let body =
            r#"{"error":{"message":"invalid temperature: only 0.6 is allowed for this model"}}"#;
        assert!(!classify(Some(400), body));
    }

    #[test]
    fn classified_errors_carry_the_overflow_variant() {
        let err = terminal(400, "prompt is too long".to_string());
        assert!(matches!(err, ProviderError::Overflow(_)));
        let err = terminal(400, "invalid temperature".to_string());
        assert!(matches!(err, ProviderError::Http { status: 400, .. }));
        let err = mid_stream("Range of input length should be [1, 983616]".to_string());
        assert!(matches!(err, ProviderError::Overflow(_)));
        let err = mid_stream("connection reset by peer".to_string());
        assert!(matches!(err, ProviderError::MidStream(_)));
    }
}
