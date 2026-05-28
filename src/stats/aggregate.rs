//! 按 model / (agent, model) 维度的聚合工具。

use std::collections::HashMap;

use super::types::{UsageRecord, UsageTotals};

pub(super) fn totals_by_model(records: &[&UsageRecord]) -> HashMap<String, UsageTotals> {
    let mut map: HashMap<String, UsageTotals> = HashMap::new();
    for r in records {
        let entry = map.entry(r.model.clone()).or_default();
        entry.add_record(r);
    }
    map
}

pub(super) fn totals_by_agent_model(
    records: &[&UsageRecord],
) -> HashMap<(String, String), UsageTotals> {
    let mut map: HashMap<(String, String), UsageTotals> = HashMap::new();
    for r in records {
        let entry = map.entry((r.agent.clone(), r.model.clone())).or_default();
        entry.add_record(r);
    }
    map
}

/// 按总用量降序取头部模型，直到累计占比 ≥ `ratio`。
/// 至少返回 1 个非空模型（如有），避免折线图为空。
pub(super) fn top_models_covering(
    totals: &HashMap<String, UsageTotals>,
    ratio: f64,
) -> Vec<String> {
    let mut v: Vec<(String, u64)> = totals
        .iter()
        .map(|(k, usage)| (k.clone(), usage.total_tokens()))
        .filter(|(_, t)| *t > 0)
        .collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let grand_total: u64 = v.iter().map(|(_, t)| *t).sum();
    if grand_total == 0 {
        return Vec::new();
    }
    let threshold = (grand_total as f64 * ratio).ceil() as u64;

    let mut acc: u64 = 0;
    let mut out: Vec<String> = Vec::new();
    for (model, total) in v {
        out.push(model);
        acc = acc.saturating_add(total);
        if acc >= threshold {
            break;
        }
    }
    out
}
