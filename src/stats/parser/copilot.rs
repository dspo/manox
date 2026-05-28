//! GitHub Copilot CLI 的 OpenTelemetry JSONL 解析。
//!
//! 数据源：`~/.copilot/otel/*.jsonl`，外加 `COPILOT_OTEL_FILE_EXPORTER_PATH` 环境变量
//! 指向单文件。
//!
//! 设计参考 ccusage `adapter/copilot/parser.rs`：
//! - 同一次请求会被多种来源同时记录：`chat span` / `inference log` / `agent turn log`
//!   / `agent summary span`。**必须按 trace_id + response_id 在四种来源里去重**，否则
//!   按优先级 `chat → inference → agent_turn → summary` 各重复一份。
//! - 本模块在解析阶段就完成同一文件内的去重；上层再统一做跨文件去重。
//! - token 字段位于 `attributes` 下，命名空间 `gen_ai.usage.*`。
//! - `input_tokens` 在 Copilot schema 中**包含** `cache_read.input_tokens`，要减掉才得到 raw input。
//! - 时间戳支持多种格式：`[seconds, nanos]` 数组、`timeUnixNano`、毫秒/微秒/纳秒标量。

use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::RawEntry;
use crate::stats::date::date_from_iso;
use crate::stats::format;

const MODEL_ATTRS: &[&str] = &["gen_ai.response.model", "gen_ai.request.model"];

/// 优先级越高，越被偏好用作 session_id。
const SESSION_ATTRS: &[(&str, u8)] = &[
    ("gen_ai.conversation.id", 3),
    ("copilot_chat.session_id", 3),
    ("copilot_chat.chat_session_id", 3),
    ("session.id", 3),
    ("github.copilot.interaction_id", 2),
    ("gen_ai.response.id", 1),
];

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Source {
    ChatSpan,
    InferenceLog,
    AgentTurnLog,
    AgentSummarySpan,
}

#[derive(Default)]
struct TraceContext {
    model: Option<String>,
    session_id: Option<String>,
    session_id_priority: u8,
}

struct Candidate {
    source: Source,
    trace_id: Option<String>,
    response_id: Option<String>,
    model: String,
    session_id: String,
    date: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_tokens: u64,
    cache_read_tokens: u64,
    reasoning_output_tokens: u64,
    timestamp_iso: String,
}

pub(super) fn parse(content: &str, agent: &str, _path: &Path) -> Vec<RawEntry> {
    let records: Vec<Map<String, Value>> = content
        .lines()
        .filter(|line| line.contains("\"attributes\""))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|v| v.as_object().cloned())
        .collect();

    let trace_contexts = collect_trace_contexts(&records);
    let candidates: Vec<Candidate> = records
        .iter()
        .filter_map(|record| to_candidate(record, &trace_contexts))
        .collect();
    let sets = CandidateSets::from(&candidates);

    candidates
        .into_iter()
        .filter(|c| should_emit(c, &sets))
        .map(|c| RawEntry {
            agent: agent.to_string(),
            model: c.model,
            date: c.date,
            input_tokens: c.input_tokens,
            output_tokens: c.output_tokens,
            cache_read_input_tokens: c.cache_read_tokens,
            cache_creation_input_tokens: c.cache_creation_tokens,
            reasoning_output_tokens: c.reasoning_output_tokens,
            // 跨文件去重主键：response_id 是 OpenAI 端的请求 ID，最稳定；缺失时用 trace+session+ts。
            dedup_primary: c
                .response_id
                .clone()
                .or_else(|| c.trace_id.clone())
                .or_else(|| Some(format!("{}|{}", c.session_id, c.timestamp_iso))),
            dedup_secondary: Some(format!(
                "{}/{}/{}/{}",
                c.input_tokens, c.cache_read_tokens, c.cache_creation_tokens, c.output_tokens
            )),
            is_sidechain: false,
        })
        .collect()
}

