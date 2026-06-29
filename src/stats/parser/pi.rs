//! pi coding-agent session jsonl 解析。
//!
//! pi 将 agent session 存放在 `~/.pi/agent/sessions/` 下，
//! 格式为树形 JSONL（与 Remora 共享同一 session 格式）：
//! - `type: "session"` 行提供 session header（含 id、timestamp、cwd）
//! - `type: "model_change"` 行记录模型切换（provider + modelId）
//! - `type: "thinking_level_change"` 行记录思考级别变化
//! - `type: "message"` + `message.role: "assistant"` 标记 assistant 消息
//! - usage 字段为 camelCase：`input` / `output` / `cacheRead` / `cacheWrite`
//!   此外还有 `cacheWrite1h` 字段（pi 独有，暂不参与统计）
//! - `type: "leaf"` / `type: "label"` / `type: "compaction"` 等不含 per-message usage，跳过
//!
//! 口径：raw-sum（不按 message id 去重），与 Remora/OMP/Claude 保持一致。
//!
//! 注意：session_id 从 `type: "session"` header 行提取。append-scan 只解析
//! 文件尾部，看不到 header，因此追加部分的 session_id 为 None。此字段仅
//! 用于信息展示和溯源，不参与去重或聚合，所以可接受；source_path 列已
//! 可唯一标识 session 来源。

use serde_json::Value;

use super::RawEntry;
use crate::stats::date::date_field;

const AGENT: &str = super::super::AGENT_PI;

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
    // pi assistant 消息为 type="message" + role="assistant"
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

    // pi usage 字段为 camelCase
    let input_tokens = usage.get("input").and_then(Value::as_u64).unwrap_or(0);
    let output_tokens = usage.get("output").and_then(Value::as_u64).unwrap_or(0);
    let cache_read = usage.get("cacheRead").and_then(Value::as_u64).unwrap_or(0);
    let cache_write = usage.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0);
    // pi 独有 cacheWrite1h 字段，暂不单独统计，归入 cache_creation 总量不合适，
    // 因为语义是"1 小时内的 cache 写入"，与 cacheWrite 可能重叠。
    // 当前做法：忽略 cacheWrite1h，仅统计 cacheWrite（与 Remora 口径一致）。

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
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-19T00:00:00.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-opus-4-7","usage":{"input":832,"output":109,"cacheRead":2048,"cacheWrite":500,"totalTokens":3489,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0},"cacheWrite1h":0}}}"#;
        let entries = parse(line);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e.agent, "pi");
        assert_eq!(e.model, "claude-opus-4-7");
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
        let content = r#"{"type":"session","version":3,"id":"019f13c0-c485-77aa-87ef-9d387089894c","timestamp":"2026-06-29T14:20:28.166Z","cwd":"/Users/test/project"}
{"type":"model_change","id":"387e98ac","parentId":null,"timestamp":"2026-06-29T14:20:28.215Z","provider":"anthropic","modelId":"claude-opus-4-7"}
{"type":"message","id":"msg1","parentId":"387e98ac","timestamp":"2026-06-29T14:20:30.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-opus-4-7","usage":{"input":832,"output":109,"cacheRead":0,"cacheWrite":0,"totalTokens":941}}}"#;
        let entries = parse(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].session_id,
            Some("019f13c0-c485-77aa-87ef-9d387089894c".to_string())
        );
    }

    #[test]
    fn skips_non_assistant_messages() {
        let line = r#"{"type":"message","id":"abc","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"user","content":[{"type":"text","text":"hello"}]}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_tool_result_messages() {
        let line = r#"{"type":"message","id":"abc","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"toolResult","toolCallId":"toolu_123","toolName":"bash","content":[{"type":"text","text":"result"}],"isError":false}}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_error_with_zero_usage() {
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-19T16:06:52.416Z","message":{"role":"assistant","model":"claude-opus-4-7","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"stopReason":"error","errorMessage":"401 authentication_error"}}"#;
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
        let line = r#"{"type":"model_change","id":"mc1","parentId":"p1","timestamp":"2026-06-19T16:04:52.234Z","provider":"anthropic","modelId":"claude-opus-4-7"}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn skips_thinking_level_change_entries() {
        let line = r#"{"type":"thinking_level_change","id":"tl1","parentId":"mc1","timestamp":"2026-06-19T16:04:52.234Z","thinkingLevel":"high"}"#;
        assert!(parse(line).is_empty());
    }

    #[test]
    fn ignores_cache_write1h() {
        // cacheWrite1h 存在但不应单独统计
        let line = r#"{"type":"message","id":"abc","parentId":"xyz","timestamp":"2026-06-19T00:00:00.000Z","message":{"role":"assistant","provider":"anthropic","model":"claude-opus-4-7","usage":{"input":832,"output":109,"cacheRead":2048,"cacheWrite":500,"totalTokens":3489,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0},"cacheWrite1h":100}}}"#;
        let entries = parse(line);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].cache_creation_input_tokens, 500);
    }

    #[test]
    fn later_session_header_overrides_first() {
        let content = r#"{"type":"session","version":3,"id":"sess-1","timestamp":"2026-06-19T16:00:00.000Z","cwd":"/test"}
{"type":"message","id":"m1","timestamp":"2026-06-19T12:00:00.000Z","message":{"role":"assistant","model":"claude-opus-4-7","usage":{"input":100,"output":50,"cacheRead":0,"cacheWrite":0}}}
{"type":"session","version":3,"id":"sess-2","timestamp":"2026-06-19T17:00:00.000Z","cwd":"/test"}
{"type":"message","id":"m2","timestamp":"2026-06-19T12:00:00.000Z","message":{"role":"assistant","model":"claude-opus-4-7","usage":{"input":200,"output":100,"cacheRead":0,"cacheWrite":0}}}"#;
        let entries = parse(content);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].session_id, Some("sess-1".to_string()));
        assert_eq!(entries[1].session_id, Some("sess-2".to_string()));
    }
}
