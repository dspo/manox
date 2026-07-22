//! Deterministic offline replay gate for model-facing context optimization.
//!
//! The corpus contains sanitized workload shapes rather than user content. It
//! exercises the production retention, rewrite, image, and schema-projection
//! paths without a provider call, then folds in recorded model latency only as
//! an explicitly labelled end-to-end proxy.

use std::collections::HashSet;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::language_model::{
    LanguageModelRequestMessage, LanguageModelToolResult, LanguageModelToolUse, MessageContent,
    Role,
};
use crate::message::Message;

#[derive(Debug, Deserialize)]
struct Corpus {
    version: u32,
    recorded_model_latency_ms: f64,
    context_window_tokens: u64,
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
struct Scenario {
    name: String,
    baseline: ReplayTrace,
    #[serde(default)]
    optimized: Option<ReplayTrace>,
}

#[derive(Debug, Deserialize)]
struct ReplayTrace {
    #[serde(default)]
    calls: Vec<ReplayCall>,
    #[serde(default)]
    images: Vec<ReplayImage>,
}

#[derive(Debug, Deserialize)]
struct ReplayCall {
    id: String,
    tool: String,
    input: serde_json::Value,
    output_seed: String,
    repeat: usize,
    #[serde(default)]
    is_error: bool,
}

#[derive(Debug, Deserialize)]
struct ReplayImage {
    mime_type: String,
    encoded_seed: String,
    repeat: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplayScenarioReport {
    pub name: String,
    pub baseline_input_tokens: u64,
    pub optimized_input_tokens: u64,
    pub input_reduction_pct: f64,
    pub baseline_model_calls: u64,
    pub optimized_model_calls: u64,
    pub baseline_compactions: u64,
    pub optimized_compactions: u64,
    pub baseline_e2e_proxy_ms: f64,
    pub optimized_e2e_proxy_ms: f64,
    pub local_projection_ms: f64,
    pub canonical_unchanged: bool,
    pub protocol_pairs_valid: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReplayReport {
    pub corpus_version: u32,
    pub token_estimator: &'static str,
    pub e2e_measurement: &'static str,
    pub median_input_reduction_pct: f64,
    pub model_call_regression_pct: f64,
    pub compaction_regression_pct: f64,
    pub median_e2e_regression_pct: f64,
    pub acceptance_passed: bool,
    pub scenarios: Vec<ReplayScenarioReport>,
}

pub fn bundled_corpus() -> &'static str {
    include_str!("../tests/fixtures/context_replay.json")
}

pub fn run_bundled() -> Result<ReplayReport, String> {
    run(bundled_corpus())
}

pub fn run(json: &str) -> Result<ReplayReport, String> {
    let corpus: Corpus =
        serde_json::from_str(json).map_err(|error| format!("invalid replay corpus: {error}"))?;
    if corpus.version != 2 || corpus.scenarios.is_empty() || corpus.context_window_tokens == 0 {
        return Err("unsupported or empty replay corpus".into());
    }

    let (full_schema_bytes, projected_schema_bytes) = production_schema_bytes()?;

    let mut reports = Vec::with_capacity(corpus.scenarios.len());
    for scenario in corpus.scenarios {
        let baseline = measure_trace(
            &scenario.baseline,
            false,
            full_schema_bytes,
            corpus.context_window_tokens,
        )?;
        let optimized = measure_trace(
            scenario.optimized.as_ref().unwrap_or(&scenario.baseline),
            true,
            projected_schema_bytes,
            corpus.context_window_tokens,
        )?;
        let baseline_tokens = median_u64(baseline.per_turn_input_tokens.clone());
        let optimized_tokens = median_u64(optimized.per_turn_input_tokens.clone());
        let input_reduction_pct = reduction_pct(baseline_tokens, optimized_tokens);
        let baseline_e2e = baseline.model_calls as f64 * corpus.recorded_model_latency_ms
            + baseline.local_projection_ms;
        let optimized_e2e = optimized.model_calls as f64 * corpus.recorded_model_latency_ms
            + optimized.local_projection_ms;
        reports.push(ReplayScenarioReport {
            name: scenario.name,
            baseline_input_tokens: baseline_tokens,
            optimized_input_tokens: optimized_tokens,
            input_reduction_pct,
            baseline_model_calls: baseline.model_calls,
            optimized_model_calls: optimized.model_calls,
            baseline_compactions: baseline.compactions,
            optimized_compactions: optimized.compactions,
            baseline_e2e_proxy_ms: baseline_e2e,
            optimized_e2e_proxy_ms: optimized_e2e,
            local_projection_ms: optimized.local_projection_ms,
            canonical_unchanged: baseline.canonical_unchanged && optimized.canonical_unchanged,
            protocol_pairs_valid: baseline.protocol_pairs_valid && optimized.protocol_pairs_valid,
        });
    }

    let median_input_reduction_pct = median(
        reports
            .iter()
            .map(|report| report.input_reduction_pct)
            .collect(),
    );
    let baseline_calls: u64 = reports
        .iter()
        .map(|report| report.baseline_model_calls)
        .sum();
    let optimized_calls: u64 = reports
        .iter()
        .map(|report| report.optimized_model_calls)
        .sum();
    let model_call_regression_pct = regression_pct(baseline_calls as f64, optimized_calls as f64);
    let baseline_compactions: u64 = reports
        .iter()
        .map(|report| report.baseline_compactions)
        .sum();
    let optimized_compactions: u64 = reports
        .iter()
        .map(|report| report.optimized_compactions)
        .sum();
    let compaction_regression_pct =
        regression_pct(baseline_compactions as f64, optimized_compactions as f64);
    let median_e2e_regression_pct = median(
        reports
            .iter()
            .map(|report| {
                regression_pct(report.baseline_e2e_proxy_ms, report.optimized_e2e_proxy_ms)
            })
            .collect(),
    );
    let acceptance_passed = median_input_reduction_pct >= 50.0
        && model_call_regression_pct <= 5.0
        && optimized_compactions <= baseline_compactions
        && median_e2e_regression_pct <= 5.0
        && reports
            .iter()
            .all(|report| report.canonical_unchanged && report.protocol_pairs_valid);
    Ok(ReplayReport {
        corpus_version: corpus.version,
        token_estimator: "per-turn deterministic UTF-8 bytes/4 over production projections and schemas",
        e2e_measurement: "trace-derived model/compaction calls × recorded latency plus measured projection time",
        median_input_reduction_pct,
        model_call_regression_pct,
        compaction_regression_pct,
        median_e2e_regression_pct,
        acceptance_passed,
        scenarios: reports,
    })
}

#[derive(Debug)]
struct TraceMeasurement {
    per_turn_input_tokens: Vec<u64>,
    model_calls: u64,
    compactions: u64,
    local_projection_ms: f64,
    canonical_unchanged: bool,
    protocol_pairs_valid: bool,
}

fn measure_trace(
    trace: &ReplayTrace,
    optimized: bool,
    schema_bytes: usize,
    context_window_tokens: u64,
) -> Result<TraceMeasurement, String> {
    let canonical = build_messages(trace);
    let untouched = serde_json::to_vec(&canonical)
        .map_err(|error| format!("canonical snapshot failed: {error}"))?;
    let mut per_turn_input_tokens = Vec::with_capacity(trace.calls.len() + 1);
    let mut protocol_valid = true;
    let started = Instant::now();

    for completed_calls in 0..=trace.calls.len() {
        let message_count = trace.images.len() + completed_calls * 2;
        let prefix = &canonical[..message_count.min(canonical.len())];
        let projected_messages = if optimized {
            let retained = crate::retention::preview(prefix, std::path::Path::new("/replay"))
                .unwrap_or_else(|| prefix.to_vec());
            protocol_valid &= protocol_pairs_valid(&retained);
            crate::optimizer::apply_image_policy(
                crate::optimizer::optimize(&retained)
                    .into_iter()
                    .map(|message| LanguageModelRequestMessage {
                        role: message.role,
                        content: message.content,
                        cache: false,
                    })
                    .collect(),
                false,
                None,
            )
        } else {
            protocol_valid &= protocol_pairs_valid(prefix);
            prefix
                .iter()
                .map(|message| LanguageModelRequestMessage {
                    role: message.role,
                    content: message.content.clone(),
                    cache: false,
                })
                .collect()
        };
        per_turn_input_tokens.push(approx_tokens(
            request_bytes(&projected_messages).saturating_add(schema_bytes),
        ));
    }

    let compactions = u64::from(
        per_turn_input_tokens
            .iter()
            .any(|tokens| *tokens >= context_window_tokens.saturating_mul(4) / 5),
    );
    let model_calls = per_turn_input_tokens.len() as u64 + compactions;
    Ok(TraceMeasurement {
        per_turn_input_tokens,
        model_calls,
        compactions,
        local_projection_ms: started.elapsed().as_secs_f64() * 1000.0,
        canonical_unchanged: serde_json::to_vec(&canonical)
            .map(|snapshot| snapshot == untouched)
            .unwrap_or(false),
        protocol_pairs_valid: protocol_valid,
    })
}

fn production_schema_bytes() -> Result<(usize, usize), String> {
    let mut registry = crate::tool::ToolRegistry::new();
    for tool in crate::tools::base_tools(std::sync::Arc::new(std::path::PathBuf::from("/replay"))) {
        registry.register(tool);
    }
    registry.register(std::sync::Arc::new(
        crate::tools::update_plan::UpdatePlanTool,
    ));
    registry.register(std::sync::Arc::new(crate::tools::code::CodeTool));
    crate::tools::tool_search::register(&mut registry);
    let full = registry.to_request_tools(crate::language::Language::En);
    let mut core: Vec<String> = crate::tools::tool_search::CORE_TOOLS
        .iter()
        .map(|name| (*name).to_string())
        .collect();
    core.push(crate::tools::CODE.to_string());
    let projected = registry.to_request_tools_in_order(&core, crate::language::Language::En, false);
    let encoded_len = |tools: &Vec<crate::language_model::LanguageModelRequestTool>| {
        serde_json::to_vec(tools)
            .map(|bytes| bytes.len())
            .map_err(|error| format!("schema snapshot failed: {error}"))
    };
    Ok((encoded_len(&full)?, encoded_len(&projected)?))
}

fn build_messages(trace: &ReplayTrace) -> Vec<Message> {
    let mut messages = Vec::new();
    for image in &trace.images {
        messages.push(message(
            Role::User,
            vec![MessageContent::Image {
                data: image.encoded_seed.repeat(image.repeat),
                mime_type: image.mime_type.clone(),
            }],
        ));
    }
    for call in &trace.calls {
        messages.push(message(
            Role::Assistant,
            vec![MessageContent::ToolUse(LanguageModelToolUse {
                id: call.id.clone(),
                name: call.tool.clone().into(),
                raw_input: call.input.to_string(),
                input: call.input.clone(),
                is_input_complete: true,
                thought_signature: None,
            })],
        ));
        messages.push(message(
            Role::User,
            vec![MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: call.id.clone(),
                tool_name: call.tool.clone().into(),
                is_error: call.is_error,
                content: call.output_seed.repeat(call.repeat),
            })],
        ));
    }
    messages
}

