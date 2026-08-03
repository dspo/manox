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

/// A required provider credential is absent from the environment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("missing credential for {provider}: set {env_var}")]
pub struct MissingCredential {
    /// The environment variable that must be set.
    pub env_var: &'static str,
    /// The provider the credential serves.
    pub provider: &'static str,
}

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

    /// The default registry: credentials from the environment. Fails with a
    /// typed [`MissingCredential`] when any of the three wired protocols has
    /// no API key — a placeholder key is never shipped to the API.
    pub fn from_env() -> Result<Self, MissingCredential> {
        let anthropic_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| MissingCredential {
            env_var: "ANTHROPIC_API_KEY",
            provider: "anthropic",
        })?;
        let openai_key = std::env::var("OPENAI_API_KEY").map_err(|_| MissingCredential {
            env_var: "OPENAI_API_KEY",
            provider: "openai",
        })?;
        let anthropic = Arc::new(
            AnthropicStreamFn::new(anthropic_key).with_base_url(
                std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com".into()),
            ),
        ) as Arc<dyn crate::agent_loop::StreamFn>;
        let completions = Arc::new(CompletionsStreamFn::new(openai_key.clone()).with_base_url(
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into()),
        )) as Arc<dyn crate::agent_loop::StreamFn>;
        let responses = Arc::new(ResponsesStreamFn::new(openai_key).with_base_url(
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into()),
        )) as Arc<dyn crate::agent_loop::StreamFn>;

        let resolver: StreamResolver = Arc::new(move |model: &Model| match model.api.as_str() {
            "anthropic" => Ok(Arc::clone(&anthropic)),
            "openai_completions" => Ok(Arc::clone(&completions)),
            "openai_responses" => Ok(Arc::clone(&responses)),
            other => Err(anyhow::anyhow!(
                "no provider runtime for api {other:?} (wired: anthropic, openai_completions, openai_responses)"
            )),
        });
        Ok(ModelRuntime::new(resolver))
    }

    /// The resolver behind this runtime.
    pub fn resolver(&self) -> StreamResolver {
        Arc::clone(&self.resolver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing API key surfaces as a typed error naming the variable —
    /// never a placeholder key that would be shipped to the API.
    #[test]
    fn from_env_reports_missing_credentials() {
        let _guard = ScopedEnvGuard::clear(&["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]);
        let err = match ModelRuntime::from_env() {
            Err(e) => e,
            Ok(_) => panic!("expected a missing-credential error"),
        };
        assert!(
            err.env_var == "ANTHROPIC_API_KEY" || err.env_var == "OPENAI_API_KEY",
            "expected a credential name, got {err}"
        );
        assert!(err.to_string().contains("missing credential"));
    }

    /// A custom registry (mock/custom runtimes) is unaffected by environment
    /// credentials.
    #[test]
    fn custom_runtime_does_not_need_env_credentials() {
        let _guard = ScopedEnvGuard::clear(&["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]);
        let resolver: StreamResolver = Arc::new(|_| Err(anyhow::anyhow!("no stream in this test")));
        let runtime = ModelRuntime::new(resolver);
        assert!(
            runtime.resolver()(&Model {
                provider: "mock".into(),
                api: "mock".into(),
                id: "mock-1".into(),
                context_window: 1000,
                max_tokens: 100,
                thinking: crate::types::ThinkingKind::None,
                metadata: Default::default(),
            })
            .is_err()
        );
    }

    /// Clears the given env vars for the duration of the test and restores
    /// them on drop.
    struct ScopedEnvGuard {
        restore: Vec<(String, Option<String>)>,
    }

    impl ScopedEnvGuard {
        fn clear(vars: &[&str]) -> Self {
            ScopedEnvGuard {
                restore: vars
                    .iter()
                    .map(|v| {
                        let prev = std::env::var(v).ok();
                        // SAFETY: single-threaded test scope; the guard restores the value on drop.
                        unsafe { std::env::remove_var(v) };
                        (v.to_string(), prev)
                    })
                    .collect(),
            }
        }
    }

    impl Drop for ScopedEnvGuard {
        fn drop(&mut self) {
            for (var, prev) in &self.restore {
                match prev {
                    Some(value) =>
                    // SAFETY: single-threaded test scope; restores the pre-test value.
                    unsafe { std::env::set_var(var, value) },
                    None => unsafe { std::env::remove_var(var) },
                }
            }
        }
    }
}