fn collect_trace_contexts(records: &[Map<String, Value>]) -> HashMap<String, TraceContext> {
    let mut contexts: HashMap<String, TraceContext> = HashMap::new();
    for record in records {
        let Some(trace_id) = trace_id(record) else {
            continue;
        };
        let Some(attributes) = record.get("attributes").and_then(Value::as_object) else {
            continue;
        };
        let ctx = contexts.entry(trace_id).or_default();
        if ctx.model.is_none() {
            ctx.model = first_non_empty_attr(attributes, MODEL_ATTRS);
        }
        if let Some((sid, prio)) = best_session_attr(attributes) {
            if prio > ctx.session_id_priority {
                ctx.session_id = Some(sid);
                ctx.session_id_priority = prio;
            }
        }
    }
    contexts
}

fn to_candidate(
    record: &Map<String, Value>,
    trace_contexts: &HashMap<String, TraceContext>,
) -> Option<Candidate> {
    let attributes = record.get("attributes")?.as_object()?;
    let source = if is_chat_span(record, attributes) {
        Source::ChatSpan
    } else if is_inference_log(record, attributes) {
        Source::InferenceLog
    } else if is_agent_turn_log(record, attributes) {
        Source::AgentTurnLog
    } else if is_agent_summary_span(record, attributes) {
        Source::AgentSummarySpan
    } else {
        return None;
    };

    let input_raw = attr_number(attributes, "gen_ai.usage.input_tokens");
    let mut output = attr_number(attributes, "gen_ai.usage.output_tokens");
    let cache_read = attr_number(attributes, "gen_ai.usage.cache_read.input_tokens");
    let cache_create = attr_number_first(
        attributes,
        &[
            "gen_ai.usage.cache_write.input_tokens",
            "gen_ai.usage.cache_creation.input_tokens",
        ],
    );
    let mut reasoning = attr_number_first(
        attributes,
        &[
            "gen_ai.usage.reasoning.output_tokens",
            "gen_ai.usage.reasoning_tokens",
        ],
    );

    // input_tokens 在 Copilot schema 中包含了 cache_read，要减掉
    let input = input_raw.saturating_sub(input_raw.min(cache_read));
    let total = attr_number_first(
        attributes,
        &[
            "gen_ai.usage.total_tokens",
            "gen_ai.usage.total.token_count",
        ],
    );
    apply_total_token_fallback(
        input,
        cache_read,
        cache_create,
        &mut output,
        &mut reasoning,
        total,
    );

    if input + output + cache_read + cache_create + reasoning == 0 {
        return None;
    }

    let trace_id = trace_id(record);
    let trace_ctx = trace_id.as_ref().and_then(|t| trace_contexts.get(t));

    let response_id = attr_string(attributes, "gen_ai.response.id");
    let model = first_non_empty_attr(attributes, MODEL_ATTRS)
        .or_else(|| trace_ctx.and_then(|c| c.model.clone()))
        .unwrap_or_else(|| "unknown".to_string());
    let session_id = best_session_attr(attributes)
        .map(|(s, _)| s)
        .or_else(|| trace_ctx.and_then(|c| c.session_id.clone()))
        .or_else(|| trace_id.clone())
        .unwrap_or_else(|| "unknown-session".to_string());

    let timestamp_iso = timestamp_from_record(record).unwrap_or_default();
    let date = date_from_iso(&timestamp_iso);
    if date.is_empty() {
        return None;
    }

    Some(Candidate {
        source,
        trace_id,
        response_id,
        model,
        session_id,
        date,
        input_tokens: input,
        output_tokens: output,
        cache_creation_tokens: cache_create,
        cache_read_tokens: cache_read,
        reasoning_output_tokens: reasoning,
        timestamp_iso,
    })
}

struct CandidateSets {
    chat_traces: HashSet<String>,
    inference_traces: HashSet<String>,
    agent_turn_traces: HashSet<String>,
    chat_response_ids: HashSet<String>,
    inference_response_ids: HashSet<String>,
    agent_turn_response_ids: HashSet<String>,
}

