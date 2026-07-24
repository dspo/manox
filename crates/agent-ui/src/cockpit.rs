//! Agent cockpit view-model logic: run-phase enum and context-budget
//! estimation. Pure functions over [`agent`] types — no GPUI, no rendering
//! (the workspace reads these and lays them into the "Conversation Info" card).
//!
//! The execution plan overview is no longer inferred here — the model publishes
//! it explicitly through the `UpdatePlan` tool, and the rail consumes the
//! [`agent::PlanSnapshot`] directly.

use std::time::Duration;

use agent::language_model::TokenUsage;

/// Coarse run phase shown in the status row. Derived from `ThreadEvent`s in
/// the workspace's `subscribe_thread` closure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CockpitPhase {
    Idle,
    Thinking,
    Streaming,
    RunningTool,
    AwaitingApproval,
    Summarizing,
    Stopped,
    Failed,
}

/// Collapse a [`CockpitPhase`] into one of three status tags the status-row
/// slider snaps to: `0` 生成中 (Streaming / RunningTool / Summarizing),
/// `1` 思考中 (Thinking), `2` 待输入 (Idle / Stopped / Failed /
/// AwaitingApproval). The slider animates the highlight between these three
/// positions; the eight raw phases never reach the UI directly.
pub fn cockpit_phase_tag(phase: CockpitPhase) -> u8 {
    match phase {
        CockpitPhase::Streaming | CockpitPhase::RunningTool | CockpitPhase::Summarizing => 0,
        CockpitPhase::Thinking => 1,
        CockpitPhase::Idle
        | CockpitPhase::Stopped
        | CockpitPhase::Failed
        | CockpitPhase::AwaitingApproval => 2,
    }
}

/// Share of the context window currently occupied by active tokens.
/// `is_estimate` is always true — provider usage is reported per-request
/// and the window is a soft bound, not a hard limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextBudget {
    /// 0.0–100.0; percent of the context window currently occupied.
    pub used_pct: f64,
    pub is_estimate: bool,
    /// Tokens counted against the window (`active_tokens` of the usage).
    /// UI shows this as the numerator of `used / window`.
    pub active_tokens: u64,
    /// The model's context window size (max_input_tokens) — the real cap,
    /// not the auto-compaction trigger threshold.
    pub cap_tokens: u64,
}

/// Compute the share of the context window currently occupied.
/// `None` only when the window is zero. The percentage is `active / window`;
/// the cap displayed is the real model window size, not the auto-compaction
/// trigger threshold.
pub fn context_budget_pct(
    max_input_tokens: u64,
    active_tokens: u64,
) -> Option<ContextBudget> {
    if max_input_tokens == 0 {
        return None;
    }
    let active = active_tokens as f64;
    let cap = max_input_tokens as f64;
    let used = (active / cap * 100.0).clamp(0.0, 100.0);
    Some(ContextBudget {
        used_pct: used,
        is_estimate: true,
        active_tokens: active as u64,
        cap_tokens: max_input_tokens,
    })
}

/// Compact token-count rendering: `1.1m` / `8.1k` / `512`. Decimal (matches
/// provider billing and the `[k]`/`[m]` window convention).
pub fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}m", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

/// Cache-hit ratio: share of the model's input that was served from the
/// prompt cache rather than re-processed. Denominator is uncached input plus
/// cache-read (cache-creation and output are excluded — they are not "input
/// the model reused"). `None` when the denominator is zero (no input this
/// turn, e.g. first turn before any usage lands).
pub fn cache_read_ratio(usage: TokenUsage) -> Option<f64> {
    let denom = usage
        .input_tokens
        .saturating_add(usage.cache_read_input_tokens);
    if denom == 0 {
        return None;
    }
    Some(usage.cache_read_input_tokens as f64 / denom as f64)
}

