// Cache miss detection — prompt cache efficiency analysis.
//
// Scans assistant message usage across turns to detect when prompt tokens
// that should have been cache reads (they were in the previous turn's prompt)
// were re-billed as input tokens.

use crate::types::{AgentMessage, Usage};
use serde::{Deserialize, Serialize};

/// Default cache TTL: Anthropic's 5-minute prompt cache retention.
pub const CACHE_TTL_MS: u64 = 5 * 60 * 1000;

/// Ignore cache misses below this token threshold as noise.
const NOISE_FLOOR_TOKENS: u64 = 1024;

/// A detected cache miss on a single assistant message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMiss {
    /// Prompt tokens in the previous turn's prompt but not read from cache.
    pub missed_tokens: u64,
    /// Extra cost paid vs. a full cache hit.
    pub missed_cost: f64,
    /// Milliseconds since the previous request.
    pub idle_ms: u64,
    /// Whether the model changed relative to the previous request.
    pub model_changed: bool,
}

/// Cumulative cache waste across a session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheWasteTotals {
    pub missed_tokens: u64,
    pub missed_cost: f64,
    pub miss_count: usize,
}

/// Pricing lookup for cache cost calculation — cost per token for the
/// protocol's input and cache-read rates.
pub trait ModelPriceSource: Send + Sync {
    /// Input price in USD per token.
    fn input_cost(&self, provider: &str, model_id: &str) -> Option<f64>;
    /// Cache-read price in USD per token.
    fn cache_read_cost(&self, provider: &str, model_id: &str) -> Option<f64>;
}

/// A static price table for the wired protocols' common models, in USD per
/// million tokens. Unknown models resolve to `None` — the caller decides how
/// to treat an unpriceable miss (the miss count and tokens still record).
#[derive(Debug, Clone)]
pub struct StaticModelPrices {
    /// `(provider, model-prefix)` → `(input, cache_read)` per million tokens.
    table: Vec<(String, String, f64, f64)>,
}

impl Default for StaticModelPrices {
    fn default() -> Self {
        StaticModelPrices {
            table: vec![
                ("anthropic".into(), "claude-sonnet-4-6".into(), 3.0, 0.30),
                ("anthropic".into(), "claude-opus-4-8".into(), 15.0, 1.50),
                ("anthropic".into(), "claude-haiku-4-5".into(), 1.0, 0.10),
                ("openai".into(), "gpt-5".into(), 1.25, 0.125),
                ("openai".into(), "gpt-4o".into(), 2.50, 0.30),
                ("openai".into(), "gpt-4.1".into(), 2.00, 0.50),
                ("openai".into(), "o3".into(), 2.00, 0.50),
                ("openai".into(), "o4".into(), 2.00, 0.50),
            ],
        }
    }
}

impl ModelPriceSource for StaticModelPrices {
    fn input_cost(&self, provider: &str, model_id: &str) -> Option<f64> {
        self.lookup(provider, model_id)
            .map(|(input, _)| input / 1_000_000.0)
    }
    fn cache_read_cost(&self, provider: &str, model_id: &str) -> Option<f64> {
        self.lookup(provider, model_id)
            .map(|(_, cache)| cache / 1_000_000.0)
    }
}

impl StaticModelPrices {
    fn lookup(&self, provider: &str, model_id: &str) -> Option<(f64, f64)> {
        let model = model_id.to_lowercase();
        self.table
            .iter()
            .find(|(p, prefix, _, _)| p == provider && model.starts_with(prefix))
            .map(|(_, _, input, cache)| (*input, *cache))
    }
}

/// Structure tracking the previous request for cache miss detection.
#[derive(Debug, Clone)]
struct PreviousRequest {
    prompt_tokens: u64,
    model_key: String,
    timestamp_ms: u64,
    reported_cache: bool,
}

