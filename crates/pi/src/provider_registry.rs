//! Provider registration — the Rust counterpart of the TS
//! `ExtensionAPI.registerProvider` seam (`packages/coding-agent`
//! `provider-composer`). Extensions describe a provider declaratively
//! (endpoint, credential, wire protocol, model catalog) and the registry
//! turns that description into `StreamFn` runtimes plus a global model
//! index the host can list and resolve.
//!
//! The registry is deliberately config-shape agnostic: parsing a concrete
//! config format (e.g. the native cx providers yaml) is the extension's
//! job (see `pi_extensions::provider`), mirroring how TS extensions own
//! their own config schemas and only hand the kernel a `ProviderConfig`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::agent_loop::{StreamFn, StreamResolver};
use crate::coding_agent::model_runtime::{DefaultModelCatalog, ModelCatalog};
use crate::provider::anthropic::AnthropicStreamFn;
use crate::provider::openai::completions::CompletionsStreamFn;
use crate::provider::openai::responses::ResponsesStreamFn;
use crate::types::{Model, StreamOptions, ThinkingKind};

/// The wire protocol a provider or model speaks — the TS `Api` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    /// Anthropic Messages (`/v1/messages`).
    AnthropicMessages,
    /// OpenAI Chat Completions (`/chat/completions`).
    OpenAiCompletions,
    /// OpenAI Responses (`/responses`).
    OpenAiResponses,
}

impl Api {
    /// The TS wire name used by `registerProvider` configs.
    pub fn as_ts_str(self) -> &'static str {
        match self {
            Api::AnthropicMessages => "anthropic-messages",
            Api::OpenAiCompletions => "openai-completions",
            Api::OpenAiResponses => "openai-responses",
        }
    }

    /// The `Model.api` discriminator the Rust harness and its
    /// `StreamResolver` implementations use.
    pub fn as_model_api(self) -> &'static str {
        match self {
            Api::AnthropicMessages => "anthropic",
            Api::OpenAiCompletions => "openai_completions",
            Api::OpenAiResponses => "openai_responses",
        }
    }

    /// Parse the TS wire name.
    pub fn from_ts_str(s: &str) -> Option<Self> {
        match s {
            "anthropic-messages" => Some(Api::AnthropicMessages),
            "openai-completions" => Some(Api::OpenAiCompletions),
            "openai-responses" => Some(Api::OpenAiResponses),
            _ => None,
        }
    }
}

/// Per-million-token cost rates — the TS `Model.cost` shape.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// An input modality a model accepts — the TS `("text" | "image")[]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputModality {
    Text,
    Image,
}

/// One model in a provider registration — the TS `ProviderModelConfig`
/// (minus `thinkingLevelMap`/`headers`/`compat`, which the Rust providers
/// do not consume yet).
#[derive(Debug, Clone)]
pub struct ProviderModelConfig {
    /// Wire-facing model id (e.g. `claude-sonnet-4-6`).
    pub id: String,
    /// Display name for UIs.
    pub name: String,
    /// Whether the model supports reasoning/thinking.
    pub reasoning: bool,
    /// Accepted input modalities.
    pub input: Vec<InputModality>,
    /// Context window in tokens.
    pub context_window: u64,
    /// Maximum output tokens per response.
    pub max_tokens: u64,
    /// Cost rates (USD per 1M tokens).
    pub cost: Cost,
    /// Per-model protocol override (falls back to the provider's `api`).
    pub api: Option<Api>,
    /// Per-model endpoint override (falls back to the provider's `base_url`).
    pub base_url: Option<String>,
    /// Agent visibility allow-list (cx domain; empty = visible to all).
    /// Not part of the TS shape — carried into `Model.metadata["agents"]`
    /// so host UIs can filter without a second config read.
    pub agents: Vec<String>,
    /// The raw config key this model was registered from (e.g. with a
    /// `[1m]` context suffix), when the registering extension wants the
    /// original reference back — carried in `Model.metadata["config_id"]`.
    pub config_id: Option<String>,
}

