//! Streaming token-usage accounting for a `Thread`.
//!
//! The provider reports running (monotonic) totals per streaming event. The
//! per-request counter holds the high-water mark; the delta against the previous
//! high-water accrues to the cumulative counter. On every terminal path the
//! in-flight request is attributed to its triggering user message and the
//! per-request counter resets, so the next turn diffs from zero instead of a
//! stale counter.
//!
//! `TokenMeter` is self-contained: the triggering user-message id is passed in
//! by the caller (`Thread`), so this struct holds no reference back to the
//! `Thread` or its message list.

use std::collections::HashMap;

use crate::language_model::TokenUsage;

/// Cumulative + per-request + per-model token accounting, decoupled from `Thread`'s
/// message storage. The owning `Thread` owns one of these and forwards.
#[derive(Default)]
pub struct TokenMeter {
    cumulative: TokenUsage,
    current_request: TokenUsage,
    per_request: HashMap<String, TokenUsage>,
    per_model: HashMap<String, TokenUsage>,
}

impl TokenMeter {
    /// Seed from a persisted `ThreadRecord` on restore: cumulative carries over,
    /// the in-flight counter starts at zero, per-message history is reloaded, and
    /// the per-model breakdown is rehydrated so the env card shows token totals
    /// for a thread the instant it opens — not only after the next stream.
    pub fn restore(
        cumulative: TokenUsage,
        per_request: HashMap<String, TokenUsage>,
        per_model: HashMap<String, TokenUsage>,
    ) -> Self {
        Self {
            cumulative,
            current_request: TokenUsage::default(),
            per_request,
            per_model,
        }
    }

    /// Cumulative usage across the whole thread's life.
    pub fn cumulative(&self) -> TokenUsage {
        self.cumulative
    }

    /// Per-user-message usage, keyed by `Message::id`.
    pub fn per_request(&self) -> &HashMap<String, TokenUsage> {
        &self.per_request
    }

    /// Per-model cumulative usage, keyed by model display name.
    pub fn per_model(&self) -> &HashMap<String, TokenUsage> {
        &self.per_model
    }

    /// Token usage attributed to the last user message, if the provider
    /// reported any for this turn. `user_id` is the owning `Thread`'s last
    /// user-message id (the caller knows the message list, this struct does not).
    pub fn last_request(&self, user_id: Option<&str>) -> Option<TokenUsage> {
        user_id.and_then(|id| self.per_request.get(id).copied())
    }

    /// Fold a streaming `UsageUpdate` into the cumulative and per-request
    /// counters. The API reports running totals (monotonic), so the per-request
    /// counter takes the `max` and the delta against the previous request
    /// counter accrues to the cumulative. Returns the new cumulative so the
    /// caller can emit it without a second read.
    pub fn accumulate(&mut self, new: TokenUsage) -> TokenUsage {
        let delta = self.compute_delta(new);
        self.cumulative = self.cumulative + delta;
        self.current_request = TokenUsage {
            input_tokens: self.current_request.input_tokens.max(new.input_tokens),
            output_tokens: self.current_request.output_tokens.max(new.output_tokens),
            cache_creation_input_tokens: self
                .current_request
                .cache_creation_input_tokens
                .max(new.cache_creation_input_tokens),
            cache_read_input_tokens: self
                .current_request
                .cache_read_input_tokens
                .max(new.cache_read_input_tokens),
        };
        self.cumulative
    }

    /// Same as `accumulate` but also attributes the delta to a specific model.
    pub fn accumulate_for_model(&mut self, new: TokenUsage, model: &str) -> TokenUsage {
        let delta = self.compute_delta(new);
        let cumulative = self.accumulate(new);
        let entry = self.per_model.entry(model.to_owned()).or_default();
        *entry = TokenUsage {
            input_tokens: entry.input_tokens.saturating_add(delta.input_tokens),
            output_tokens: entry.output_tokens.saturating_add(delta.output_tokens),
            cache_creation_input_tokens: entry
                .cache_creation_input_tokens
                .saturating_add(delta.cache_creation_input_tokens),
            cache_read_input_tokens: entry
                .cache_read_input_tokens
                .saturating_add(delta.cache_read_input_tokens),
        };
        cumulative
    }

