//! Remora session jsonl 解析。
//!
//! Remora 管理多个 agent session，日志存放在 `~/.remora/projects/` 下，
//! 格式与 OMP session jsonl 相似：
//! - `type: "message"` + `message.role: "assistant"` 标记 assistant 消息
//! - usage 字段为 camelCase：`input` / `output` / `cacheRead` / `cacheWrite`
//! - `type: "session"` 行提供 session header（含 id、timestamp、cwd）
//! - `type: "compaction"` 等非 message 类型不含 per-message usage，跳过
//!
//! 口径：raw-sum（不按 message id 去重），与 OMP/Claude 保持一致。
//!
//! 注意：session_id 从 `type: "session"` header 行提取。append-scan 只解析
//! 文件尾部，看不到 header，因此追加部分的 session_id 为 None。此字段仅
//! 用于信息展示和溯源，不参与去重或聚合，所以可接受；source_path 列已
//! 可唯一标识 session 来源。

use serde_json::Value;

use super::RawEntry;
use crate::stats::date::date_field;

const AGENT: &str = super::super::AGENT_REMORA;

pub(super) fn parse(content: &str) -> Vec<RawEntry> {
    let mut out: Vec<RawEntry> = Vec::new();
    let mut session_id: Option<String> = None;

    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };

        // 从 type: "session" header 行提取 session_id
        if v.get("type").and_then(Value::as_str) == Some("session") {
            if let Some(id) = v.get("id").and_then(Value::as_str) {
                session_id = Some(id.to_string());
            }
            continue;
        }

        if let Some(entry) = parse_one(&v, session_id.as_deref()) {
            out.push(entry);
        }
    }
    out
}

fn parse_one(v: &Value, session_id: Option<&str>) -> Option<RawEntry> {
    // Remora assistant 消息为 type="message" + role="assistant"
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

    // Remora usage 字段为 camelCase，与 OMP 一致
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
        session_id: session_id.map(str::to_string),
        message_id: v.get("id").and_then(Value::as_str).map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_assistant_message() {
        // 使用本地时区日期：UTC "2026-06-19T16:06:52.416Z" 在 UTC+8 为 2026-06-20
        // 为避免时区依赖，使用不含时区偏移的日期字符串
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-19T00:00:00.000Z","message":{"role":"assistant","provider":"dashscope","model":"qwen3.7-max","usage":{"input":832,"output":109,"cacheRead":2048,"cacheWrite":500,"totalTokens":3489}}}"#;
        let entries = parse(line);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.agent, "remora");
        assert_eq!(e.model, "qwen3.7-max");
        assert_eq!(e.date, "2026-06-19");
        assert_eq!(e.input_tokens, 832);
        assert_eq!(e.output_tokens, 109);
        assert_eq!(e.cache_read_input_tokens, 2048);
        assert_eq!(e.cache_creation_input_tokens, 500);
        assert_eq!(e.session_id, None);
        assert_eq!(e.message_id, Some("abc".to_string()));
    }

    #[test]
    fn extracts_session_id_from_session_header() {
        let content = r#"{"type":"session","version":3,"id":"003b21eb-a30a-44ee-a50a-b74c09f90759","timestamp":"2026-06-19T16:04:52.232Z","cwd":"/Users/test/project"}
{"type":"message","id":"msg1","parentId":"xyz","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"assistant","model":"qwen3.7-max","usage":{"input":832,"output":109,"cacheRead":0,"cacheWrite":0,"totalTokens":941}}}"#;
        let entries = parse(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].session_id,
            Some("003b21eb-a30a-44ee-a50a-b74c09f90759".to_string())
        );
    }

    #[test]
    fn skips_non_assistant_messages() {
        let line = r#"{"type":"message","id":"abc","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_tool_result_messages() {
        let line = r#"{"type":"message","id":"abc","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"toolResult","content":"result"}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_error_with_zero_usage() {
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"assistant","model":"qwen3.7-max","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0}}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_empty_model() {
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"assistant","model":"","usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0}}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_synthetic_model() {
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"assistant","model":"<synthetic>","usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0}}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_compaction_entries() {
        let line = r#"{"type":"compaction","id":"c1","parentId":"p1","timestamp":"2026-06-19T16:06:52.416Z","summary":"...","firstKeptEntryId":"e1","tokensBefore":5000}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_model_change_entries() {
        let line = r#"{"type":"model_change","id":"mc1","parentId":"p1","timestamp":"2026-06-19T16:04:52.234Z","provider":"dashscope","modelId":"qwen3.7-max"}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn later_session_header_overrides_first() {
        let content = r#"{"type":"session","version":3,"id":"sess-1","timestamp":"2026-06-19T16:00:00.000Z","cwd":"/test"}
{"type":"message","id":"m1","timestamp":"2026-06-19T12:00:00.000Z","message":{"role":"assistant","model":"qwen3.7-max","usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0}}}
{"type":"session","version":3,"id":"sess-2","timestamp":"2026-06-19T17:00:00.000Z","cwd":"/test"}
{"type":"message","id":"m2","timestamp":"2026-06-19T12:00:00.000Z","message":{"role":"assistant","model":"qwen3.7-max","usage":{"input":200,"output":100,"cacheRead":0,"cacheWrite":0}}}"#;
        let entries = parse(content);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].session_id, Some("sess-1".to_string()));
        assert_eq!(entries[1].session_id, Some("sess-2".to_string()));
    }
}
