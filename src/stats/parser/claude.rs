//! Claude Code 的 jsonl 解析。
//!
//! 关键口径：
//! - Claude Code Stats 视图与本地实测一致：按顶层 `type: "assistant"` usage 行 raw sum，
//!   不按 `message.id`/`requestId` 去重；这点不同于 ccusage 的 Claude 报表口径。
//! - 递归扫描 `.claude/projects` 会包含 `subagents/*.jsonl`，但 subagent 文件里的条目仍然是
//!   顶层 assistant 行。
//! - 过滤 `<synthetic>` / 空 model / 空白 requestId/sessionId/messageId / 非 semver version。

use serde_json::Value;

use super::{RawEntry, u64_field};
use crate::stats::date::{date_field, timestamp_secs_from_iso};

const AGENT: &str = super::super::AGENT_CLAUDE;

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
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return None;
    }

    let message = v.get("message")?;
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

    // 空字符串字段过滤（参考 ccusage is_valid_usage_entry）
    if is_blank_string_field(v, "sessionId")
        || is_blank_string_field(v, "requestId")
        || is_blank_string_field(message, "id")
        || is_blank_string_field(message, "model")
    {
        return None;
    }
    if let Some(version) = v.get("version").and_then(Value::as_str) {
        if !is_semver_prefix(version) {
            return None;
        }
    }

    let date = date_field(v.get("timestamp"))?;

    let timestamp_secs = v
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(timestamp_secs_from_iso);

    let input_tokens = u64_field(usage, "input_tokens");
    let cache_read = u64_field(usage, "cache_read_input_tokens");
    let cache_create = u64_field(usage, "cache_creation_input_tokens");
    let out_tokens = u64_field(usage, "output_tokens");
    if input_tokens == 0 && cache_read == 0 && cache_create == 0 && out_tokens == 0 {
        return None;
    }

    Some(RawEntry {
        agent: AGENT.to_string(),
        model: model.to_string(),
        date,
        input_tokens,
        output_tokens: out_tokens,
        cache_read_input_tokens: cache_read,
        cache_creation_input_tokens: cache_create,
        reasoning_output_tokens: 0,
        dedup_primary: None,
        dedup_secondary: None,
        is_sidechain: v
            .get("isSidechain")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        session_id: v
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string),
        message_id: message
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string),
        timestamp_secs,
    })
}

fn is_blank_string_field(v: &Value, key: &str) -> bool {
    match v.get(key) {
        Some(value) => value.as_str().is_some_and(str::is_empty),
        None => false,
    }
}

/// 极简 semver 前缀校验：`<digits>.<digits>.<digits>`。
fn is_semver_prefix(value: &str) -> bool {
    fn consume_digits(bytes: &[u8], i: &mut usize) -> bool {
        let start = *i;
        while bytes.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i > start
    }

    let bytes = value.as_bytes();
    let mut i = 0;
    if !consume_digits(bytes, &mut i) || bytes.get(i) != Some(&b'.') {
        return false;
    }
    i += 1;
    if !consume_digits(bytes, &mut i) || bytes.get(i) != Some(&b'.') {
        return false;
    }
    i += 1;
    bytes.get(i).is_some_and(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_by_message_timestamp_without_dedup_keys() {
        let line = r#"{"type":"assistant","timestamp":"2026-05-27T12:34:56Z","requestId":"req-1","isSidechain":false,"message":{"id":"msg-1","model":"claude-opus-4-7","usage":{"input_tokens":100,"cache_read_input_tokens":20,"cache_creation_input_tokens":5,"output_tokens":7}}}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        let e = parse_one(&v).unwrap();
        assert_eq!(e.agent, AGENT);
        assert_eq!(e.model, "claude-opus-4-7");
        assert_eq!(e.date, "2026-05-27");
        assert_eq!(e.input_tokens, 100);
        assert_eq!(e.output_tokens, 7);
        assert_eq!(e.cache_read_input_tokens, 20);
        assert_eq!(e.cache_creation_input_tokens, 5);
        assert!(e.dedup_primary.is_none());
        assert!(e.dedup_secondary.is_none());
        assert!(!e.is_sidechain);
        assert_eq!(e.timestamp_secs, Some(1_779_885_296)); // 2026-05-27T12:34:56Z
    }

    #[test]
    fn ignores_agent_progress_wrapper() {
        let line = r#"{"data":{"message":{"timestamp":"2026-03-29T07:00:00.000Z","requestId":"req-sidechain","isSidechain":true,"message":{"usage":{"input_tokens":0,"output_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":20},"model":"claude-sonnet-4-20250514","id":"msg-sidechain"}}}}"#;
        let v: Value = serde_json::from_str(line).unwrap();
        assert!(parse_one(&v).is_none());
    }

    #[test]
    fn rejects_synthetic_and_blank_models() {
        let synthetic = r#"{"type":"assistant","timestamp":"2026-05-27T12:34:56Z","message":{"id":"m","model":"<synthetic>","usage":{"input_tokens":1,"output_tokens":1}}}"#;
        assert!(parse_one(&serde_json::from_str(synthetic).unwrap()).is_none());
    }

    #[test]
    fn rejects_blank_session_id() {
        let line = r#"{"type":"assistant","timestamp":"2026-05-27T12:34:56Z","sessionId":"","requestId":"r","message":{"id":"m","model":"claude","usage":{"input_tokens":1,"output_tokens":1}}}"#;
        assert!(parse_one(&serde_json::from_str(line).unwrap()).is_none());
    }

    #[test]
    fn rejects_non_semver_version() {
        let line = r#"{"type":"assistant","timestamp":"2026-05-27T12:34:56Z","version":"latest","message":{"id":"m","model":"claude","usage":{"input_tokens":1,"output_tokens":1}}}"#;
        assert!(parse_one(&serde_json::from_str(line).unwrap()).is_none());
    }
}