/// Compact elapsed-time rendering: `1h 5m` / `10m 30s` / `3s`.
pub fn format_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h >= 1 {
        format!("{h}h {m}m")
    } else if m >= 1 {
        format!("{m}m {sec}s")
    } else {
        format!("{sec}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_budget_pct_basic() {
        // 200k window, 90k active → 45% used.
        let b = context_budget_pct(200_000, 90_000).unwrap();
        assert!((b.used_pct - 45.0).abs() < 0.01);
        assert_eq!(b.cap_tokens, 200_000);
        assert!(b.is_estimate);
    }

    #[test]
    fn context_budget_pct_zero_window() {
        assert_eq!(context_budget_pct(0, 0), None);
    }

    #[test]
    fn context_budget_pct_small_window() {
        // 50k window, 40k active → 80% used.
        let b = context_budget_pct(50_000, 40_000).unwrap();
        assert!((b.used_pct - 80.0).abs() < 0.01);
        assert_eq!(b.cap_tokens, 50_000);
    }

    #[test]
    fn context_budget_pct_clamps_at_100_when_over() {
        let b = context_budget_pct(200_000, 500_000).unwrap();
        assert_eq!(b.used_pct, 100.0);
        assert_eq!(b.cap_tokens, 200_000);
    }

    #[test]
    fn format_tokens_cases() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(512), "512");
        assert_eq!(format_tokens(8_100), "8.1k");
        assert_eq!(format_tokens(22_700), "22.7k");
        assert_eq!(format_tokens(1_100_000), "1.1m");
    }

    #[test]
    fn cockpit_phase_tag_collapses_eight_phases_into_three() {
        assert_eq!(cockpit_phase_tag(CockpitPhase::Streaming), 0);
        assert_eq!(cockpit_phase_tag(CockpitPhase::RunningTool), 0);
        assert_eq!(cockpit_phase_tag(CockpitPhase::Summarizing), 0);
        assert_eq!(cockpit_phase_tag(CockpitPhase::Thinking), 1);
        assert_eq!(cockpit_phase_tag(CockpitPhase::Idle), 2);
        assert_eq!(cockpit_phase_tag(CockpitPhase::Stopped), 2);
        assert_eq!(cockpit_phase_tag(CockpitPhase::Failed), 2);
        assert_eq!(cockpit_phase_tag(CockpitPhase::AwaitingApproval), 2);
    }

    #[test]
    fn format_elapsed_cases() {
        assert_eq!(format_elapsed(Duration::from_secs(3)), "3s");
        assert_eq!(format_elapsed(Duration::from_secs(630)), "10m 30s");
        assert_eq!(format_elapsed(Duration::from_secs(3900)), "1h 5m");
    }

    #[test]
    fn cache_read_ratio_typical_turn() {
        // input=2_797_897, cache_read=13_633_280 → denom=16_431_177,
        // ratio≈0.8298 (83%).
        let usage = TokenUsage {
            input_tokens: 2_797_897,
            output_tokens: 500,
            cache_creation_input_tokens: 1_000,
            cache_read_input_tokens: 13_633_280,
        };
        let r = cache_read_ratio(usage).unwrap();
        assert!((r - 0.8298).abs() < 0.001, "got {r}");
    }

    #[test]
    fn cache_read_ratio_no_cache_read() {
        let usage = TokenUsage {
            input_tokens: 1_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        assert_eq!(cache_read_ratio(usage), Some(0.0));
    }

    #[test]
    fn cache_read_ratio_zero_denominator() {
        // No uncached input and no cache read → None.
        let usage = TokenUsage {
            input_tokens: 0,
            output_tokens: 500,
            cache_creation_input_tokens: 1_000,
            cache_read_input_tokens: 0,
        };
        assert_eq!(cache_read_ratio(usage), None);
    }

    #[test]
    fn cache_read_ratio_all_cached_read() {
        // Pure cache read, no uncached input → ratio 1.0.
        let usage = TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 10_000,
        };
        assert_eq!(cache_read_ratio(usage), Some(1.0));
    }
}
