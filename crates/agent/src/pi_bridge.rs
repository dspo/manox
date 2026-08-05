//! Bridge from manox provider configuration into the pi runtime.
//!
//! Maps a manox `LanguageModel` onto a pi `Model` and builds the pi
//! `StreamResolver` that dispatches to the matching pi `StreamFn` with the
//! caller's credential and endpoint. Shared by the `pi_agent` tool (manox
//! harness path) and the `pi-ui` workspace (pi harness path), so the wiring
//! lives in exactly one place.

use std::sync::Arc;

use pi::agent_loop::StreamResolver;
use pi::types::{Model, ThinkingKind};

use crate::language_model::AnyLanguageModel;
use crate::provider::WireApi;

/// Map a manox model onto a pi `Model`.
pub fn map_model(model: &AnyLanguageModel) -> Result<Model, String> {
    let api = match model.wire_api() {
        WireApi::Anthropic => "anthropic",
        WireApi::Completions => "openai_completions",
        WireApi::Responses => "openai_responses",
        WireApi::Unavailable => return Err("model wire is unavailable".to_string()),
    };
    Ok(Model {
        provider: model.provider_id(),
        api: api.to_string(),
        id: model.id(),
        // `max_token_count` is the context window; `max_output_tokens` is the
        // per-response budget that goes on the wire as `max_tokens`. Mapping
        // them the wrong way round would send a 200k/1M max_tokens and 400.
        context_window: model.max_token_count() as usize,
        max_tokens: model.max_output_tokens() as usize,
        thinking: if model.supports_thinking() {
            ThinkingKind::Enabled
        } else {
            ThinkingKind::None
        },
        metadata: Default::default(),
    })
}

/// Build a stream resolver that dispatches by the model's wire API to the
/// matching pi `StreamFn`, seeded with the caller's credential and endpoint.
pub fn stream_resolver(model: &AnyLanguageModel) -> Result<StreamResolver, String> {
    let api_key = model.api_key().to_string();
    let base_url = model.base_url().to_string();
    let wire = model.wire_api();
    Ok(Arc::new(
        move |m: &Model| -> Result<Arc<dyn pi::agent_loop::StreamFn>, anyhow::Error> {
            let stream: Arc<dyn pi::agent_loop::StreamFn> = match wire {
                WireApi::Anthropic => Arc::new(
                    pi::AnthropicStreamFn::new(api_key.clone()).with_base_url(base_url.clone()),
                ),
                WireApi::Completions => Arc::new(
                    pi::CompletionsStreamFn::new(api_key.clone()).with_base_url(base_url.clone()),
                ),
                WireApi::Responses => Arc::new(
                    pi::ResponsesStreamFn::new(api_key.clone()).with_base_url(base_url.clone()),
                ),
                WireApi::Unavailable => return Err(anyhow::anyhow!("model wire is unavailable")),
            };
            let _ = m;
            Ok(stream)
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::{
        LanguageModel as LanguageModelTrait, LanguageModelCompletionEvent, LanguageModelRequest,
    };

    /// A mock model whose window and output budget are deliberately far
    /// apart, so a wrong mapping fails loudly.
    struct MockModel;

    impl LanguageModelTrait for MockModel {
        fn id(&self) -> String {
            "mock-1m".into()
        }
        fn name(&self) -> String {
            "Mock 1M".into()
        }
        fn provider_id(&self) -> String {
            "mock".into()
        }
        fn provider_name(&self) -> String {
            "Mock".into()
        }
        fn wire_api(&self) -> WireApi {
            WireApi::Anthropic
        }
        fn api_key(&self) -> &str {
            "k"
        }
        fn base_url(&self) -> &str {
            "https://mock"
        }
        fn max_token_count(&self) -> u64 {
            1_000_000
        }
        fn max_output_tokens(&self) -> u64 {
            8192
        }
        fn stream_completion(
            &self,
            _request: LanguageModelRequest,
            _cx: &gpui::AsyncApp,
        ) -> futures::future::BoxFuture<
            'static,
            anyhow::Result<
                futures::stream::BoxStream<'static, anyhow::Result<LanguageModelCompletionEvent>>,
            >,
        > {
            use futures::StreamExt as _;
            Box::pin(async move { Ok(futures::stream::iter(std::iter::empty()).boxed()) })
        }
    }

    #[test]
    fn map_model_keeps_window_and_output_budget_apart() {
        let model: AnyLanguageModel = Arc::new(MockModel);
        let pi_model = map_model(&model).unwrap();
        // The context window comes from `max_token_count`; the wire-facing
        // `max_tokens` comes from the output budget. Mapping them the wrong
        // way round would 400 on a real Anthropic call.
        assert_eq!(pi_model.context_window, 1_000_000);
        assert_eq!(pi_model.max_tokens, 8192);
        assert_eq!(pi_model.api, "anthropic");
        assert_eq!(pi_model.id, "mock-1m");
    }
}
