// OpenAI-compatible providers, one submodule per API shape.
//
// `completions` covers the Chat Completions protocol spoken by OpenAI and
// most OpenAI-compatible endpoints.
//
// Compatibility policy: the request side encodes exactly what the caller
// declared — `ThinkingKind` selects the thinking wire mechanism and effort
// levels pass through unclamped, so an unsupported combination fails at the
// vendor rather than being second-guessed here. The response side parses
// liberally: extension fields that endpoints invented independently
// (`reasoning_content` and its spelling variants, the flat cache-hit
// counters) are deserialized unconditionally — present-when-relevant,
// harmless when absent. Behavior branches on the endpoint only in the two
// helpers below, each a documented protocol requirement of the endpoint.

pub mod completions;

/// Endpoints whose Chat Completions implementation only accepts the legacy
/// `max_tokens` field name; everything else takes `max_completion_tokens`.
fn uses_legacy_max_tokens(provider: &str, base_url: &str) -> bool {
    provider == "moonshotai"
        || provider == "moonshotai-cn"
        || provider == "together"
        || provider == "nvidia"
        || provider == "cloudflare-ai-gateway"
        || provider == "ant-ling"
        || base_url.contains("chutes.ai")
        || base_url.contains("api.moonshot.")
        || base_url.contains("api.together.")
        || base_url.contains("gateway.ai.cloudflare.com")
        || base_url.contains("integrate.api.nvidia.com")
        || base_url.contains("api.ant-ling.com")
}

/// Endpoints whose thinking mode rejects a replayed assistant message that
/// lacks the `reasoning_content` field; an empty string satisfies them.
fn requires_reasoning_content_on_assistant(provider: &str, base_url: &str) -> bool {
    provider == "deepseek" || base_url.contains("deepseek.com")
}

/// `prompt_cache_key` is capped at 64 characters.
fn clamp_cache_key(key: &str) -> String {
    key.chars().take(64).collect()
}
