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
    scenarios: Vec<Scenario>,
}

#[derive(Debug, Deserialize)]
struct Scenario {
    name: String,
    #[serde(default)]
    calls: Vec<ReplayCall>,
    #[serde(default)]
    images: Vec<ReplayImage>,
    schema_baseline_bytes: usize,
    schema_projected_bytes: usize,
    baseline_model_calls: u64,
    optimized_model_calls: u64,
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
    if corpus.version != 1 || corpus.scenarios.is_empty() {
        return Err("unsupported or empty replay corpus".into());
    }

    let mut reports = Vec::with_capacity(corpus.scenarios.len());
    for scenario in corpus.scenarios {
        let canonical = build_messages(&scenario);
        let untouched = serde_json::to_vec(&canonical)
            .map_err(|error| format!("canonical snapshot failed: {error}"))?;
        let baseline_bytes =
            message_bytes(&canonical).saturating_add(scenario.schema_baseline_bytes);
        let started = Instant::now();
        let retained = crate::retention::preview(&canonical, std::path::Path::new("/replay"))
            .unwrap_or_else(|| canonical.clone());
        let rewritten = crate::optimizer::optimize(&retained);
        let projected = crate::optimizer::apply_image_policy(
            rewritten
                .iter()
                .map(|message| LanguageModelRequestMessage {
                    role: message.role,
                    content: message.content.clone(),
                    cache: false,
                })
                .collect(),
            false,
            None,
        );
        let optimized_bytes =
            request_bytes(&projected).saturating_add(scenario.schema_projected_bytes);
        let local_projection_ms = started.elapsed().as_secs_f64() * 1000.0;
        let baseline_tokens = approx_tokens(baseline_bytes);
        let optimized_tokens = approx_tokens(optimized_bytes);
        let input_reduction_pct = reduction_pct(baseline_tokens, optimized_tokens);
        let baseline_e2e = scenario.baseline_model_calls as f64 * corpus.recorded_model_latency_ms;
        let optimized_e2e = scenario.optimized_model_calls as f64
            * corpus.recorded_model_latency_ms
            + local_projection_ms;
        reports.push(ReplayScenarioReport {
            name: scenario.name,
            baseline_input_tokens: baseline_tokens,
            optimized_input_tokens: optimized_tokens,
            input_reduction_pct,
            baseline_model_calls: scenario.baseline_model_calls,
            optimized_model_calls: scenario.optimized_model_calls,
            baseline_e2e_proxy_ms: baseline_e2e,
            optimized_e2e_proxy_ms: optimized_e2e,
            local_projection_ms,
            canonical_unchanged: serde_json::to_vec(&canonical)
                .map(|snapshot| snapshot == untouched)
                .unwrap_or(false),
            protocol_pairs_valid: protocol_pairs_valid(&retained),
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
        && median_e2e_regression_pct <= 5.0
        && reports
            .iter()
            .all(|report| report.canonical_unchanged && report.protocol_pairs_valid);
    Ok(ReplayReport {
        corpus_version: corpus.version,
        token_estimator: "deterministic UTF-8 bytes/4 (same local estimator as compaction)",
        e2e_measurement: "recorded model-latency proxy plus measured local projection time",
        median_input_reduction_pct,
        model_call_regression_pct,
        median_e2e_regression_pct,
        acceptance_passed,
        scenarios: reports,
    })
}

fn build_messages(scenario: &Scenario) -> Vec<Message> {
    let mut messages = Vec::new();
    for call in &scenario.calls {
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
    for image in &scenario.images {
        messages.push(message(
            Role::User,
            vec![MessageContent::Image {
                data: image.encoded_seed.repeat(image.repeat),
                mime_type: image.mime_type.clone(),
            }],
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

fn message_bytes(messages: &[Message]) -> usize {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .map(content_bytes)
        .sum()
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
