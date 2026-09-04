//! Session usage aggregation — the port of TS `usage-totals.ts`,
//! `getSessionStats()`, and `getUsageCostBreakdown()`.
//!
//! Totals are derived from the authoritative transcript, never from an
//! incremental ledger: stats stay correct across restore, retries, and
//! compaction by construction.

use crate::session::SessionTreeEntry;
use crate::types::{AgentMessage, ContentBlock, Usage};
use serde::{Deserialize, Serialize};

/// Token + cost totals — the TS `UsageTotals`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Sum of per-message `usage.cost.total` for messages priced at the
    /// wire boundary; unpriced sessions stay `0.0`.
    pub cost: f64,
}

impl UsageTotals {
    pub fn total_tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write
    }
}

pub fn create_usage_totals() -> UsageTotals {
    UsageTotals::default()
}

pub fn add_usage_to_totals(totals: &mut UsageTotals, usage: &Usage) {
    totals.input += usage.input_tokens;
    totals.output += usage.output_tokens;
    totals.cache_read += usage.cache_read_input_tokens;
    totals.cache_write += usage.cache_creation_input_tokens;
    totals.cost += usage.cost.as_ref().map(|c| c.total).unwrap_or(0.0);
}

/// Per-model attributable usage — the TS `UsageCostBreakdownEntry`,
/// carrying the full token-class totals so hosts can render per-model
/// input/cache/output breakdowns (the TS shape exposes only the summed
/// `tokens` + `cost`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelUsageBreakdown {
    /// `{provider}/{response_model or model}` for assistant usage; other
    /// usage (tool results, compaction, branch summaries) buckets into
    /// `Tools/summaries` (TS parity).
    pub key: String,
    pub totals: UsageTotals,
}

impl ModelUsageBreakdown {
    pub fn tokens(&self) -> u64 {
        self.totals.total_tokens()
    }
}

/// Session statistics — the TS `SessionStats` minus `contextUsage` (hosts
/// derive the context budget separately).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub tool_calls: u64,
    pub tool_results: u64,
    pub total_messages: u64,
    pub tokens: UsageTotals,
    pub per_model: Vec<ModelUsageBreakdown>,
}

/// The breakdown bucket for usage that is not attributable to an assistant
/// turn (tool results, compaction and branch-summary calls).
pub const TOOLS_SUMMARIES_KEY: &str = "Tools/summaries";

fn add_to_breakdown(
    per_model: &mut std::collections::HashMap<String, UsageTotals>,
    key: &str,
    usage: &Usage,
) {
    let entry = per_model.entry(key.to_string()).or_default();
    add_usage_to_totals(entry, usage);
}

