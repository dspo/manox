//! Agent cockpit view-model logic: milestone parsing, run-phase enum, and
//! context-budget estimation. Pure functions over [`agent`] types — no GPUI,
//! no rendering (the workspace reads these and lays them into the
//! "Conversation info" card).

use std::time::Duration;

use agent::compact::{MIN_COMPACTION_CONTEXT_WINDOW, active_tokens};
use agent::language_model::TokenUsage;

/// Lifecycle of a single plan-derived milestone. `Blocked` / `Completed` /
/// `Failed` are declared for the future model-driven update interface; in v1
/// only `Pending` / `InProgress` are populated by the conservative cockpit
/// logic (never auto-marks a step done — no reliable signal exists).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MilestoneStatus {
    Pending,
    InProgress,
    Blocked { by: Vec<usize> },
    Completed,
    Failed,
}

/// A parsed plan step. The cockpit never reorders; list order is preserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Milestone {
    pub title: String,
    pub status: MilestoneStatus,
}

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

/// Estimated headroom before the context budget is exhausted. `is_estimate` is
/// always true — provider usage is reported per-request and the trigger is a
/// configured threshold, not a hard limit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContextBudget {
    /// 0.0–100.0; percent of the budget still free before auto-summary fires
    /// (or before the raw window fills, when auto-summary is off / window too
    /// small to qualify).
    pub remaining_pct: f64,
    pub is_estimate: bool,
    /// Tokens counted against the budget in the last reported request
    /// (`active_tokens` of the usage). UI shows this as the numerator of
    /// `current / cap`.
    pub active_tokens: u64,
    /// The budget cap the percentage is measured against: the auto-summary
    /// trigger when enabled and the window qualifies, else the raw window.
    pub cap_tokens: u64,
}

/// Parse a plan's markdown body into milestones. Best-effort: each line that
/// opens with an ordered (`1.` / `1)`) or unordered (`-` / `*` / `+`) list
/// marker yields one milestone (marker stripped, remainder is the title).
/// Non-list prose lines are dropped rather than folded in (folding mangles
/// titles). When no list item is found, the whole plan is kept as a single
/// milestone so the panel never regresses to empty. All milestones start
/// `Pending`; the workspace promotes the first to `InProgress` while running.
/// Pseudo-dependency text (e.g. "needs #2") is not parsed into edges.
pub fn parse_milestones(plan_text: &str) -> Vec<Milestone> {
    let mut out: Vec<Milestone> = Vec::new();
    for line in plan_text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some(title) = strip_list_marker(trimmed) else {
            continue;
        };
        let title = title.trim();
        if title.is_empty() {
            continue;
        }
        out.push(Milestone {
            title: title.to_string(),
            status: MilestoneStatus::Pending,
        });
    }
    if out.is_empty() {
        let fallback = plan_text.trim();
        if fallback.is_empty() {
            return Vec::new();
        }
        out.push(Milestone {
            title: fallback.to_string(),
            status: MilestoneStatus::Pending,
        });
    }
    out
}

/// Strip a leading ordered or unordered list marker, returning the remainder.
/// Returns `None` for lines that do not open with a list marker (callers drop
/// them). Pure byte scan — no regex.
fn strip_list_marker(line: &str) -> Option<&str> {
    let b = line.as_bytes();
    if b.is_empty() {
        return None;
    }
    // Ordered: one or more digits followed by '.' or ')'.
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 {
        if i < b.len() && (b[i] == b'.' || b[i] == b')') {
            return Some(&line[i + 1..]);
        }
        return None;
    }
    // Unordered: a single leading '-' / '*' / '+'.
    match b[0] {
        b'-' | b'*' | b'+' => Some(&line[1..]),
        _ => None,
    }
}

