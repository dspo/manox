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

/// Minimal pricing lookup for cache cost calculation.
pub trait ModelPriceSource: Send + Sync {
    fn cache_read_cost(&self, provider: &str, model_id: &str) -> Option<f64>;
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

            let miss = detect_miss(&prev, usage, prompt_tokens, model, provider);
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

    let idle_ms = if prev.timestamp_ms > 0 {
        // We don't have the current message's timestamp in this simplified
        // version — use 0 as placeholder.
        0
    } else {
        0
    };

    Some(CacheMiss {
        missed_tokens,
        missed_cost: 0.0, // Simplified — real implementation needs pricing
        idle_ms,
        model_changed: format!("{provider}/{model}") != prev.model_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_messages() {
        struct NoPrice;
        impl ModelPriceSource for NoPrice {
            fn cache_read_cost(&self, _provider: &str, _model_id: &str) -> Option<f64> {
                None
            }
        }

        let totals = compute_cache_waste(&[], &NoPrice);
        assert_eq!(totals.missed_tokens, 0);
        assert_eq!(totals.miss_count, 0);
    }
}
