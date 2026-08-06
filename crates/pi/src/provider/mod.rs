// Provider layer — real `StreamFn` implementations backed by LLM APIs.
//
// Each provider lives in its own submodule with wire types that mirror that
// provider's protocol exactly. Wire types are never shared across providers;
// the cross-provider representation is the domain types in `crate::types`.
// The SSE parser (`sse`) is transport-level and is shared, as are the
// handshake retry loop (`retry`), the context-overflow classifier
// (`overflow`), and the transcript repair (`transform`) — all
// shape-agnostic.

pub mod anthropic;
pub mod openai;
pub mod overflow;
pub mod retry;
pub mod sse;
pub mod transform;

/// Observes each HTTP request attempt of a provider stream — the TS
/// before-payload / after-response hooks. A consumer attaches one via the
/// provider builder (`with_request_observer`) to surface payload and status
/// outside the provider; the harness maps it onto its
/// `BeforeProviderPayload` / `AfterProviderResponse` hook points.
pub trait RequestObserver: Send + Sync {
    /// The payload about to be sent for `model`, `attempt` 1-indexed.
    /// Returning `Some(replacement)` substitutes the payload for this
    /// attempt (the TS before-payload mutation); `None` sends the original.
    fn before_payload(
        &self,
        attempt: u32,
        model: &crate::types::Model,
        payload: &serde_json::Value,
    ) -> Option<serde_json::Value>;

    /// The HTTP status and headers of an attempt's response — success and
    /// retryable statuses both fire.
    fn after_response(&self, attempt: u32, status: u16, headers: &reqwest::header::HeaderMap);
}

use crate::types::{Cost, Model, Usage};

/// The model's rate card from registration metadata (`cost`, USD per 1M
/// tokens per class), when present and non-zero. The kernel never guesses
/// rates: a model registered without pricing (or with an all-zero card)
/// carries no cost.
pub fn model_cost_rates(model: &Model) -> Option<Cost> {
    let card = model.metadata.get("cost")?;
    let rate = |key: &str| card.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let rates = Cost {
        input: rate("input"),
        output: rate("output"),
        cache_read: rate("cacheRead"),
        cache_write: rate("cacheWrite"),
        total: 0.0,
    };
    (rates.input > 0.0 || rates.output > 0.0 || rates.cache_read > 0.0 || rates.cache_write > 0.0)
        .then_some(rates)
}

/// Price a wire usage against a rate card (USD per 1M tokens per class) —
/// the TS pi-ai per-message pricing step. `Cost.total` sums all classes.
pub fn price_usage(rates: &Cost, usage: &Usage) -> Cost {
    let per_million = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
    let input = per_million(usage.input_tokens, rates.input);
    let output = per_million(usage.output_tokens, rates.output);
    let cache_read = per_million(usage.cache_read_input_tokens, rates.cache_read);
    let cache_write = per_million(usage.cache_creation_input_tokens, rates.cache_write);
    Cost {
        input,
        output,
        cache_read,
        cache_write,
        total: input + output + cache_read + cache_write,
    }
}

use thiserror::Error;

/// Errors a provider can surface while streaming.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The API returned a non-2xx status. `body` holds the error envelope.
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },

    /// The request input exceeds the model's context window. Deterministic —
    /// the loop layer answers with compact-and-retry, not a plain retry.
    #[error("context overflow: {0}")]
    Overflow(String),

    /// An SSE frame could not be parsed.
    #[error("malformed sse frame: {line}")]
    Sse { line: String },

    /// A protocol payload failed to deserialize.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// The request was aborted via its cancellation token.
    #[error("aborted")]
    Aborted,

    /// A transport-level failure (connect, timeout, reset).
    #[error("transport error: {0}")]
    Transport(String),

    /// The API signalled an error mid-stream: a 2xx response whose event
    /// stream carries an `{"error": ...}` payload instead of protocol events.
    #[error("provider error mid-stream: {0}")]
    MidStream(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ThinkingKind;
    use serde_json::json;
    use std::collections::HashMap;

    fn model_with_cost(card: Option<serde_json::Value>) -> Model {
        let mut metadata = HashMap::new();
        if let Some(v) = card {
            metadata.insert("cost".to_string(), v);
        }
        Model {
            provider: "p".into(),
            api: "anthropic".into(),
            id: "m".into(),
            context_window: 1000,
            max_tokens: 100,
            thinking: ThinkingKind::None,
            metadata,
        }
    }

    #[test]
    fn rate_card_parses_and_prices_usage() {
        let model = model_with_cost(Some(json!({
            "input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75
        })));
        let rates = model_cost_rates(&model).expect("rate card present");
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_read_input_tokens: 2_000_000,
            cache_creation_input_tokens: 400_000,
            cache_write_1h: None,
            reasoning_tokens: None,
            total_tokens: 0,
            cost: None,
        };
        let cost = price_usage(&rates, &usage);
        assert!((cost.input - 3.0).abs() < 1e-9);
        assert!((cost.output - 1.5).abs() < 1e-9);
        assert!((cost.cache_read - 0.6).abs() < 1e-9);
        assert!((cost.cache_write - 1.5).abs() < 1e-9);
        assert!((cost.total - 6.6).abs() < 1e-9);
    }

    #[test]
    fn missing_or_zero_rate_card_yields_no_cost() {
        assert!(model_cost_rates(&model_with_cost(None)).is_none());
        assert!(
            model_cost_rates(&model_with_cost(Some(json!({
                "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0
            }))))
            .is_none()
        );
    }
}
