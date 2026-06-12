//! Mimo CLI session SQLite 解析。
//!
//! Mimo 将 session 数据存储在 `~/.local/share/mimocode/mimocode.db`，
//! token 用量记录在 `message` 表的 JSON `data` 字段中。
//!
//! 数据结构：
//! - `role: "assistant"` + `tokens` 字段标记有用量的 assistant 消息
//! - `tokens.input` / `tokens.output` / `tokens.reasoning`
//! - `tokens.cache.read` / `tokens.cache.write`
//! - `time.created`（unix 毫秒）、`modelID`、`providerID`
//!
//! 口径：raw-sum（不按 message id 去重），与 Claude Code Stats 保持一致。
//! 同一 SQLite 文件不会出现跨文件重复，无需全局去重。

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;

use super::RawEntry;
use crate::stats::date::date_from_iso;
use crate::stats::format::iso_from_unix_ms;

const AGENT: &str = super::super::AGENT_MIMO;

pub(super) fn parse(db_path: &Path) -> Result<Vec<RawEntry>> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("打开 Mimo 数据库失败: {}", db_path.display()))?;

    let mut stmt = conn
        .prepare(
            "SELECT data FROM message WHERE data LIKE '%\"tokens\"%' AND data LIKE '%\"assistant\"%'",
        )
        .context("查询 Mimo message 表失败")?;

    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;

    let mut out = Vec::new();
    let mut row_errors = 0u32;
    for row in rows {
        let data_str = match row {
            Ok(s) => s,
            Err(_) => {
                row_errors += 1;
                continue;
            }
        };
        let Ok(v) = serde_json::from_str::<Value>(&data_str) else {
            row_errors += 1;
            continue;
        };
        if let Some(entry) = parse_one(&v) {
            out.push(entry);
        }
    }
    if row_errors > 0 {
        eprintln!("cx: Mimo: {row_errors} rows skipped in {}", db_path.display());
    }

    Ok(out)
}

fn parse_one(v: &Value) -> Option<RawEntry> {
    if v.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }

    let tokens = v.get("tokens")?;
    if tokens.is_null() {
        return None;
    }

    let model = v.get("modelID").and_then(Value::as_str).map(str::trim)?;
    if model.is_empty() || model == "<synthetic>" {
        return None;
    }

    let time_created = v.get("time")?.get("created")?.as_u64()?;
    let date = {
        let iso = iso_from_unix_ms(i64::try_from(time_created).ok()?);
        let d = date_from_iso(&iso);
        if d.is_empty() {
            return None;
        }
        d
    };

    let input_tokens = tokens.get("input").and_then(Value::as_u64).unwrap_or(0);
    let output_tokens = tokens.get("output").and_then(Value::as_u64).unwrap_or(0);
    let reasoning_tokens = tokens.get("reasoning").and_then(Value::as_u64).unwrap_or(0);
    let cache = tokens.get("cache");
    let cache_read = cache
        .and_then(|c| c.get("read"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_write = cache
        .and_then(|c| c.get("write"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

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
        reasoning_output_tokens: reasoning_tokens,
        dedup_primary: None,
        dedup_secondary: None,
        is_sidechain: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_json(data: &str) -> Option<RawEntry> {
        let v: Value = serde_json::from_str(data).ok()?;
        parse_one(&v)
    }

    #[test]
    fn parses_assistant_message_with_tokens() {
        let data = r#"{"role":"assistant","time":{"created":1779772684681},"modelID":"claude-opus-4-7","providerID":"anthropic","tokens":{"input":1,"output":534,"reasoning":0,"cache":{"read":70789,"write":586}}}"#;
        let entry = parse_json(data).unwrap();
        assert_eq!(entry.agent, "mimo");
        assert_eq!(entry.model, "claude-opus-4-7");
        assert_eq!(entry.input_tokens, 1);
        assert_eq!(entry.output_tokens, 534);
        assert_eq!(entry.cache_read_input_tokens, 70789);
        assert_eq!(entry.cache_creation_input_tokens, 586);
        assert!(entry.dedup_primary.is_none());
    }

    #[test]
    fn skips_user_messages() {
        let data = r#"{"role":"user","time":{"created":1779772607934},"model":{"providerID":"anthropic","modelID":"unknown"}}"#;
        assert!(parse_json(data).is_none());
    }

    #[test]
    fn skips_messages_without_tokens() {
        let data = r#"{"role":"assistant","time":{"created":1779772607934},"modelID":"test"}"#;
        assert!(parse_json(data).is_none());
    }

    #[test]
    fn skips_synthetic_model() {
        let data = r#"{"role":"assistant","time":{"created":1779772607934},"modelID":"<synthetic>","tokens":{"input":100,"output":50}}"#;
        assert!(parse_json(data).is_none());
    }

    #[test]
    fn skips_empty_model() {
        let data = r#"{"role":"assistant","time":{"created":1779772607934},"modelID":"","tokens":{"input":100,"output":50}}"#;
        assert!(parse_json(data).is_none());
    }

    #[test]
    fn skips_zero_usage() {
        let data = r#"{"role":"assistant","time":{"created":1779772607934},"modelID":"test","tokens":{"input":0,"output":0,"cache":{"read":0,"write":0}}}"#;
        assert!(parse_json(data).is_none());
    }

    #[test]
    fn parses_with_only_cache_tokens() {
        let data = r#"{"role":"assistant","time":{"created":1779772607934},"modelID":"mimo-v2.5-pro","tokens":{"input":0,"output":0,"cache":{"read":5000,"write":100}}}"#;
        let entry = parse_json(data).unwrap();
        assert_eq!(entry.model, "mimo-v2.5-pro");
        assert_eq!(entry.cache_read_input_tokens, 5000);
        assert_eq!(entry.cache_creation_input_tokens, 100);
        assert_eq!(entry.input_tokens, 0);
        assert_eq!(entry.output_tokens, 0);
    }

    #[test]
    fn parses_nonzero_reasoning_tokens() {
        let data = r#"{"role":"assistant","time":{"created":1779772607934},"modelID":"deepseek-v3","tokens":{"input":500,"output":200,"reasoning":1234,"cache":{"read":0,"write":0}}}"#;
        let entry = parse_json(data).unwrap();
        assert_eq!(entry.reasoning_output_tokens, 1234);
        assert_eq!(entry.output_tokens, 200);
    }
}