impl CandidateSets {
    fn from(candidates: &[Candidate]) -> Self {
        let by_trace = |s: Source| -> HashSet<String> {
            candidates
                .iter()
                .filter(|c| c.source == s)
                .filter_map(|c| c.trace_id.clone())
                .collect()
        };
        let by_response = |s: Source| -> HashSet<String> {
            candidates
                .iter()
                .filter(|c| c.source == s)
                .filter_map(|c| c.response_id.clone())
                .collect()
        };
        Self {
            chat_traces: by_trace(Source::ChatSpan),
            inference_traces: by_trace(Source::InferenceLog),
            agent_turn_traces: by_trace(Source::AgentTurnLog),
            chat_response_ids: by_response(Source::ChatSpan),
            inference_response_ids: by_response(Source::InferenceLog),
            agent_turn_response_ids: by_response(Source::AgentTurnLog),
        }
    }
}

/// 优先保留 chat span；下沉到 inference / agent_turn / summary 时跳过已被前级覆盖的同 trace/response。
fn should_emit(c: &Candidate, sets: &CandidateSets) -> bool {
    let trace_in = |set: &HashSet<String>| c.trace_id.as_ref().is_some_and(|t| set.contains(t));
    let resp_in = |set: &HashSet<String>| c.response_id.as_ref().is_some_and(|r| set.contains(r));
    match c.source {
        Source::ChatSpan => true,
        Source::InferenceLog => !trace_in(&sets.chat_traces) && !resp_in(&sets.chat_response_ids),
        Source::AgentTurnLog => {
            !trace_in(&sets.chat_traces)
                && !trace_in(&sets.inference_traces)
                && !resp_in(&sets.chat_response_ids)
                && !resp_in(&sets.inference_response_ids)
        }
        Source::AgentSummarySpan => {
            !trace_in(&sets.chat_traces)
                && !trace_in(&sets.inference_traces)
                && !trace_in(&sets.agent_turn_traces)
                && !resp_in(&sets.chat_response_ids)
                && !resp_in(&sets.inference_response_ids)
                && !resp_in(&sets.agent_turn_response_ids)
        }
    }
}

fn is_span_record(record: &Map<String, Value>) -> bool {
    if let Some(t) = record.get("type").and_then(Value::as_str) {
        return t == "span";
    }
    string_value(record.get("name")).is_some()
        && (string_value(record.get("spanId")).is_some()
            || string_value(record.get("traceId")).is_some()
            || record.get("startTime").is_some()
            || record.get("endTime").is_some()
            || record.get("duration").is_some()
            || record.get("kind").is_some())
}

fn is_chat_span(record: &Map<String, Value>, attrs: &Map<String, Value>) -> bool {
    is_span_record(record)
        && (attr_string(attrs, "gen_ai.operation.name").as_deref() == Some("chat")
            || string_value(record.get("name")).is_some_and(|n| n.starts_with("chat ")))
}

fn is_agent_summary_span(record: &Map<String, Value>, attrs: &Map<String, Value>) -> bool {
    is_span_record(record)
        && (attr_string(attrs, "gen_ai.operation.name").as_deref() == Some("invoke_agent")
            || string_value(record.get("name")).is_some_and(|n| n.starts_with("invoke_agent ")))
}

fn is_inference_log(record: &Map<String, Value>, attrs: &Map<String, Value>) -> bool {
    !is_span_record(record)
        && (attr_string(attrs, "event.name").as_deref()
            == Some("gen_ai.client.inference.operation.details")
            || record_body(record).is_some_and(|b| b.starts_with("GenAI inference:")))
}

fn is_agent_turn_log(record: &Map<String, Value>, attrs: &Map<String, Value>) -> bool {
    !is_span_record(record)
        && (attr_string(attrs, "event.name").as_deref() == Some("copilot_chat.agent.turn")
            || record_body(record).is_some_and(|b| b.starts_with("copilot_chat.agent.turn")))
}

