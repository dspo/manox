//! The manox harness streaming-model abstraction.
//!
//! The wire data types (requests, events, usage) live in
//! `agent::language_model` and are re-exported here so harness code keeps
//! one import path; this module adds the `LanguageModel` trait the manox
//! provider stack implements. The pi harness never touches this module.

pub use agent::language_model::*;

use std::sync::Arc;

use futures::{future::BoxFuture, stream::BoxStream};
use gpui::AsyncApp;

use crate::provider::WireApi;

/// Language model abstraction.
pub trait LanguageModel: Send + Sync {
    fn id(&self) -> String;
    /// The wire-facing model id (context-window suffix like `[1m]` and any
    /// `provider/` display prefix stripped). Defaults to [`Self::id`]; the
    /// built-in provider models override it with their resolved `api_model_id`.
    fn api_model_id(&self) -> String {
        self.id()
    }
    fn name(&self) -> String;
    fn provider_id(&self) -> String;
    fn provider_name(&self) -> String;
    fn wire_api(&self) -> WireApi;
    /// The resolved API key this model speaks with. Exposed so wiring layers
    /// (e.g. the pi bridge) can reuse the same credential.
    fn api_key(&self) -> &str;
    /// The provider endpoint URL (without the wire-specific path suffix).
    fn base_url(&self) -> &str;
    /// Per-response output budget in tokens. Providers override; the default
    /// falls back to the context window so wiring layers always get a value.
    fn max_output_tokens(&self) -> u64 {
        self.max_token_count()
    }

    /// cx agent ids this model can drive (`claude` / `codex` / `copilot` / …),
    /// sourced from the provider config's endpoint `agents:` list. Empty means
    /// no external-agent coupling — a plain manox-thread model. The new-session
    /// wizard filters the model list by the chosen agent's id.
    fn visible_agents(&self) -> &[String] {
        &[]
    }

    fn supports_thinking(&self) -> bool {
        false
    }
    fn supports_tools(&self) -> bool {
        true
    }
    /// Whether the model accepts image attachments in user content. Sourced
    /// from the provider config's per-model `supports_images` field (ground
    /// truth, not model self-report). Defaults to `false`; concrete providers
    /// override when the resolved model declares the capability.
    fn supports_images(&self) -> bool {
        false
    }
    /// Whether the provider supports long-lived prompt cache retention
    /// (`cache_control.ttl:"1h"` on Anthropic, `prompt_cache_retention:"24h"`
    /// on OpenAI). Defaults to `false`; concrete providers override based on
    /// the endpoint host (official APIs only).
    fn supports_long_prompt_cache_retention(&self) -> bool {
        false
    }
    fn max_token_count(&self) -> u64;
    /// Auto-compact window override (token count), sourced from the provider
    /// config's provider-level or model-level `env: CLAUDE_CODE_AUTO_COMPACT_WINDOW`.
    /// Only effective on the Anthropic wire. When `Some`, the thread
    /// auto-compacts at 80% of this value (Claude Code parity) instead of the
    /// model's full `max_token_count` at the user's settings threshold. Defaults
    /// to `None`; only Anthropic-wire models whose config sets the env var
    /// override it.
    fn auto_compact_window(&self) -> Option<u64> {
        None
    }

    /// Stream a completion. Returns a `BoxFuture` (handshake) that yields a `BoxStream` of events.
    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        anyhow::Result<BoxStream<'static, anyhow::Result<LanguageModelCompletionEvent>>>,
    >;
}

pub type AnyLanguageModel = Arc<dyn LanguageModel>;
