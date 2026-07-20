//! LLM provider integration: parse `~/.config/cx/cx.providers.config.yaml` (via the
//! shared `cx-providers` crate, single source of truth also consumed by `cx`) and
//! build `LanguageModel` implementations per `wire_api`.

pub mod anthropic;
pub mod anthropic_cache;
pub mod completions;
pub mod overflow;
pub mod registry;
pub mod responses;
pub mod retry;
pub mod sse;

pub use cx_providers::{
    AgentConfig, ApiKeySourceKind, CopilotAuth, CxConfig, EndpointConfig, ModelConfig,
    ProviderConfig, ProviderEndpointDetail, ProviderEndpointSpec, ProviderModelConfig,
    ResolvedModel, WireApi, context_window_from_suffix, parse_context_window, resolve_apikey,
};
pub use registry::{ProviderRegistry, global as registry_global, init as registry_init};

/// Truncate a prompt-cache key to the 64-char limit OpenAI's Responses API
/// enforces (`prompt_cache_key`). Keeps the key stable across turns of the
/// same model so the provider can reuse the cached prefix.
pub fn clamp_prompt_cache_key(key: &str) -> String {
    key.chars().take(64).collect()
}

/// Whether an OpenAI-compatible endpoint is the official `api.openai.com`
/// host (and thus eligible for `prompt_cache_retention:"24h"` and other
/// first-party-only features). Third-party OpenAI-compatible servers are
/// treated conservatively.
pub fn openai_long_ttl(endpoint_url: &str) -> bool {
    match reqwest::Url::parse(endpoint_url) {
        Ok(u) => u.host_str() == Some("api.openai.com"),
        Err(_) => {
            // Tolerate scheme-less inputs the same way `anthropic_cache::endpoint_host` does.
            let no_scheme = endpoint_url
                .split_once("://")
                .map(|(_, rest)| rest)
                .unwrap_or(endpoint_url);
            let host = no_scheme.split(['/', ':']).next().unwrap_or("");
            host == "api.openai.com"
        }
    }
}

/// Whether an Anthropic-wire model accepts `output_config.effort`. Effort is
/// a 4.6+-era feature (Opus 4.5+, Sonnet 4.6+, Fable 5, Mythos 5); older
/// Claude (3.x / 3.5 / 3.7) and non-Claude models on an Anthropic-compatible
/// wire reject or ignore it, so we gate on the model id family. There is no
/// per-model capability field in `cx-providers` today, so this mirrors the
/// `openai_long_ttl` host-detection precedent: a conservative, derivable
/// heuristic computed once at model construction.
pub fn anthropic_supports_effort(api_model_id: &str) -> bool {
    let m = api_model_id.to_ascii_lowercase();
    m.contains("claude-opus-4")
        || m.contains("claude-sonnet-4-6")
        || m.contains("claude-sonnet-5")
        || m.contains("claude-fable")
        || m.contains("claude-mythos")
}

/// Whether an endpoint + model id pair looks like DeepSeek. Used only for
/// observability (a `tracing::debug` confirming the DeepSeek-aware completions
/// parsing path is active); no behavior is gated on it, because the fields
/// DeepSeek returns (`reasoning_content`, `prompt_cache_hit_tokens`) are parsed
/// unconditionally as optional serde fields — present-when-relevant, harmless
/// when absent, and shared with other OpenAI-compatible reasoning models.
pub fn is_deepseek(endpoint_url: &str, model_id: &str) -> bool {
    let host = reqwest::Url::parse(endpoint_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
    let host_match = host.as_deref().is_some_and(|h| h.contains("deepseek.com"));
    let id_match = model_id.to_ascii_lowercase().contains("deepseek");
    host_match || id_match
}

#[cfg(test)]
mod tests {
    use super::anthropic_supports_effort;

    #[test]
    fn anthropic_supports_effort_for_capable_claude_families() {
        assert!(anthropic_supports_effort("claude-opus-4-8-20250101"));
        assert!(anthropic_supports_effort("claude-opus-4-5"));
        assert!(anthropic_supports_effort("claude-sonnet-4-6"));
        assert!(anthropic_supports_effort("claude-sonnet-5-20250101"));
        assert!(anthropic_supports_effort("claude-fable-5"));
        assert!(anthropic_supports_effort("claude-mythos-preview"));
    }

    #[test]
    fn anthropic_supports_effort_false_for_unsupported_models() {
        // Older Claude (budget_tokens era, no output_config.effort).
        assert!(!anthropic_supports_effort("claude-3-7-sonnet"));
        assert!(!anthropic_supports_effort("claude-3-5-haiku"));
        // Non-Claude models on an Anthropic-compatible wire.
        assert!(!anthropic_supports_effort("glm-5.2"));
        assert!(!anthropic_supports_effort(""));
        // Sonnet 4.5 is not in the documented effort-capable set.
        assert!(!anthropic_supports_effort("claude-sonnet-4-5"));
    }
}