/// A declarative provider registration — the TS `ProviderConfig` (subset:
/// no `streamSimple`/`refreshModels`/`oauth` yet).
#[derive(Debug, Clone, Default)]
pub struct ProviderConfig {
    /// Display name (defaults to the registration name when absent).
    pub name: Option<String>,
    /// API endpoint base URL. Required when models are defined.
    pub base_url: Option<String>,
    /// API key literal or env interpolation (`$ENV_VAR` / `${ENV_VAR}`),
    /// resolved per request so env changes are tracked.
    pub api_key: Option<String>,
    /// Provider-level protocol; models inherit it unless they override.
    pub api: Option<Api>,
    /// Extra headers merged into every request.
    pub headers: Option<HashMap<String, String>>,
    /// When true, `Authorization: Bearer <resolved key>` is added on top
    /// of the protocol's native key header (TS `authHeader`).
    pub auth_header: bool,
    /// The models to register; replaces any previous models of this
    /// provider on re-registration.
    pub models: Vec<ProviderModelConfig>,
}

/// Registered provider descriptions plus the global model index. The
/// stream resolution order is: registered provider → optional fallback
/// resolver (the host's legacy bridge) → error.
pub struct ProviderRegistry {
    providers: Mutex<HashMap<String, ProviderConfig>>,
    /// Two-level model index: provider name → model id → expanded `Model`.
    /// Keyed per provider on purpose — distinct providers legitimately
    /// register the same model id.
    models: Mutex<HashMap<String, HashMap<String, Model>>>,
    fallback: Mutex<Option<StreamResolver>>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        ProviderRegistry {
            providers: Mutex::new(HashMap::new()),
            models: Mutex::new(HashMap::new()),
            fallback: Mutex::new(None),
        }
    }

    /// Register (or replace) a provider and expand its models into the
    /// global index. Validation mirrors the TS composer: a provider that
    /// defines models needs a `base_url`, and every model must resolve a
    /// protocol from the provider or itself.
    pub fn register_provider(&self, name: &str, config: ProviderConfig) -> Result<(), String> {
        if !config.models.is_empty() {
            if config.base_url.is_none() {
                return Err(format!(
                    "provider {name:?}: base_url is required when defining models"
                ));
            }
            for model in &config.models {
                if model.api.is_none() && config.api.is_none() {
                    return Err(format!(
                        "provider {name:?}, model {:?}: no api at provider or model level",
                        model.id
                    ));
                }
            }
        }

        let display_name = config
            .name
            .clone()
            .unwrap_or_else(|| name.to_string());

        let mut expanded: HashMap<String, Model> = HashMap::new();
        for model in &config.models {
            let api = model.api.or(config.api).expect("validated above");
            let mut metadata = HashMap::new();
            metadata.insert("name".to_string(), json!(model.name));
            metadata.insert("reasoning".to_string(), json!(model.reasoning));
            metadata.insert("provider_display_name".to_string(), json!(display_name));
            metadata.insert(
                "cost".to_string(),
                json!({
                    "input": model.cost.input,
                    "output": model.cost.output,
                    "cacheRead": model.cost.cache_read,
                    "cacheWrite": model.cost.cache_write,
                }),
            );
            metadata.insert("agents".to_string(), json!(model.agents));
            if let Some(config_id) = &model.config_id {
                metadata.insert("config_id".to_string(), json!(config_id));
            }
            metadata.insert(
                "input".to_string(),
                json!(model
                    .input
                    .iter()
                    .map(|m| match m {
                        InputModality::Text => "text",
                        InputModality::Image => "image",
                    })
                    .collect::<Vec<_>>()),
            );
            expanded.insert(
                model.id.clone(),
                Model {
                    provider: name.to_string(),
                    api: api.as_model_api().to_string(),
                    id: model.id.clone(),
                    context_window: model.context_window as usize,
                    max_tokens: model.max_tokens as usize,
                    thinking: if model.reasoning {
                        ThinkingKind::Enabled
                    } else {
                        ThinkingKind::None
                    },
                    metadata,
                },
            );
        }

        self.providers
            .lock()
            .unwrap()
            .insert(name.to_string(), config);
        self.models.lock().unwrap().insert(name.to_string(), expanded);
        Ok(())
    }

    /// Remove a provider and its models (TS `unregisterProvider`).
    pub fn unregister_provider(&self, name: &str) {
        self.providers.lock().unwrap().remove(name);
        self.models.lock().unwrap().remove(name);
    }

    /// The registered provider config, when present.
    pub fn provider_config(&self, name: &str) -> Option<ProviderConfig> {
        self.providers.lock().unwrap().get(name).cloned()
    }

    /// The registered provider names (sorted, for deterministic UI/tests).
    pub fn provider_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.providers.lock().unwrap().keys().cloned().collect();
        names.sort();
        names
    }

    /// Every registered model across providers (sorted by provider, then
    /// id) — the model picker's source.
    pub fn models(&self) -> Vec<Model> {
        let index = self.models.lock().unwrap();
        let mut providers: Vec<&String> = index.keys().collect();
        providers.sort();
        let mut out = Vec::new();
        for provider in providers {
            let models = &index[provider];
            let mut ids: Vec<&String> = models.keys().collect();
            ids.sort();
            for id in ids {
                out.push(models[id].clone());
            }
        }
        out
    }

    /// Resolve a registered model by provider name + model id.
    pub fn resolve_model(&self, provider: &str, id: &str) -> Option<Model> {
        self.models
            .lock()
            .unwrap()
            .get(provider)
            .and_then(|models| models.get(id))
            .cloned()
    }

    /// Resolve the `StreamFn` for a model: look up the model's provider,
    /// pick the protocol (model override → provider level → the model's
    /// own `api`), interpolate the credential per request, and build the
    /// matching provider stream. Unknown providers delegate to the
    /// fallback resolver when one is set.
    pub fn resolve_stream(&self, model: &Model) -> Result<Arc<dyn StreamFn>, anyhow::Error> {
        let config = self.providers.lock().unwrap().get(&model.provider).cloned();
        let Some(config) = config else {
            if let Some(fallback) = self.fallback.lock().unwrap().clone() {
                return fallback(model);
            }
            return Err(anyhow::anyhow!(
                "no provider registered for {:?} (and no fallback resolver)",
                model.provider
            ));
        };

        let model_cfg = config.models.iter().find(|m| m.id == model.id);
        let api = model_cfg
            .and_then(|m| m.api)
            .or(config.api)
            .map(|api| api.as_model_api())
            .unwrap_or(model.api.as_str());
        let base_url = model_cfg
            .and_then(|m| m.base_url.clone())
            .or_else(|| config.base_url.clone())
            .ok_or_else(|| anyhow::anyhow!("provider {:?} has no base_url", model.provider))?;
        let api_key = match &config.api_key {
            Some(raw) => Some(interpolate_env(raw).map_err(|var| {
                anyhow::anyhow!(
                    "provider {:?}: api key references missing env var {var:?}",
                    model.provider
                )
            })?),
            None => None,
        };

        let mut headers: Vec<(String, String)> = config
            .headers
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if config.auth_header
            && let Some(key) = &api_key
        {
            headers.push(("Authorization".to_string(), format!("Bearer {key}")));
        }
        let options = StreamOptions {
            headers,
            ..Default::default()
        };

        let key = api_key.unwrap_or_default();
        let stream: Arc<dyn StreamFn> = match api {
            "anthropic" => Arc::new(
                AnthropicStreamFn::new(key)
                    .with_base_url(base_url)
                    .with_options(options),
            ),
            "openai_completions" => Arc::new(
                CompletionsStreamFn::new(key)
                    .with_base_url(base_url)
                    .with_options(options),
            ),
            "openai_responses" => Arc::new(
                ResponsesStreamFn::new(key)
                    .with_base_url(base_url)
                    .with_options(options),
            ),
            other => {
                return Err(anyhow::anyhow!(
                    "no provider runtime for api {other:?} (wired: anthropic, openai_completions, openai_responses)"
                ));
            }
        };
        Ok(stream)
    }

    /// A `StreamResolver` view over this registry — the seam
    /// `ModelRuntime::with_provider_registry` plugs in.
    pub fn stream_resolver(self: &Arc<Self>) -> StreamResolver {
        let registry = Arc::clone(self);
        Arc::new(move |model: &Model| registry.resolve_stream(model))
    }

    /// Install a fallback resolver for models whose provider is not
    /// registered. Replaces any previous fallback.
    pub fn set_fallback_resolver(&self, resolver: StreamResolver) {
        *self.fallback.lock().unwrap() = Some(resolver);
    }

    /// Drop the fallback resolver.
    pub fn clear_fallback_resolver(&self) {
        *self.fallback.lock().unwrap() = None;
    }

    /// A `ModelCatalog` over the registry's model index, chaining to the
    /// default catalog so built-in providers still resolve on restore.
    pub fn catalog(self: &Arc<Self>) -> Arc<dyn ModelCatalog> {
        Arc::new(RegistryCatalog {
            registry: Arc::clone(self),
        })
    }
}

