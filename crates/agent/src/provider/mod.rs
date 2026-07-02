//! LLM provider integration: parse `~/.config/cx/cx.providers.config.yaml` and
//! build `LanguageModel` implementations per `wire_api`.

pub mod anthropic;
pub mod api_key;
pub mod completions;
pub mod config;
pub mod registry;
pub mod responses;
pub mod sse;

pub use config::{
    CxConfig, EndpointConfig, ModelConfig, ProviderConfig, ProviderModelConfig, ResolvedModel,
    WireApi,
};
pub use api_key::resolve_apikey;
pub use registry::{ProviderRegistry, global as registry_global, init as registry_init};
