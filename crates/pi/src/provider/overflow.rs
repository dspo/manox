//! Context-overflow classification for provider errors.
//!
//! A provider rejecting a request because the serialized input exceeds the
//! model's context window is a deterministic failure: re-sending the identical
//! request can never succeed, and a plain retry would burn the whole attempt
//! budget for nothing. Providers report it with wildly different statuses and
//! message shapes (400 vs 413, structured codes vs free-form prose), so this
//! module maps them all onto [`ProviderError::Overflow`], which the loop layer
//! answers with a single compact-and-retry.

use std::sync::LazyLock;

use regex::{RegexSet, RegexSetBuilder};

use crate::provider::ProviderError;

/// Message shapes meaning "input too large", gathered across providers.
///
/// The patterns are anchored on the digits and structure each provider emits
/// rather than on bare keywords, because the two failure directions are not
/// symmetric: a false positive routes a *retryable* failure into the
/// non-retryable overflow path, spending a compaction and forfeiting the
/// retry budget for a request that would have succeeded, while a false
/// negative merely leaves the user to compact by hand. So `exceeds the
/// maximum` alone is not enough — "temperature exceeds the maximum allowed
/// value" must not match.
static OVERFLOW_PATTERNS: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSetBuilder::new([
        r"prompt is too long",                    // Anthropic token overflow
        r"request_too_large",                     // Anthropic byte-size overflow (413)
        r"input is too long for requested model", // Amazon Bedrock
        r"exceeds the context window",            // OpenAI Completions & Responses
        r"exceeds (?:the )?(?:model'?s )?maximum context length(?: of [\d,]+ tokens?|\s*\([\d,]+\))", // LiteLLM-style proxies
        r"input token count.*exceeds the maximum", // Google Gemini
        r"maximum prompt length is \d+",           // xAI
        r"reduce the length of the messages",      // Groq
        r"maximum context length is \d+ tokens",   // OpenRouter
        r"exceeds (?:the )?maximum allowed input length of [\d,]+ tokens?", // OpenRouter/Poolside
        r"input \(\d+ tokens\) is longer than the model'?s context length \(\d+ tokens\)", // Together AI
        r"exceeds the limit of \d+",           // GitHub Copilot
        r"exceeds the available context size", // llama.cpp
        r"greater than the context length",    // LM Studio
        r"context window exceeds limit",       // MiniMax
        r"exceeded model token limit",         // Kimi For Coding
        r"too large for model with \d+ maximum context length", // Mistral
        r"prompt has [\d,]+ tokens?, but the configured context size is [\d,]+ tokens?", // DS4
        r"model_context_window_exceeded",      // z.ai
        r"prompt too long; exceeded (?:max )?context length", // Ollama
        r"range of input length should be",    // DashScope / Qwen
        r"context[_ ]length[_ ]exceeded",
        r"too many tokens",
        r"token limit exceeded",
        r"^4(?:00|13)\s*(?:status code)?\s*\(no body\)", // Cerebras: status line, empty body
    ])
    .case_insensitive(true)
    .build()
    .expect("overflow patterns are valid regexes")
});

/// Message shapes that look overflow-adjacent but mean throttling, quota, or
/// billing — checked first so a transient limit is never misrouted into the
/// non-retryable overflow path.
///
/// Deliberately broader than the TS set. TS anchors its Bedrock exclusion on
/// `^Throttling error:`, prose its own `formatBedrockError` produces; this
/// crate ships no Bedrock adapter and so sees the raw AWS body
/// `ThrottlingException: Too many tokens, please wait before trying again.`,
/// which fails that anchor while matching `too many tokens`. Keeping the
/// unanchored fragments is what stops the false positive the TS pattern set
/// only avoids by virtue of its normalizing layer.
static EXCLUSION_PATTERNS: LazyLock<RegexSet> = LazyLock::new(|| {
    RegexSetBuilder::new([
        r"^(?:throttling error|service unavailable):",
        r"rate[ _]limit",
        r"too many requests",
        r"throttl",
        r"please wait",
        r"quota",
        r"insufficient",
    ])
    .case_insensitive(true)
    .build()
    .expect("exclusion patterns are valid regexes")
});