fn trace_id(record: &Map<String, Value>) -> Option<String> {
    string_value(record.get("traceId"))
        .map(str::to_string)
        .or_else(|| nested_string(record, "spanContext", "traceId"))
}

fn nested_string(record: &Map<String, Value>, object: &str, key: &str) -> Option<String> {
    record
        .get(object)
        .and_then(Value::as_object)
        .and_then(|o| string_value(o.get(key)))
        .map(str::to_string)
}

fn record_body(record: &Map<String, Value>) -> Option<&str> {
    string_value(record.get("body")).or_else(|| string_value(record.get("_body")))
}

fn string_value(value: Option<&Value>) -> Option<&str> {
    let v = value?.as_str()?.trim();
    (!v.is_empty()).then_some(v)
}

fn number_value(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().and_then(|i| (i >= 0).then_some(i as u64))),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn attr_string(attrs: &Map<String, Value>, key: &str) -> Option<String> {
    string_value(attrs.get(key)).map(str::to_string)
}

fn attr_number(attrs: &Map<String, Value>, key: &str) -> u64 {
    number_value(attrs.get(key)).unwrap_or_default()
}

fn attr_number_first(attrs: &Map<String, Value>, keys: &[&str]) -> u64 {
    keys.iter()
        .map(|k| attr_number(attrs, k))
        .find(|v| *v > 0)
        .unwrap_or_default()
}

fn apply_total_token_fallback(
    input: u64,
    cache_read: u64,
    cache_create: u64,
    output: &mut u64,
    reasoning: &mut u64,
    total: u64,
) {
    let known = input
        .saturating_add(*output)
        .saturating_add(cache_read)
        .saturating_add(cache_create)
        .saturating_add(*reasoning);
    let missing = total.saturating_sub(known);
    if missing == 0 {
        return;
    }
    if *output == 0 {
        *output = missing;
    } else {
        *reasoning = (*reasoning).saturating_add(missing);
    }
}

fn first_non_empty_attr(attrs: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| attr_string(attrs, k))
}

fn best_session_attr(attrs: &Map<String, Value>) -> Option<(String, u8)> {
    SESSION_ATTRS
        .iter()
        .filter_map(|(k, p)| attr_string(attrs, k).map(|v| (v, *p)))
        .max_by_key(|(_, p)| *p)
}

fn timestamp_from_record(record: &Map<String, Value>) -> Option<String> {
    timestamp_from_parts(record.get("endTime"))
        .or_else(|| timestamp_from_parts(record.get("startTime")))
        .or_else(|| timestamp_from_parts(record.get("hrTime")))
        .or_else(|| timestamp_from_parts(record.get("_hrTime")))
        .or_else(|| timestamp_from_parts(record.get("time")))
        .or_else(|| timestamp_from_scalar(record.get("timestamp")))
        .or_else(|| timestamp_from_scalar(record.get("observedTimestamp")))
        .or_else(|| timestamp_from_unix_nanos(record.get("timeUnixNano")))
}

fn timestamp_from_parts(value: Option<&Value>) -> Option<String> {
    let arr = value?.as_array()?;
    let secs = number_value(arr.first())?;
    let nanos = number_value(arr.get(1))?;
    let ms = secs.checked_mul(1_000)?.checked_add(nanos / 1_000_000)?;
    Some(format::iso_from_unix_ms(ms.min(i64::MAX as u64) as i64))
}

fn timestamp_from_scalar(value: Option<&Value>) -> Option<String> {
    let raw = number_value(value)?;
    let ms = if raw >= 100_000_000_000_000_000 {
        raw / 1_000_000
    } else if raw >= 100_000_000_000_000 {
        raw / 1_000
    } else if raw >= 100_000_000_000 {
        raw
    } else {
        raw * 1_000
    };
    Some(format::iso_from_unix_ms(ms.min(i64::MAX as u64) as i64))
}

