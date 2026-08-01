// Model resolution for the coding-agent facade: a small registry that maps a
// model descriptor to a provider runtime, reading credentials from the
// environment. The crate stays registry-free at the harness level; this
// facade owns a concrete registry so `create_agent_session` works out of the
// box.

use std::sync::Arc;

use crate::agent_loop::StreamResolver;
use crate::provider::anthropic::AnthropicStreamFn;
use crate::provider::openai::completions::CompletionsStreamFn;
use crate::provider::openai::responses::ResponsesStreamFn;
use crate::types::Model;

/// Resolves a model's provider runtime. The default registry serves the
/// three selected protocols, taking credentials from env vars
/// (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`); consumers can build their own
/// registry over the same `StreamResolver` seam.
pub struct ModelRuntime {
    resolver: StreamResolver,
}

impl ModelRuntime {
    pub fn new(resolver: StreamResolver) -> Self {
        ModelRuntime { resolver }
    }

    /// The default registry: credentials from the environment.
    pub fn from_env() -> Self {
        let anthropic = Arc::new(
            AnthropicStreamFn::new(
                std::env::var("ANTHROPIC_API_KEY")
                    .unwrap_or_else(|_| "missing-ANTHROPIC_API_KEY".into()),
            )
            .with_base_url(
                std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com".into()),
            ),
        ) as Arc<dyn crate::agent_loop::StreamFn>;
        let completions = Arc::new(
            CompletionsStreamFn::new(
                std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "missing-OPENAI_API_KEY".into()),
            )
            .with_base_url(
                std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
            ),
        ) as Arc<dyn crate::agent_loop::StreamFn>;
        let responses = Arc::new(
            ResponsesStreamFn::new(
                std::env::var("OPENAI_API_KEY").unwrap_or_else(|_| "missing-OPENAI_API_KEY".into()),
            )
            .with_base_url(
                std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
            ),
        ) as Arc<dyn crate::agent_loop::StreamFn>;

        let resolver: StreamResolver = Arc::new(move |model: &Model| match model.api.as_str() {
            "anthropic" => Ok(Arc::clone(&anthropic)),
            "openai_completions" => Ok(Arc::clone(&completions)),
            "openai_responses" => Ok(Arc::clone(&responses)),
            other => Err(anyhow::anyhow!(
                "no provider runtime for api {other:?} (wired: anthropic, openai_completions, openai_responses)"
            )),
        });
        ModelRuntime::new(resolver)
    }

    /// The resolver behind this runtime.
    pub fn resolver(&self) -> StreamResolver {
        Arc::clone(&self.resolver)
    }
}