/// Compute cache waste across all assistant messages in a session.
pub fn compute_cache_waste(
    messages: &[AgentMessage],
    _models: &dyn ModelPriceSource,
) -> CacheWasteTotals {
    let mut prev: Option<PreviousRequest> = None;
    let mut totals = CacheWasteTotals::default();

    for msg in messages {
        if let AgentMessage::Assistant {
            usage,
            model,
            provider,
            timestamp,
            ..
        } = msg
        {
            let prompt_tokens = usage.input_tokens
                + usage.cache_read_input_tokens
                + usage.cache_creation_input_tokens;
            if prompt_tokens == 0 {
                continue;
            }

            let miss = detect_miss(
                &prev,
                usage,
                prompt_tokens,
                model,
                provider,
                timestamp.timestamp_millis() as u64,
                _models,
            );
            if let Some(m) = miss {
                totals.missed_tokens += m.missed_tokens;
                totals.missed_cost += m.missed_cost;
                totals.miss_count += 1;
            }

            prev = Some(PreviousRequest {
                prompt_tokens,
                model_key: format!("{provider}/{model}"),
                timestamp_ms: timestamp.timestamp_millis() as u64,
                reported_cache: prev.map(|p| p.reported_cache).unwrap_or(false)
                    || usage.cache_read_input_tokens + usage.cache_creation_input_tokens > 0,
            });
        }
    }

    totals
}

fn detect_miss(
    prev: &Option<PreviousRequest>,
    usage: &Usage,
    prompt_tokens: u64,
    model: &str,
    provider: &str,
    current_timestamp_ms: u64,
    prices: &dyn ModelPriceSource,
) -> Option<CacheMiss> {
    let prev = prev.as_ref()?;

    if usage.cache_read_input_tokens + usage.cache_creation_input_tokens == 0
        && !prev.reported_cache
    {
        return None;
    }

    let missed_tokens = prev
        .prompt_tokens
        .min(prompt_tokens)
        .saturating_sub(usage.cache_read_input_tokens);

    if missed_tokens <= NOISE_FLOOR_TOKENS {
        return None;
    }

    // The miss pays the model's input rate for tokens that would have been
    // cache reads. Unpriceable models still record tokens and count.
    let missed_cost = prices
        .input_cost(provider, model)
        .map(|per_token| missed_tokens as f64 * per_token)
        .unwrap_or(0.0);

    Some(CacheMiss {
        missed_tokens,
        missed_cost,
        idle_ms: current_timestamp_ms.saturating_sub(prev.timestamp_ms),
        model_changed: format!("{provider}/{model}") != prev.model_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A miss under a priced model records the real input-rate cost, not a
    /// placeholder zero.
    #[test]
    fn missed_cost_uses_input_rate_when_priced() {
        let prices = StaticModelPrices::default();
        assert_eq!(
            prices.input_cost("anthropic", "claude-sonnet-4-6").unwrap(),
            3.0 / 1_000_000.0
        );
        let messages = vec![
            crate::types::AgentMessage::Assistant {
                content: vec![],
                model: "claude-sonnet-4-6".into(),
                provider: "anthropic".into(),
                api: "anthropic".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(crate::types::StopReason::Stop),
                usage: Box::new(crate::types::Usage {
                    input_tokens: 10_000,
                    cache_read_input_tokens: 10_000,
                    ..Default::default()
                }),
                error_message: None,
                timestamp: chrono::Utc::now(),
            },
            crate::types::AgentMessage::Assistant {
                content: vec![],
                model: "claude-sonnet-4-6".into(),
                provider: "anthropic".into(),
                api: "anthropic".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(crate::types::StopReason::Stop),
                // No cache read at all despite the prior all-cached prompt.
                usage: Box::new(crate::types::Usage {
                    input_tokens: 12_000,
                    ..Default::default()
                }),
                error_message: None,
                timestamp: chrono::Utc::now(),
            },
        ];
        let totals = compute_cache_waste(&messages, &prices);
        assert_eq!(totals.miss_count, 1);
        assert_eq!(totals.missed_tokens, 12_000);
        assert!(
            totals.missed_cost > 0.0,
            "priced miss must not be zero: {}",
            totals.missed_cost
        );
    }

    #[test]
    fn test_empty_messages() {
        struct NoPrice;
        impl ModelPriceSource for NoPrice {
            fn input_cost(&self, _provider: &str, _model_id: &str) -> Option<f64> {
                None
            }
            fn cache_read_cost(&self, _provider: &str, _model_id: &str) -> Option<f64> {
                None
            }
        }

        let totals = compute_cache_waste(&[], &NoPrice);
        assert_eq!(totals.missed_tokens, 0);
        assert_eq!(totals.miss_count, 0);
    }
}