fn timestamp_from_unix_nanos(value: Option<&Value>) -> Option<String> {
    let raw = number_value(value)?;
    if raw == 0 {
        return None;
    }
    Some(format::iso_from_unix_ms(
        (raw / 1_000_000).min(i64::MAX as u64) as i64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_lines(lines: &str) -> Vec<RawEntry> {
        parse(lines, "copilot", Path::new("/tmp/copilot.jsonl"))
    }

    #[test]
    fn picks_chat_span_over_inference_log_for_same_response() {
        let chat = r#"{"type":"span","traceId":"t1","spanId":"s1","name":"chat gpt-4o","startTime":[1779000000,0],"attributes":{"gen_ai.operation.name":"chat","gen_ai.response.model":"gpt-4o","gen_ai.response.id":"resp-1","gen_ai.usage.input_tokens":120,"gen_ai.usage.output_tokens":30,"gen_ai.usage.cache_read.input_tokens":50,"gen_ai.conversation.id":"conv-1"}}"#;
        let inference = r#"{"traceId":"t1","spanId":"s2","body":"GenAI inference: gpt-4o","attributes":{"event.name":"gen_ai.client.inference.operation.details","gen_ai.response.model":"gpt-4o","gen_ai.response.id":"resp-1","gen_ai.usage.input_tokens":120,"gen_ai.usage.output_tokens":30,"gen_ai.usage.cache_read.input_tokens":50,"gen_ai.conversation.id":"conv-1","timeUnixNano":1779000000000000000}"#;
        let inference = format!("{inference}}}");
        let content = format!("{chat}\n{inference}\n");
        let r = parse_lines(&content);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].model, "gpt-4o");
        // input - cache_read: 120 - 50 = 70
        assert_eq!(r[0].input_tokens, 70);
        assert_eq!(r[0].cache_read_input_tokens, 50);
        assert_eq!(r[0].output_tokens, 30);
    }

    #[test]
    fn falls_back_to_inference_log_when_no_chat_span() {
        let inference = r#"{"traceId":"t9","spanId":"s9","body":"GenAI inference: gpt-4o","attributes":{"event.name":"gen_ai.client.inference.operation.details","gen_ai.response.model":"gpt-4o","gen_ai.response.id":"r9","gen_ai.usage.input_tokens":10,"gen_ai.usage.output_tokens":3,"gen_ai.conversation.id":"c9"},"timeUnixNano":1779000000000000000}"#;
        let r = parse_lines(&format!("{inference}\n"));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].input_tokens, 10);
    }

    #[test]
    fn applies_total_token_fallback_to_missing_copilot_output() {
        let chat = r#"{"type":"span","traceId":"t-total","spanId":"s1","name":"chat gpt-4o","startTime":[1779000000,0],"attributes":{"gen_ai.operation.name":"chat","gen_ai.response.model":"gpt-4o","gen_ai.response.id":"resp-total","gen_ai.usage.input_tokens":125,"gen_ai.usage.cache_read.input_tokens":25,"gen_ai.usage.total_tokens":175,"gen_ai.conversation.id":"conv-total"}}"#;
        let r = parse_lines(&format!("{chat}\n"));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].input_tokens, 100);
        assert_eq!(r[0].cache_read_input_tokens, 25);
        assert_eq!(r[0].output_tokens, 50);
    }

    #[test]
    fn keeps_total_token_fallback_as_reasoning_when_output_is_known() {
        let chat = r#"{"type":"span","traceId":"t-extra","spanId":"s1","name":"chat gpt-4o","startTime":[1779000000,0],"attributes":{"gen_ai.operation.name":"chat","gen_ai.response.model":"gpt-4o","gen_ai.response.id":"resp-extra","gen_ai.usage.input_tokens":125,"gen_ai.usage.cache_read.input_tokens":25,"gen_ai.usage.output_tokens":50,"gen_ai.usage.total_tokens":200,"gen_ai.conversation.id":"conv-extra"}}"#;
        let r = parse_lines(&format!("{chat}\n"));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].output_tokens, 50);
        assert_eq!(r[0].reasoning_output_tokens, 25);
    }
}
