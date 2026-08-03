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
    /// Whether this runtime is the env-backed default registry; only it can
    /// rebuild provider streams with a request observer attached.
    env_backed: bool,
}

impl ModelRuntime {
    pub fn new(resolver: StreamResolver) -> Self {
        ModelRuntime {
            resolver,
            env_backed: false,
        }
    }

    /// The default registry: credentials from the environment. Fails with a
    /// typed [`MissingCredential`] when any of the three wired protocols has
    /// no API key — a placeholder key is never shipped to the API.
    pub fn from_env() -> Self {
        // Credentials resolve lazily per model at resolve time, so a
        // single-provider configuration works without keys for the others.
        // A missing key for the requested model's api surfaces as a typed
        // [`MissingCredential`] — a placeholder key is never shipped.
        let resolver: StreamResolver = Arc::new(move |model: &Model| {
            let base_url = |var: &str, default: &str| {
                std::env::var(var).unwrap_or_else(|_| default.to_string())
            };
            let stream: Arc<dyn crate::agent_loop::StreamFn> = match model.api.as_str() {
                "anthropic" => {
                    let key =
                        std::env::var("ANTHROPIC_API_KEY").map_err(|_| MissingCredential {
                            env_var: "ANTHROPIC_API_KEY",
                            provider: "anthropic",
                        })?;
                    Arc::new(
                        AnthropicStreamFn::new(key).with_base_url(base_url(
                            "ANTHROPIC_BASE_URL",
                            "https://api.anthropic.com",
                        )),
                    )
                }
                "openai_completions" => {
                    let key = std::env::var("OPENAI_API_KEY").map_err(|_| MissingCredential {
                        env_var: "OPENAI_API_KEY",
                        provider: "openai",
                    })?;
                    Arc::new(
                        CompletionsStreamFn::new(key).with_base_url(base_url(
                            "OPENAI_BASE_URL",
                            "https://api.openai.com/v1",
                        )),
                    )
                }
                "openai_responses" => {
                    let key = std::env::var("OPENAI_API_KEY").map_err(|_| MissingCredential {
                        env_var: "OPENAI_API_KEY",
                        provider: "openai",
                    })?;
                    Arc::new(
                        ResponsesStreamFn::new(key).with_base_url(base_url(
                            "OPENAI_BASE_URL",
                            "https://api.openai.com/v1",
                        )),
                    )
                }
                other => {
                    return Err(anyhow::anyhow!(
                        "no provider runtime for api {other:?} (wired: anthropic, openai_completions, openai_responses)"
                    ));
                }
            };
            Ok(stream)
        });
        ModelRuntime {
            resolver,
            env_backed: true,
        }
    }

    /// The resolver behind this runtime.
    pub fn resolver(&self) -> StreamResolver {
        Arc::clone(&self.resolver)
    }

    /// A resolver whose provider streams carry a request observer (the TS
    /// before-payload / after-response hooks). Only the env-backed registry
    /// rebuilds its streams with the observer attached; custom runtimes
    /// (mock/custom providers) resolve their own streams, which do not fire
    /// wire hooks anyway.
    pub fn resolver_with_observer(
        &self,
        observer: Arc<dyn crate::provider::RequestObserver>,
    ) -> StreamResolver {
        if !self.env_backed {
            return Arc::clone(&self.resolver);
        }
        let base_url =
            |var: &str, default: &str| std::env::var(var).unwrap_or_else(|_| default.to_string());
        Arc::new(move |model: &Model| {
            let stream: Arc<dyn crate::agent_loop::StreamFn> = match model.api.as_str() {
                "anthropic" => Arc::new(
                    AnthropicStreamFn::new(std::env::var("ANTHROPIC_API_KEY").map_err(|_| {
                        MissingCredential {
                            env_var: "ANTHROPIC_API_KEY",
                            provider: "anthropic",
                        }
                    })?)
                    .with_base_url(base_url("ANTHROPIC_BASE_URL", "https://api.anthropic.com"))
                    .with_request_observer(Arc::clone(&observer)),
                ),
                "openai_completions" => Arc::new(
                    CompletionsStreamFn::new(std::env::var("OPENAI_API_KEY").map_err(|_| {
                        MissingCredential {
                            env_var: "OPENAI_API_KEY",
                            provider: "openai",
                        }
                    })?)
                    .with_base_url(base_url("OPENAI_BASE_URL", "https://api.openai.com/v1"))
                    .with_request_observer(Arc::clone(&observer)),
                ),
                "openai_responses" => Arc::new(
                    ResponsesStreamFn::new(std::env::var("OPENAI_API_KEY").map_err(|_| {
                        MissingCredential {
                            env_var: "OPENAI_API_KEY",
                            provider: "openai",
                        }
                    })?)
                    .with_base_url(base_url("OPENAI_BASE_URL", "https://api.openai.com/v1"))
                    .with_request_observer(Arc::clone(&observer)),
                ),
                other => {
                    return Err(anyhow::anyhow!(
                        "no provider runtime for api {other:?} (wired: anthropic, openai_completions, openai_responses)"
                    ));
                }
            };
            Ok(stream)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A missing key for the requested model's api surfaces as a typed
    /// error at resolve time — never a placeholder key shipped to the API.
    #[test]
    fn from_env_resolves_credentials_lazily_per_model() {
        let _guard = ScopedEnvGuard::clear(&["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]);
        let runtime = ModelRuntime::from_env();
        // Only the anthropic model is served by this resolver's own closure;
        // the openai key absence errors only when an openai model resolves.
        let err = match runtime.resolver()(&Model {
            provider: "openai".into(),
            api: "openai_responses".into(),
            id: "gpt-5".into(),
            context_window: 1000,
            max_tokens: 100,
            thinking: crate::types::ThinkingKind::None,
            metadata: Default::default(),
        }) {
            Err(e) => e,
            Ok(_) => panic!("expected a missing-credential error"),
        };
        let missing = err.downcast_ref::<MissingCredential>();
        assert_eq!(missing.map(|m| m.env_var), Some("OPENAI_API_KEY"), "{err}");
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