fn message(role: Role, content: Vec<MessageContent>) -> Message {
    Message {
        id: format!("replay-{}", uuid::Uuid::new_v4()),
        timestamp: 0,
        parent_id: None,
        role,
        content,
        ui: None,
    }
}

fn content_bytes(content: &MessageContent) -> usize {
    match content {
        MessageContent::Text(text) | MessageContent::Compaction(text) => text.len(),
        MessageContent::Thinking { text, signature } => {
            text.len() + signature.as_ref().map_or(0, String::len)
        }
        MessageContent::Image { data, mime_type } => data.len() + mime_type.len(),
        MessageContent::ToolUse(tool_use) => {
            tool_use.raw_input.len()
                + serde_json::to_string(&tool_use.input).map_or(0, |value| value.len())
        }
        MessageContent::ToolResult(result) => result.content.len(),
    }
}

fn request_bytes(messages: &[LanguageModelRequestMessage]) -> usize {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .map(content_bytes)
        .sum()
}

fn protocol_pairs_valid(messages: &[Message]) -> bool {
    let uses: HashSet<&str> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            MessageContent::ToolUse(tool_use) => Some(tool_use.id.as_str()),
            _ => None,
        })
        .collect();
    let results: HashSet<&str> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|content| match content {
            MessageContent::ToolResult(result) => Some(result.tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    uses == results
}

fn approx_tokens(bytes: usize) -> u64 {
    bytes.div_ceil(4) as u64
}

fn reduction_pct(baseline: u64, optimized: u64) -> f64 {
    if baseline == 0 {
        0.0
    } else {
        (1.0 - optimized as f64 / baseline as f64) * 100.0
    }
}

fn regression_pct(baseline: f64, optimized: f64) -> f64 {
    if baseline <= f64::EPSILON {
        0.0
    } else {
        (optimized / baseline - 1.0) * 100.0
    }
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    }
}

fn median_u64(values: Vec<u64>) -> u64 {
    median(values.into_iter().map(|value| value as f64).collect()).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_replay_meets_acceptance_gate() {
        let report = run_bundled().unwrap();
        assert!(
            report.acceptance_passed,
            "{}",
            serde_json::to_string_pretty(&report).unwrap()
        );
    }
}
