//! OMP session jsonl 解析。
//!
//! OMP 管理多个 Claude Code session，日志存放在 `~/.omp/agent/sessions/` 下，
//! 格式与 Claude Code 原生 jsonl 相似但有字段差异：
//! - `type: "message"` + `message.role: "assistant"` 标记 assistant 消息
//! - usage 字段为 camelCase：`input` / `output` / `cacheRead` / `cacheWrite`
//! - 无 sessionId / requestId / version 等校验字段
//!
//! 口径：raw-sum（不按 message id 去重），与 Claude Code Stats 保持一致。

use serde_json::Value;

use super::RawEntry;
use crate::stats::date::date_field;

const AGENT: &str = super::super::AGENT_OMP;

pub(super) fn parse(content: &str) -> Vec<RawEntry> {
    let mut out: Vec<RawEntry> = Vec::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(entry) = parse_one(&v) {
            out.push(entry);
        }
    }
    out
}

fn parse_one(v: &Value) -> Option<RawEntry> {
    // OMP session 中 assistant 消息为 type="message" + role="assistant"
    if v.get("type").and_then(Value::as_str) != Some("message") {
        return None;
    }

    let message = v.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }

    let usage = message.get("usage")?;
    if usage.is_null() {
        return None;
    }

    let model = message
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)?;
    if model.is_empty() || model == "<synthetic>" {
        return None;
    }

    // 日期从顶层 timestamp（ISO 8601）提取
    let date = date_field(v.get("timestamp"))?;

    // OMP usage 字段为 camelCase
    let input_tokens = usage.get("input").and_then(Value::as_u64).unwrap_or(0);
    let output_tokens = usage.get("output").and_then(Value::as_u64).unwrap_or(0);
    let cache_read = usage.get("cacheRead").and_then(Value::as_u64).unwrap_or(0);
    let cache_write = usage.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0);

    if input_tokens == 0 && cache_read == 0 && cache_write == 0 && output_tokens == 0 {
        return None;
    }

    Some(RawEntry {
        agent: AGENT.to_string(),
        model: model.to_string(),
        date,
        input_tokens,
        output_tokens,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_write,
        reasoning_output_tokens: 0,
        dedup_primary: None,
        dedup_secondary: None,
        is_sidechain: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_assistant_message() {
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-03T06:23:10.437Z","message":{"role":"assistant","api":"anthropic-messages","provider":"packyapi","model":"claude-opus-4-8","usage":{"input":1500,"output":300,"cacheRead":7000,"cacheWrite":200,"totalTokens":9000},"stopReason":"toolUse","timestamp":1780467789239,"duration":1180}}"#;
        let entries = parse(line);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.agent, "omp");
        assert_eq!(e.model, "claude-opus-4-8");
        assert_eq!(e.date, "2026-06-03");
        assert_eq!(e.input_tokens, 1500);
        assert_eq!(e.output_tokens, 300);
        assert_eq!(e.cache_read_input_tokens, 7000);
        assert_eq!(e.cache_creation_input_tokens, 200);
    }

    #[test]
    fn skips_non_assistant_messages() {
        let line = r#"{"type":"message","id":"abc","timestamp":"2026-06-03T06:23:10.437Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_error_with_zero_usage() {
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-03T06:23:10.437Z","message":{"role":"assistant","api":"anthropic-messages","provider":"packyapi","model":"claude-opus-4-8","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0},"stopReason":"error","timestamp":1780467789239,"errorStatus":401,"duration":1180}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_empty_model() {
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-03T06:23:10.437Z","message":{"role":"assistant","model":"","usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0}}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_synthetic_model() {
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-03T06:23:10.437Z","message":{"role":"assistant","model":"<synthetic>","usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0}}}"#;
        assert!(parse(line).is_empty());
    }
}
