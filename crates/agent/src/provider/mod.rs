//! LLM provider integration: parse `~/.config/cx/cx.providers.config.yaml` (via the
//! shared `cx-providers` crate, single source of truth also consumed by `cx`) and
//! build `LanguageModel` implementations per `wire_api`.

pub mod anthropic;
pub mod completions;
pub mod registry;
pub mod responses;
pub mod sse;

pub use cx_providers::{
    AgentConfig, ApiKeySourceKind, CopilotAuth, CxConfig, EndpointConfig, ModelConfig,
    ProviderConfig, ProviderEndpointDetail, ProviderEndpointSpec, ProviderModelConfig,
    ResolvedModel, WireApi, resolve_apikey,
};
pub use registry::{ProviderRegistry, global as registry_global, init as registry_init};