struct RegistryCatalog {
    registry: Arc<ProviderRegistry>,
}

impl ModelCatalog for RegistryCatalog {
    fn resolve(&self, provider: &str, model_id: &str) -> Option<Model> {
        if let Some(model) = self.registry.resolve_model(provider, model_id) {
            return Some(model);
        }
        // Legacy manox-style provider ids persisted by older sessions:
        // "{wire}:{name}" (e.g. "anthropic:DeepSeek") aliases the
        // registration name "{name}-{wire}" ("DeepSeek-anthropic").
        if let Some((wire, name)) = provider.split_once(':') {
            let aliased = format!("{name}-{wire}");
            if let Some(model) = self.registry.resolve_model(&aliased, model_id) {
                return Some(model);
            }
        }
        DefaultModelCatalog.resolve(provider, model_id)
    }
}

/// Interpolate `$VAR` / `${VAR}` env references in a configured value —
/// the TS `resolveTemplate`. Called per request so environment changes
/// are tracked (TS resolves the api key uncached for the same reason).
pub fn interpolate_env(value: &str) -> Result<String, String> {
    interpolate_env_with(value, &|name| std::env::var(name).ok())
}

/// Interpolation against a caller-supplied lookup (test seam). A bare `$`
/// not followed by a variable reference is kept literally; a missing
/// variable is an error naming the variable.
pub fn interpolate_env_with(
    value: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(idx) = rest.find('$') {
        out.push_str(&rest[..idx]);
        rest = &rest[idx + 1..];
        if let Some(braced) = rest.strip_prefix('{') {
            let end = braced
                .find('}')
                .ok_or_else(|| "unterminated ${ in config value".to_string())?;
            let name = &braced[..end];
            let resolved = lookup(name).ok_or_else(|| name.to_string())?;
            out.push_str(&resolved);
            rest = &braced[end + 1..];
            continue;
        }
        let name_end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        if name_end == 0 {
            // A bare `$` (end of string or no name chars): keep literally.
            out.push('$');
            continue;
        }
        let name = &rest[..name_end];
        let resolved = lookup(name).ok_or_else(|| name.to_string())?;
        out.push_str(&resolved);
        rest = &rest[name_end..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(models: Vec<ProviderModelConfig>) -> ProviderConfig {
        ProviderConfig {
            name: Some("Test".into()),
            base_url: Some("https://test.example".into()),
            api_key: Some("sk-literal".into()),
            api: Some(Api::AnthropicMessages),
            headers: None,
            auth_header: true,
            models,
        }
    }

    fn model_cfg(id: &str) -> ProviderModelConfig {
        ProviderModelConfig {
            id: id.into(),
            name: id.into(),
            reasoning: false,
            input: vec![InputModality::Text],
            context_window: 131_072,
            max_tokens: 8_192,
            cost: Cost::default(),
            api: None,
            base_url: None,
            agents: Vec::new(),
            config_id: None,
        }
    }

    #[test]
    fn register_expands_models_into_index() {
        let registry = ProviderRegistry::new();
        registry
            .register_provider("Test-anthropic", provider(vec![model_cfg("m-1"), model_cfg("m-2")]))
            .unwrap();

        let models = registry.models();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].provider, "Test-anthropic");
        assert_eq!(models[0].api, "anthropic");
        assert_eq!(models[0].context_window, 131_072);
        assert_eq!(
            models[0].metadata.get("provider_display_name").unwrap(),
            "Test"
        );
        assert_eq!(
            registry.resolve_model("Test-anthropic", "m-2").unwrap().id,
            "m-2"
        );
        assert!(registry.resolve_model("Test-anthropic", "nope").is_none());
    }

    #[test]
    fn duplicate_model_ids_across_providers_coexist() {
        let registry = ProviderRegistry::new();
        registry
            .register_provider("A-anthropic", provider(vec![model_cfg("shared")]))
            .unwrap();
        registry
            .register_provider("B-anthropic", provider(vec![model_cfg("shared")]))
            .unwrap();
        assert_eq!(registry.models().len(), 2);
        assert!(registry.resolve_model("A-anthropic", "shared").is_some());
        assert!(registry.resolve_model("B-anthropic", "shared").is_some());
    }

    #[test]
    fn reregister_replaces_models() {
        let registry = ProviderRegistry::new();
        registry
            .register_provider("p", provider(vec![model_cfg("old")]))
            .unwrap();
        registry
            .register_provider("p", provider(vec![model_cfg("new")]))
            .unwrap();
        assert_eq!(registry.models().len(), 1);
        assert_eq!(registry.models()[0].id, "new");
    }

    #[test]
    fn register_validates_base_url_and_api() {
        let registry = ProviderRegistry::new();
        let mut config = provider(vec![model_cfg("m")]);
        config.base_url = None;
        assert!(registry.register_provider("p", config).is_err());

        let mut config = provider(vec![model_cfg("m")]);
        config.api = None;
        assert!(registry.register_provider("p", config).is_err());

        // A model-level api satisfies the requirement.
        let mut config = provider(vec![ProviderModelConfig {
            api: Some(Api::OpenAiResponses),
            ..model_cfg("m")
        }]);
        config.api = None;
        registry.register_provider("p", config).unwrap();
        assert_eq!(registry.models()[0].api, "openai_responses");
    }

    #[test]
    fn resolve_stream_picks_protocol_and_honors_overrides() {
        let registry = ProviderRegistry::new();
        let mut completions_model = model_cfg("chat-model");
        completions_model.api = Some(Api::OpenAiCompletions);
        completions_model.base_url = Some("https://override.example".into());
        completions_model.agents = Vec::new();
        registry
            .register_provider(
                "Test-mixed",
                provider(vec![model_cfg("claude-model"), completions_model]),
            )
            .unwrap();

        let anthropic = registry
            .resolve_stream(&registry.resolve_model("Test-mixed", "claude-model").unwrap())
            .unwrap();
        assert_eq!(anthropic.api(), "anthropic");

        let completions = registry
            .resolve_stream(&registry.resolve_model("Test-mixed", "chat-model").unwrap())
            .unwrap();
        assert_eq!(completions.api(), "openai_completions");
    }

    #[test]
    fn resolve_stream_falls_back_for_unknown_provider() {
        let registry = ProviderRegistry::new();
        let fallback: StreamResolver = Arc::new(|model: &Model| {
            Err(anyhow::anyhow!("fallback saw {:?}", model.provider))
        });
        registry.set_fallback_resolver(fallback);

        let model = Model {
            provider: "ghost".into(),
            api: "anthropic".into(),
            id: "m".into(),
            context_window: 1,
            max_tokens: 1,
            thinking: ThinkingKind::None,
            metadata: Default::default(),
        };
        let Err(err) = registry.resolve_stream(&model) else {
            panic!("expected fallback error");
        };
        assert!(err.to_string().contains("fallback saw"));

        registry.clear_fallback_resolver();
        assert!(registry.resolve_stream(&model).is_err());
    }

    #[test]
    fn interpolation_resolves_env_shapes() {
        let lookup = |name: &str| -> Option<String> {
            match name {
                "KEY" => Some("secret".into()),
                _ => None,
            }
        };
        assert_eq!(interpolate_env_with("$KEY", &lookup).unwrap(), "secret");
        assert_eq!(
            interpolate_env_with("pre-${KEY}-post", &lookup).unwrap(),
            "pre-secret-post"
        );
        assert_eq!(interpolate_env_with("plain", &lookup).unwrap(), "plain");
        assert_eq!(interpolate_env_with("a$ b", &lookup).unwrap(), "a$ b");
        assert_eq!(
            interpolate_env_with("$MISSING", &lookup).unwrap_err(),
            "MISSING"
        );
        assert!(interpolate_env_with("${KEY", &lookup).is_err());
    }

    #[test]
    fn catalog_chains_to_default_and_aliases_legacy_ids() {
        let registry = Arc::new(ProviderRegistry::new());
        registry
            .register_provider("DeepSeek-anthropic", provider(vec![model_cfg("deepseek-v4-flash")]))
            .unwrap();
        let catalog = registry.catalog();
        assert!(catalog.resolve("DeepSeek-anthropic", "deepseek-v4-flash").is_some());
        // Legacy manox-style provider ids alias onto registration names.
        assert!(catalog.resolve("anthropic:DeepSeek", "deepseek-v4-flash").is_some());
        // Built-in provider ids still resolve through the default catalog.
        assert!(catalog.resolve("anthropic", "claude-sonnet-4-6").is_some());
        assert!(catalog.resolve("DeepSeek-anthropic", "nope").is_none());
    }
}