    /// Delta between a new running-total report and the current high-water mark.
    fn compute_delta(&self, new: TokenUsage) -> TokenUsage {
        let prev = self.current_request;
        TokenUsage {
            input_tokens: new.input_tokens.saturating_sub(prev.input_tokens),
            output_tokens: new.output_tokens.saturating_sub(prev.output_tokens),
            cache_creation_input_tokens: new
                .cache_creation_input_tokens
                .saturating_sub(prev.cache_creation_input_tokens),
            cache_read_input_tokens: new
                .cache_read_input_tokens
                .saturating_sub(prev.cache_read_input_tokens),
        }
    }

    /// Attribute the in-flight request's usage to its triggering user message
    /// and reset the per-request counter. Called on every terminal path —
    /// `Stop` from the provider and `cancel()` from the user — so a cancelled
    /// turn still lands its partial usage and the next turn starts from zero
    /// instead of diffing against a stale counter.
    pub fn finalize_request(&mut self, user_id: Option<&str>) {
        if let Some(uid) = user_id {
            self.per_request
                .insert(uid.to_owned(), self.current_request);
        }
        self.current_request = TokenUsage::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `accumulate` uses max+saturating_sub: the API sends running
    /// totals, the per-request counter takes the high-water, and the delta
    /// accrues to cumulative. `finalize_request` stamps the in-flight request
    /// onto its triggering user message and resets the counter so the next turn
    /// diffs from zero.
    #[test]
    fn accumulate_tracks_running_total_and_resets_on_finalize() {
        let mut m = TokenMeter::default();

        let u1 = TokenUsage {
            input_tokens: 100,
            output_tokens: 10,
            ..Default::default()
        };
        let u2 = TokenUsage {
            input_tokens: 120,
            output_tokens: 40,
            ..Default::default()
        };
        // Non-monotonic jitter: a later event reports a smaller running total.
        let u_jitter = TokenUsage {
            input_tokens: 110,
            output_tokens: 20,
            ..Default::default()
        };

        m.accumulate(u1);
        assert_eq!(m.cumulative, u1);
        assert_eq!(m.current_request, u1);

        // Second monotonic update: delta = u2 - u1, cumulative = u2.
        m.accumulate(u2);
        assert_eq!(m.cumulative, u2);
        assert_eq!(m.current_request, u2);

        // Non-monotonic: delta = 0 (saturating_sub), high-water stays u2.
        m.accumulate(u_jitter);
        assert_eq!(m.cumulative, u2);
        assert_eq!(m.current_request, u2);

        // Finalize: stamps u2 (the high-water) onto the user message and
        // resets the per-request counter.
        m.finalize_request(Some("uid-1"));
        assert_eq!(m.current_request, TokenUsage::default());
        assert_eq!(
            m.per_request.get("uid-1").copied(),
            Some(u2),
            "finalize must stamp the high-water onto the user message"
        );
    }

    /// `accumulate_for_model` tracks per-model deltas independently while
    /// keeping the same cumulative + high-water semantics as `accumulate`.
    #[test]
    fn accumulate_for_model_tracks_per_model_deltas() {
        let mut m = TokenMeter::default();

        // First turn with model A.
        let a1 = TokenUsage {
            input_tokens: 100,
            output_tokens: 10,
            ..Default::default()
        };
        m.accumulate_for_model(a1, "model-a");
        assert_eq!(m.per_model.get("model-a").unwrap().input_tokens, 100);
        assert_eq!(m.per_model.get("model-a").unwrap().output_tokens, 10);

        // Second update still model A: delta = 50 in, 20 out.
        let a2 = TokenUsage {
            input_tokens: 150,
            output_tokens: 30,
            ..Default::default()
        };
        m.accumulate_for_model(a2, "model-a");
        assert_eq!(m.per_model.get("model-a").unwrap().input_tokens, 150);
        assert_eq!(m.per_model.get("model-a").unwrap().output_tokens, 30);

        // Finalize and start a new turn with model B.
        m.finalize_request(Some("uid-1"));
        let b1 = TokenUsage {
            input_tokens: 200,
            output_tokens: 50,
            ..Default::default()
        };
        m.accumulate_for_model(b1, "model-b");
        assert_eq!(m.per_model.get("model-b").unwrap().input_tokens, 200);
        assert_eq!(m.per_model.get("model-b").unwrap().output_tokens, 50);

        // Model A totals are preserved.
        assert_eq!(m.per_model.get("model-a").unwrap().input_tokens, 150);

        // Cumulative tracks the overall total.
        assert_eq!(m.cumulative.input_tokens, 350);
        assert_eq!(m.cumulative.output_tokens, 80);
    }
}