/// Estimate the remaining context budget. `None` only when the window is zero
/// — the caller always supplies a usage value (the latest single request's
/// fill), so the indicator never falls back to a placeholder. When auto-summary
/// is enabled and the window qualifies (≥ [`MIN_COMPACTION_CONTEXT_WINDOW`]),
/// the budget is measured against the trigger threshold; otherwise against the
/// raw window. The active token count is the latest request's fill, matching
/// the auto-compaction trigger's notion of "current" usage, so the percentage
/// reads as "share of the context window the latest request occupies".
pub fn context_budget_pct(
    max_input_tokens: u64,
    last_usage: TokenUsage,
    auto_compact_enabled: bool,
    threshold: f64,
) -> Option<ContextBudget> {
    if max_input_tokens == 0 {
        return None;
    }
    let active = active_tokens(last_usage) as f64;
    let cap = max_input_tokens as f64;
    let trigger = if auto_compact_enabled && max_input_tokens >= MIN_COMPACTION_CONTEXT_WINDOW {
        cap * threshold
    } else {
        cap
    };
    if trigger <= 0.0 {
        return None;
    }
    let remaining = ((trigger - active) / trigger * 100.0).clamp(0.0, 100.0);
    Some(ContextBudget {
        remaining_pct: remaining,
        is_estimate: true,
        active_tokens: active as u64,
        cap_tokens: trigger as u64,
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

/// Compact MiB rendering for the request-body budget: `6m` / `1.2m`. Integer
/// magnitudes drop the decimal so the 6 MiB cap reads as `6m` not `6.0m`.
/// Inputs are raw bytes under the 1024² convention used by
/// [`agent::compact::MAX_REQUEST_BODY_BYTES`].
pub fn format_mib(bytes: usize) -> String {
    let m = bytes as f64 / (1024.0 * 1024.0);
    if m.fract() == 0.0 {
        format!("{}m", m as i64)
    } else {
        format!("{:.1}m", m)
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
    fn parse_milestones_ordered_and_unordered() {
        let plan =
            "1. First step\n2. Second step\n- Third step\n* Fourth step\nprose intro\n3) Sixth";
        let ms = parse_milestones(plan);
        let titles: Vec<&str> = ms.iter().map(|m| m.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "First step",
                "Second step",
                "Third step",
                "Fourth step",
                "Sixth"
            ]
        );
        assert!(ms.iter().all(|m| m.status == MilestoneStatus::Pending));
    }

    #[test]
    fn parse_milestones_drops_non_list_prose() {
        let plan = "This plan does X.\n1. Real step\nSome closing remark.";
        let ms = parse_milestones(plan);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].title, "Real step");
    }

    #[test]
    fn parse_milestones_fallback_single_when_no_list() {
        let plan = "Free-form prose plan with no markers.";
        let ms = parse_milestones(plan);
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].title, plan);
    }

    #[test]
    fn parse_milestones_empty_plan() {
        assert!(parse_milestones("").is_empty());
        assert!(parse_milestones("   \n  ").is_empty());
    }

    #[test]
    fn strip_list_marker_cases() {
        assert_eq!(strip_list_marker("1. foo"), Some(" foo"));
        assert_eq!(strip_list_marker("12) bar"), Some(" bar"));
        assert_eq!(strip_list_marker("- baz"), Some(" baz"));
        assert_eq!(strip_list_marker("* qux"), Some(" qux"));
        assert_eq!(strip_list_marker("+ quux"), Some(" quux"));
        assert_eq!(strip_list_marker("plain text"), None);
        assert_eq!(strip_list_marker("2026 plan"), None); // digits, no marker
    }

    #[test]
    fn context_budget_pct_basic() {
        // 200k window, auto-compact on (threshold 0.9 → trigger 180k), 90k active.
        let usage = TokenUsage {
            input_tokens: 90_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        let b = context_budget_pct(200_000, usage, true, 0.9).unwrap();
        // remaining = (180k - 90k)/180k = 50%
        assert!((b.remaining_pct - 50.0).abs() < 0.01);
        assert!(b.is_estimate);
    }

    #[test]
    fn context_budget_pct_zero_window() {
        let usage = TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        assert_eq!(context_budget_pct(0, usage, true, 0.9), None);
    }

    #[test]
    fn context_budget_pct_small_window_uses_raw_cap() {
        // Window < MIN_COMPACTION_CONTEXT_WINDOW (80k): budget vs raw cap, not trigger.
        let usage = TokenUsage {
            input_tokens: 40_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        let b = context_budget_pct(50_000, usage, true, 0.9).unwrap();
        // remaining = (50k - 40k)/50k = 20%
        assert!((b.remaining_pct - 20.0).abs() < 0.01);
    }

    #[test]
    fn context_budget_pct_clamps_at_zero_when_over() {
        let usage = TokenUsage {
            input_tokens: 500_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        let b = context_budget_pct(200_000, usage, true, 0.9).unwrap();
        assert_eq!(b.remaining_pct, 0.0);
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
    fn format_mib_cases() {
        // 6 MiB cap (MAX_REQUEST_BODY_BYTES) renders as "6m", not "6.0m".
        assert_eq!(format_mib(6 * 1024 * 1024), "6m");
        // 1.2 MiB keeps one decimal.
        assert_eq!(format_mib(1_258_291), "1.2m");
        // Zero body → "0m".
        assert_eq!(format_mib(0), "0m");
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
