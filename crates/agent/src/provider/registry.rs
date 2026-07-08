//! Process-global `LanguageModel` registry.
//!
//! `init(cx)` reads the cx yaml → builds the matching `LanguageModel` for each
//! `ResolvedModel` by `wire_api` → stores them in a global
//! `Vec<Arc<dyn LanguageModel>>`. `list_models` / `get_model` serve the UI and `Thread`.

use std::sync::{Arc, OnceLock};

use gpui::App;

use crate::language_model::AnyLanguageModel;
use crate::provider::anthropic::AnthropicModel;
use crate::provider::completions::CompletionsModel;
use crate::provider::resolve_apikey;
use crate::provider::responses::ResponsesModel;
use crate::provider::{CxConfig, ResolvedModel, WireApi};

/// Global provider registry.
static REGISTRY: OnceLock<ProviderRegistry> = OnceLock::new();

pub struct ProviderRegistry {
    models: Vec<AnyLanguageModel>,
}

impl ProviderRegistry {
    /// Build models from the default cx config, resolving each api_key.
    /// Models whose api_key fails to resolve are skipped (warn-logged, never blocking the rest).
    pub fn from_config(config: CxConfig) -> Self {
        let mut models: Vec<AnyLanguageModel> = Vec::new();
        for resolved in config.resolve_all_models() {
            match build_model(&resolved) {
                Ok(m) => models.push(m),
                Err(e) => tracing::warn!(
                    provider = resolved.provider_name.as_str(),
                    model = resolved.id.as_str(),
                    error = %e,
                    "Skipping unresolvable model"
                ),
            }
        }
        Self { models }
    }

    pub fn models(&self) -> &[AnyLanguageModel] {
        &self.models
    }

    /// Look up a model by its stable manox id (`provider/model/wire`).
    pub fn get_model(&self, id: &str) -> Option<AnyLanguageModel> {
        self.models.iter().find(|m| m.id() == id).cloned()
    }
}

/// Build a concrete `LanguageModel` from a `ResolvedModel` by `wire_api`. Requires resolving the api_key.
fn build_model(resolved: &ResolvedModel) -> anyhow::Result<AnyLanguageModel> {
    let api_key = resolve_apikey(resolved.apikey_source.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "provider {} has no apikey_source configured",
            resolved.provider_name
        )
    })?)?;
    let max_tokens = parse_max_tokens(&resolved.context);

    let model: AnyLanguageModel = match resolved.wire_api {
        WireApi::Anthropic => Arc::new(AnthropicModel::new(
            resolved.key(),
            resolved.id.clone(),
            resolved.provider_name.clone(),
            resolved.api_model_id(),
            resolved.endpoint_url.clone(),
            api_key,
            max_tokens,
        )),
        WireApi::Responses => Arc::new(ResponsesModel::new(
            resolved.key(),
            resolved.id.clone(),
            resolved.provider_name.clone(),
            resolved.api_model_id(),
            resolved.endpoint_url.clone(),
            api_key,
            max_tokens,
        )),
        WireApi::Completions => Arc::new(CompletionsModel::new(
            resolved.key(),
            resolved.id.clone(),
            resolved.provider_name.clone(),
            resolved.api_model_id(),
            resolved.endpoint_url.clone(),
            api_key,
            max_tokens,
        )),
        WireApi::Unavailable => {
            anyhow::bail!("wire_api {:?} is unavailable", resolved.wire_api)
        }
    };
    Ok(model)
}

/// Parse a context string (e.g. `1m` / `200k` / `8192`) into a token count.
/// Falls back to 8192 when unparseable.
fn parse_max_tokens(context: &str) -> u64 {
    let trimmed = context.trim();
    if trimmed.is_empty() {
        return 8192;
    }
    let (num_part, unit) = match trimmed.chars().last() {
        Some(u) if u.is_ascii_alphabetic() => (&trimmed[..trimmed.len() - 1], Some(u)),
        _ => (trimmed, None),
    };
    let Ok(n) = num_part.parse::<u64>() else {
        return 8192;
    };
    let mult: u64 = match unit {
        Some('k') | Some('K') => 1024,
        Some('m') | Some('M') => 1024 * 1024,
        _ => 1,
    };
    n.saturating_mul(mult)
}

/// Read the cx config, build the registry, and register it globally. Call at App startup.
/// Panics on failure — manox is unusable without an LLM.
pub fn init(_cx: &mut App) {
    let config = CxConfig::load_default()
        .unwrap_or_else(|e| panic!("Failed to load cx providers config: {e}"));
    let registry = ProviderRegistry::from_config(config);
    if registry.models().is_empty() {
        tracing::error!("ProviderRegistry initialized with no available models");
    }
    let _ = REGISTRY.set(registry);
}

/// Returns the global registry. Panics if `init` was not called.
pub fn global() -> &'static ProviderRegistry {
    REGISTRY
        .get()
        .expect("ProviderRegistry not initialized; call agent::init first")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_max_tokens_cases() {
        assert_eq!(parse_max_tokens("1m"), 1024 * 1024);
        assert_eq!(parse_max_tokens("200k"), 200 * 1024);
        assert_eq!(parse_max_tokens("8192"), 8192);
        assert_eq!(parse_max_tokens(""), 8192);
        assert_eq!(parse_max_tokens("garbage"), 8192);
    }
}
