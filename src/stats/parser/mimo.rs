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
//!
//! 导入历史过滤：mimocode 首次启动时会从 `~/.claude/projects/` 自动导入
//! Claude Code 的历史 session（`claude_import` 表记录了导入映射，
//! 含 `message_ids` JSON 数组标记每条导入的 message ID）。
//! 这些导入的 message 属于 Claude Code 产生的用量，不应重复计入 mimo。
//! 排除粒度为 message（而非 session），保留 mimo 在导入 session 里追加的
//! 原生 message。

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

    let has_import_table = has_claude_import_table(&conn)?;

    // 有 claude_import 表时，收集所有导入的 message_id 并排除。
    let imported_ids: std::collections::HashSet<String> = if has_import_table {
        collect_imported_message_ids(&conn)?
    } else {
        std::collections::HashSet::new()
    };

    let mut stmt = conn
        .prepare(
            "SELECT id, session_id, data FROM message \
             WHERE data LIKE '%\"tokens\"%' AND data LIKE '%\"assistant\"%'",
        )
        .context("查询 Mimo message 表失败")?;

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    let mut out = Vec::new();
    let mut row_errors = 0u32;
    for row in rows {
        let (msg_id, session_id, data_str) = match row {
            Ok(t) => t,
            Err(_) => {
                row_errors += 1;
                continue;
            }
        };

        // 按 message 排除：导入的 Claude Code message 不计入 mimo
        if imported_ids.contains(&msg_id) {
            continue;
        }

        let Ok(v) = serde_json::from_str::<Value>(&data_str) else {
            row_errors += 1;
            continue;
        };
        if let Some(entry) = parse_one(&v, &session_id) {
            out.push(entry);
        }
    }
    if row_errors > 0 {
        eprintln!("cx: Mimo: {row_errors} rows skipped in {}", db_path.display());
    }

    Ok(out)
}

fn has_claude_import_table(conn: &Connection) -> Result<bool> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='claude_import'")
        .context("查询 sqlite_master 失败")?;
    let exists = stmt.query_map([], |row| row.get::<_, String>(0))?.count() > 0;
    Ok(exists)
}

/// 收集 claude_import 表中所有导入的 message_id。
fn collect_imported_message_ids(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn
        .prepare("SELECT message_ids FROM claude_import")
        .context("查询 claude_import 失败")?;

    let rows = stmt.query_map([], |row| row.get::<_, Option<String>>(0))?;

    let mut ids = std::collections::HashSet::new();
    for row in rows {
        let json_str: Option<String> = match row {
            Ok(s) => s,
            Err(_) => continue,
        };
        let Some(json) = json_str else {
            continue;
        };
        if let Ok(arr) = serde_json::from_str::<Vec<String>>(&json) {
            ids.extend(arr);
        }
    }
    Ok(ids)
}

fn parse_one(v: &Value, session_id: &str) -> Option<RawEntry> {
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
        session_id: Some(session_id.to_string()),
        message_id: None, // mimo message.id 是内部生成的，对用户无意义
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_json(data: &str) -> Option<RawEntry> {
        let v: Value = serde_json::from_str(data).ok()?;
        parse_one(&v, "ses_test")
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
        assert_eq!(entry.session_id.as_deref(), Some("ses_test"));
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

    #[test]
    fn parse_excludes_imported_message_ids() {
        // 模拟：创建临时 db，插入 claude_import 和 message，验证排除
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_mimo.db");
        let conn = Connection::open(&db_path).unwrap();

        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL, time_created INTEGER NOT NULL);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, time_created INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE TABLE claude_import (source_uuid TEXT PRIMARY KEY, session_id TEXT NOT NULL, source_path TEXT NOT NULL, source_mtime INTEGER NOT NULL, time_imported INTEGER NOT NULL, message_ids TEXT);",
        ).unwrap();

        // 插入一个导入的 session
        conn.execute("INSERT INTO session (id, project_id, time_created) VALUES ('ses_imported', 'proj', 1000)", []).unwrap();
        // 插入导入的 message_ids
        conn.execute(
            "INSERT INTO claude_import (source_uuid, session_id, source_path, source_mtime, time_imported, message_ids)
             VALUES ('uuid1', 'ses_imported', '/path', 1000, 1000, '[\"msg_import1\",\"msg_import2\"]')",
            [],
        ).unwrap();

        // 插入导入的 message（应被排除）
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data)
             VALUES ('msg_import1', 'ses_imported', 1779772684681, '{\"role\":\"assistant\",\"time\":{\"created\":1779772684681},\"modelID\":\"claude-opus-4-7\",\"tokens\":{\"input\":100,\"output\":50}}')",
            [],
        ).unwrap();
        // 插入 mimo 原生的 message（应保留）
        conn.execute(
            "INSERT INTO message (id, session_id, time_created, data)
             VALUES ('msg_native1', 'ses_imported', 1781188251308, '{\"role\":\"assistant\",\"time\":{\"created\":1781188251308},\"modelID\":\"mimo-auto\",\"tokens\":{\"input\":10,\"output\":5}}')",
            [],
        ).unwrap();

        let entries = parse(&db_path).unwrap();
        // msg_import1 应被排除，只保留 msg_native1
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].model, "mimo-auto");
        assert_eq!(entries[0].input_tokens, 10);
    }
}