/// Whether a provider failure describes a context-window overflow.
///
/// A 413 short-circuits on the status alone: the status is stronger evidence
/// than any body match, and an empty-bodied 413 carries no text to match.
pub fn classify(status: Option<u16>, body: &str) -> bool {
    if EXCLUSION_PATTERNS.is_match(body) {
        return false;
    }
    if status == Some(413) {
        return true;
    }
    OVERFLOW_PATTERNS.is_match(body)
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

/// Whether an assistant message signals a context-window overflow, mirroring
/// the TS `isContextOverflow` three-case check:
///
/// 1. Error-based: `Error` stop reason whose message classifies as overflow.
/// 2. Silent: a completed response whose reported input (plus cache reads)
///    exceeds the window — some providers accept oversized input silently.
/// 3. Length-stop: the server truncated the input to fill the window,
///    leaving no room for output (`Length` stop, zero output tokens, input
///    at ≥99% of the window).
pub fn is_context_overflow(message: &crate::types::AgentMessage, context_window: u64) -> bool {
    let crate::types::AgentMessage::Assistant {
        stop_reason,
        error_message,
        usage,
        ..
    } = message
    else {
        return false;
    };

    if *stop_reason == Some(crate::types::StopReason::Error)
        && error_message
            .as_deref()
            .is_some_and(|msg| classify(None, msg))
    {
        return true;
    }

    if context_window > 0 {
        let input_tokens = usage.input_tokens + usage.cache_read_input_tokens;
        if *stop_reason == Some(crate::types::StopReason::Stop) && input_tokens > context_window {
            return true;
        }
        if *stop_reason == Some(crate::types::StopReason::Length)
            && usage.output_tokens == 0
            && input_tokens >= context_window * 99 / 100
        {
            return true;
        }
    }

    false
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
    fn keyword_only_matches_are_not_overflow() {
        // Each of these contains a fragment an unanchored keyword set would
        // match, in a message that has nothing to do with the context window.
        for body in [
            "invalid temperature: exceeds the maximum allowed value",
            "the requested image size exceeds the limit of this endpoint",
            "please reduce the length of your model name",
            "payload too large: attachment exceeds 20MB",
        ] {
            assert!(!classify(Some(400), body), "{body}");
        }
    }

    #[test]
    fn provider_shapes_with_digits_are_overflow() {
        for body in [
            // xAI
            "This model's maximum prompt length is 131072 but the request contains 537812 tokens",
            // Together AI
            "The input (265330 tokens) is longer than the model's context length (262144 tokens).",
            // DS4
            "Prompt has 5000 tokens, but the configured context size is 4096 tokens",
            // Mistral
            "Prompt contains 9001 tokens, too large for model with 8192 maximum context length",
            // LiteLLM-style proxy
            "Requested token count exceeds the model's maximum context length of 131072 tokens",
            // OpenAI-compatible parenthesized form
            "Input length (265330) exceeds model's maximum context length (262144).",
            // Google Gemini
            "The input token count (1196265) exceeds the maximum number of tokens allowed (1048575)",
            // OpenRouter/Poolside
            "Input length 300000 exceeds the maximum allowed input length of 262144 tokens.",
            // GitHub Copilot
            "prompt token count of 150000 exceeds the limit of 128000",
            // Kimi For Coding
            "Your request exceeded model token limit: 131072 (requested: 200000)",
            // llama.cpp
            "the request exceeds the available context size, try increasing it",
            // LM Studio
            "tokens to keep from the initial prompt is greater than the context length",
            // MiniMax
            "invalid params, context window exceeds limit",
            // Ollama
            "prompt too long; exceeded max context length by 4096 tokens",
            // Amazon Bedrock
            "Input is too long for requested model",
        ] {
            assert!(classify(Some(400), body), "{body}");
        }
    }

    #[test]
    fn cerebras_empty_body_status_line_is_overflow_only_when_anchored() {
        assert!(classify(Some(400), "400 status code (no body)"));
        assert!(classify(Some(400), "413 (no body)"));
        // The same text mid-sentence describes an upstream hop, not this
        // request's input size.
        assert!(!classify(
            Some(500),
            "got 400 status code (no body) from upstream"
        ));
    }

    #[test]
    fn raw_bedrock_throttling_is_not_overflow() {
        // Without a Bedrock adapter the body arrives unnormalized, so the
        // anchored `^Throttling error:` pattern TS relies on does not fire —
        // the unanchored fragments are what keep this out of the overflow
        // path. This is the case that pins the broader exclusion set.
        let body = "ThrottlingException: Too many tokens, please wait before trying again.";
        assert!(!classify(Some(429), body));
    }

    #[test]
    fn service_unavailable_prefix_is_not_overflow() {
        assert!(!classify(
            Some(503),
            "Service unavailable: too many tokens in flight"
        ));
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

    fn assistant(
        stop_reason: crate::types::StopReason,
        error_message: Option<&str>,
        usage: crate::types::Usage,
    ) -> crate::types::AgentMessage {
        crate::types::AgentMessage::Assistant {
            content: Vec::new(),
            model: "m".into(),
            provider: "p".into(),
            api: "a".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(stop_reason),
            usage: Box::new(usage),
            error_message: error_message.map(str::to_string),
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn error_message_overflow_is_detected() {
        let msg = assistant(
            crate::types::StopReason::Error,
            Some("prompt is too long: 213462 tokens > 200000 maximum"),
            crate::types::Usage::default(),
        );
        assert!(is_context_overflow(&msg, 200_000));
    }

    #[test]
    fn rate_limit_error_is_not_overflow() {
        let msg = assistant(
            crate::types::StopReason::Error,
            Some("rate limit exceeded, please retry later"),
            crate::types::Usage::default(),
        );
        assert!(!is_context_overflow(&msg, 200_000));
    }

    #[test]
    fn silent_overflow_is_detected_from_usage() {
        let msg = assistant(
            crate::types::StopReason::Stop,
            None,
            crate::types::Usage {
                input_tokens: 150_000,
                cache_read_input_tokens: 60_000,
                ..Default::default()
            },
        );
        assert!(is_context_overflow(&msg, 200_000));
        assert!(!is_context_overflow(&msg, 0));
    }

    #[test]
    fn length_stop_with_full_window_and_no_output_is_overflow() {
        let msg = assistant(
            crate::types::StopReason::Length,
            None,
            crate::types::Usage {
                input_tokens: 199_000,
                output_tokens: 0,
                ..Default::default()
            },
        );
        assert!(is_context_overflow(&msg, 200_000));

        // Room left for output means an ordinary length stop, not truncation.
        let msg = assistant(
            crate::types::StopReason::Length,
            None,
            crate::types::Usage {
                input_tokens: 199_000,
                output_tokens: 500,
                ..Default::default()
            },
        );
        assert!(!is_context_overflow(&msg, 200_000));
    }
}