/// Aggregate [`SessionStats`] over session entries. Counts every message
/// entry and folds in the usage of assistant turns, tool results, and
/// compaction/branch-summary calls alike — totals reflect what was
/// actually billed across the session (TS `getSessionStats` semantics).
pub fn session_stats_from_entries(entries: &[SessionTreeEntry]) -> SessionStats {
    let mut stats = SessionStats::default();
    let mut per_model: std::collections::HashMap<String, UsageTotals> =
        std::collections::HashMap::new();

    for entry in entries {
        match entry {
            SessionTreeEntry::Compaction {
                usage: Some(usage), ..
            }
            | SessionTreeEntry::BranchSummary {
                usage: Some(usage), ..
            } => {
                add_usage_to_totals(&mut stats.tokens, usage);
                add_to_breakdown(&mut per_model, TOOLS_SUMMARIES_KEY, usage);
            }
            SessionTreeEntry::Message { message, .. } => {
                stats.total_messages += 1;
                match message {
                    AgentMessage::User { .. } => {
                        stats.user_messages += 1;
                    }
                    AgentMessage::ToolResult { usage, .. } => {
                        stats.tool_results += 1;
                        if let Some(usage) = usage {
                            add_usage_to_totals(&mut stats.tokens, usage);
                            add_to_breakdown(&mut per_model, TOOLS_SUMMARIES_KEY, usage);
                        }
                    }
                    AgentMessage::Assistant {
                        content,
                        provider,
                        model,
                        response_model,
                        usage,
                        ..
                    } => {
                        stats.assistant_messages += 1;
                        stats.tool_calls += content
                            .iter()
                            .filter(|block| matches!(block, ContentBlock::ToolUse { .. }))
                            .count() as u64;
                        add_usage_to_totals(&mut stats.tokens, usage);
                        let key = format!(
                            "{}/{}",
                            provider,
                            response_model.as_deref().unwrap_or(model.as_str())
                        );
                        add_to_breakdown(&mut per_model, &key, usage);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    let mut breakdown: Vec<ModelUsageBreakdown> = per_model
        .into_iter()
        .map(|(key, totals)| ModelUsageBreakdown { key, totals })
        .collect();
    breakdown.sort_by(|a, b| b.tokens().cmp(&a.tokens()).then_with(|| a.key.cmp(&b.key)));
    stats.per_model = breakdown;
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Cost, StopReason};

    fn usage(input: u64, output: u64, cost_total: Option<f64>) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_write_1h: None,
            reasoning_tokens: None,
            total_tokens: input + output,
            cost: cost_total.map(|total| Cost {
                total,
                ..Default::default()
            }),
        }
    }

    fn assistant(provider: &str, model: &str, usage: Usage) -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: "a".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: AgentMessage::Assistant {
                content: vec![
                    ContentBlock::Text {
                        text: "hi".into(),
                        signature: None,
                    },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "read".into(),
                        input: serde_json::json!({}),
                        thought_signature: None,
                    },
                ],
                model: model.into(),
                provider: provider.into(),
                api: "anthropic".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                stop_reason: Some(StopReason::Stop),
                raw_stop_reason: None,
                usage: Box::new(usage),
                error_message: None,
                timestamp: chrono::Utc::now(),
            },
            origin: None,
        }
    }

    fn user() -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: "u".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: AgentMessage::User {
                content: vec![],
                timestamp: chrono::Utc::now(),
            },
            origin: None,
        }
    }

    #[test]
    fn stats_aggregate_messages_compaction_and_tool_results() {
        let entries = vec![
            user(),
            assistant("p", "m-1", usage(100, 50, Some(0.5))),
            SessionTreeEntry::Message {
                id: "tr".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                message: AgentMessage::ToolResult {
                    tool_call_id: "t1".into(),
                    tool_name: "read".into(),
                    content: vec![],
                    is_error: false,
                    details: None,
                    usage: Some(usage(10, 5, None)),
                    added_tool_names: None,
                    timestamp: chrono::Utc::now(),
                },
                origin: None,
            },
            SessionTreeEntry::Compaction {
                id: "c".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                summary: "s".into(),
                first_kept_entry_id: None,
                tokens_before: 0,
                retained_tail: None,
                usage: Some(usage(20, 10, Some(0.2))),
                details: None,
                from_hook: None,
            },
            SessionTreeEntry::BranchSummary {
                id: "b".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                from_id: "x".into(),
                summary: "s".into(),
                details: None,
                usage: Some(usage(4, 2, None)),
                from_hook: None,
            },
        ];
        let stats = session_stats_from_entries(&entries);
        assert_eq!(stats.user_messages, 1);
        assert_eq!(stats.assistant_messages, 1);
        assert_eq!(stats.tool_results, 1);
        assert_eq!(stats.total_messages, 3);
        assert_eq!(stats.tool_calls, 1);
        // 100+10+20+4 input, 50+5+10+2 output — compacted history counts.
        assert_eq!(stats.tokens.input, 134);
        assert_eq!(stats.tokens.output, 67);
        assert_eq!(stats.tokens.total_tokens(), 201);
        assert!((stats.tokens.cost - 0.7).abs() < 1e-9);
        // Breakdown: assistant key first (larger), summaries bucket second.
        assert_eq!(stats.per_model.len(), 2);
        assert_eq!(stats.per_model[0].key, "p/m-1");
        assert_eq!(stats.per_model[0].tokens(), 150);
        assert_eq!(stats.per_model[0].totals.input, 100);
        assert_eq!(stats.per_model[0].totals.output, 50);
        assert!((stats.per_model[0].totals.cost - 0.5).abs() < 1e-9);
        assert_eq!(stats.per_model[1].key, TOOLS_SUMMARIES_KEY);
        assert_eq!(stats.per_model[1].tokens(), 51);
    }

    #[test]
    fn response_model_wins_the_breakdown_key() {
        let mut entry = assistant("p", "requested", usage(1, 1, None));
        let SessionTreeEntry::Message { message, .. } = &mut entry else {
            panic!()
        };
        let AgentMessage::Assistant { response_model, .. } = message else {
            panic!()
        };
        *response_model = Some("routed".into());
        let stats = session_stats_from_entries(&[entry]);
        assert_eq!(stats.per_model[0].key, "p/routed");
    }
}
