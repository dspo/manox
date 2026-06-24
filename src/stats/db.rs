//! SQLite 持久化用量缓存（~/.config/cx/cx.db）。
//!
//! v3 schema：per-message 明细表替代 v1 的 per-day 聚合表，
//! 单一真相来源，聚合用 SQL GROUP BY 实时计算。
//!
//! 增量更新：append-only 文件按尾部增量解析；全量刷新时走事务化 DELETE + INSERT。
//! 跨文件去重（codex/copilot）在 insert 时处理。

use anyhow::{Context, Result};
use dirs::home_dir;
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};

use super::parser::RawEntry;
use super::types::UsageRecord;

pub(super) const DB_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ScanState {
    pub(super) mtime_secs: u64,
    pub(super) size: u64,
    pub(super) parsed_upto_bytes: u64,
    pub(super) file_id: Option<String>,
}

pub(super) fn db_path() -> Result<PathBuf> {
    let home = home_dir().context("无法解析用户主目录")?;
    Ok(home.join(".config/cx/cx.db"))
}

pub(super) fn open_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建数据库目录失败: {}", parent.display()))?;
    }
    let conn =
        Connection::open(path).with_context(|| format!("打开数据库失败: {}", path.display()))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    Ok(conn)
}

pub(super) fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS scanned_files (
            path       TEXT PRIMARY KEY,
            mtime_secs INTEGER NOT NULL,
            size       INTEGER NOT NULL,
            parsed_upto_bytes INTEGER NOT NULL DEFAULT 0,
            file_id    TEXT
        );

        CREATE TABLE IF NOT EXISTS messages (
            id                      INTEGER PRIMARY KEY AUTOINCREMENT,
            agent                   TEXT NOT NULL,
            model                   TEXT NOT NULL,
            date                    TEXT NOT NULL,
            input_tokens            INTEGER NOT NULL DEFAULT 0,
            output_tokens           INTEGER NOT NULL DEFAULT 0,
            cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
            cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
            reasoning_output_tokens INTEGER NOT NULL DEFAULT 0,
            session_id              TEXT,
            message_id              TEXT,
            dedup_primary           TEXT,
            dedup_secondary         TEXT,
            is_sidechain            INTEGER NOT NULL DEFAULT 0,
            source_path             TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_messages_agg
            ON messages (agent, model, date);
        CREATE INDEX IF NOT EXISTS idx_messages_source
            ON messages (source_path);
        CREATE INDEX IF NOT EXISTS idx_messages_dedup
            ON messages (dedup_primary) WHERE dedup_primary IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_messages_agent_dedup
            ON messages (agent, dedup_primary) WHERE dedup_primary IS NOT NULL;",
    )?;

    let current_version: u32 = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);

    if current_version == 1 {
        // v1 was an unreleased aggregate-cache schema. Reset it instead of
        // mixing per-day aggregates into the final per-message schema.
        with_transaction(conn, |tx| {
            tx.execute("DROP TABLE IF EXISTS usage_records", [])?;
            tx.execute("DROP TABLE IF EXISTS scanned_files", [])?;
            tx.execute("DELETE FROM messages", [])?;
            tx.execute_batch(
                "CREATE TABLE scanned_files (
                    path       TEXT PRIMARY KEY,
                    mtime_secs INTEGER NOT NULL,
                    size       INTEGER NOT NULL,
                    parsed_upto_bytes INTEGER NOT NULL DEFAULT 0,
                    file_id    TEXT
                );",
            )?;
            set_schema_version(tx, DB_VERSION)?;
            Ok(())
        })?;
    } else if current_version == 2 {
        ensure_scanned_files_columns(conn)?;
        set_schema_version(conn, DB_VERSION)?;
    } else if current_version == 0 {
        ensure_scanned_files_columns(conn)?;
        set_schema_version(conn, DB_VERSION)?;
    }

    Ok(())
}

fn set_schema_version(conn: &Connection, version: u32) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
        params![version.to_string()],
    )?;
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let pragma = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&pragma)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_scanned_files_columns(conn: &Connection) -> Result<()> {
    if !column_exists(conn, "scanned_files", "parsed_upto_bytes")? {
        conn.execute(
            "ALTER TABLE scanned_files ADD COLUMN parsed_upto_bytes INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
        conn.execute(
            "UPDATE scanned_files SET parsed_upto_bytes = size WHERE parsed_upto_bytes = 0",
            [],
        )?;
    }
    if !column_exists(conn, "scanned_files", "file_id")? {
        conn.execute("ALTER TABLE scanned_files ADD COLUMN file_id TEXT", [])?;
    }
    Ok(())
}

/// 读取某源文件的扫描状态。
pub(super) fn load_scan_state(conn: &Connection, path: &str) -> Option<ScanState> {
    conn.query_row(
        "SELECT mtime_secs, size, parsed_upto_bytes, file_id
         FROM scanned_files WHERE path = ?1",
        params![path],
        |row| {
            Ok(ScanState {
                mtime_secs: row.get(0)?,
                size: row.get(1)?,
                parsed_upto_bytes: row.get(2)?,
                file_id: row.get(3)?,
            })
        },
    )
    .ok()
}

/// 检查源文件是否已扫描且未变化（mtime + size 匹配）。
#[cfg(test)]
pub(super) fn file_unchanged(conn: &Connection, path: &str, mtime: u64, size: u64) -> bool {
    load_scan_state(conn, path).is_some_and(|state| state.mtime_secs == mtime && state.size == size)
}

/// 删除某源文件的所有旧 message，为重新解析做准备。
pub(super) fn delete_messages_for_source(conn: &Connection, source_path: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM messages WHERE source_path = ?1",
        params![source_path],
    )?;
    Ok(())
}

/// 插入一条 per-message 记录。
///
/// 对于有 dedup_primary 的条目（codex/copilot），需要跨文件去重：
/// 如果同 agent + dedup_primary 的条目已存在且 token 更多，则跳过；
/// 如果新条目 token 更多，则替换旧的。
fn insert_one(conn: &Connection, entry: &RawEntry, source_path: &str) -> Result<bool> {
    let is_sidechain_i32: i32 = if entry.is_sidechain { 1 } else { 0 };

    if let Some(ref primary) = entry.dedup_primary {
        // 跨文件去重：查找同 agent + dedup_primary 的已有条目
        let existing: Option<(i32, u64, u64, u64, u64)> = conn
            .query_row(
                "SELECT is_sidechain, input_tokens, output_tokens,
                        cache_read_input_tokens, cache_creation_input_tokens
                 FROM messages
                 WHERE agent = ?1 AND dedup_primary = ?2
                 LIMIT 1",
                params![entry.agent, primary],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .ok();

        if let Some((ex_sidechain, ex_in, ex_out, ex_cr, ex_cc)) = existing {
            // 去 weight: 非 sidechain 优先于 sidechain；
            // 同 sidechain 状态时，token 更多者优先。
            let should_replace = if entry.is_sidechain && ex_sidechain == 0 {
                false // sidechain 不能替换非 sidechain
            } else if !entry.is_sidechain && ex_sidechain != 0 {
                true // 非 sidechain 一定替换 sidechain
            } else {
                // 同类：比 token
                let new_total = entry.input_tokens
                    + entry.output_tokens
                    + entry.cache_read_input_tokens
                    + entry.cache_creation_input_tokens;
                let ex_total = ex_in + ex_out + ex_cr + ex_cc;
                new_total > ex_total
            };

            if should_replace {
                // 删除旧条目再插入新的
                conn.execute(
                    "DELETE FROM messages WHERE agent = ?1 AND dedup_primary = ?2",
                    params![entry.agent, primary],
                )?;
            } else {
                // 旧条目更好，跳过新条目
                return Ok(false);
            }
        }
    }

    conn.execute(
        "INSERT INTO messages (
            agent, model, date,
            input_tokens, output_tokens,
            cache_read_input_tokens, cache_creation_input_tokens,
            reasoning_output_tokens,
            session_id, message_id,
            dedup_primary, dedup_secondary, is_sidechain,
            source_path
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            entry.agent,
            entry.model,
            entry.date,
            entry.input_tokens,
            entry.output_tokens,
            entry.cache_read_input_tokens,
            entry.cache_creation_input_tokens,
            entry.reasoning_output_tokens,
            entry.session_id,
            entry.message_id,
            entry.dedup_primary,
            entry.dedup_secondary,
            is_sidechain_i32,
            source_path,
        ],
    )?;
    Ok(true)
}

/// 批量插入某源文件的 per-message 记录（带跨文件去重）。
pub(super) fn insert_file_messages(
    conn: &Connection,
    entries: &[RawEntry],
    source_path: &str,
) -> Result<u64> {
    let mut inserted: u64 = 0;
    for entry in entries {
        if insert_one(conn, entry, source_path)? {
            inserted += 1;
        }
    }
    Ok(inserted)
}

fn with_transaction<T, F>(conn: &Connection, f: F) -> Result<T>
where
    F: FnOnce(&Connection) -> Result<T>,
{
    conn.execute_batch("BEGIN IMMEDIATE")?;
    match f(conn) {
        Ok(value) => {
            conn.execute_batch("COMMIT")?;
            Ok(value)
        }
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

/// 记录源文件扫描状态（mtime + size）。
pub(super) fn mark_file_scanned(
    conn: &Connection,
    path: &str,
    mtime: u64,
    size: u64,
    parsed_upto_bytes: u64,
    file_id: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO scanned_files (path, mtime_secs, size, parsed_upto_bytes, file_id)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![path, mtime, size, parsed_upto_bytes, file_id],
    )?;
    Ok(())
}

pub(super) fn replace_file_messages(
    conn: &Connection,
    entries: &[RawEntry],
    source_path: &str,
    state: &ScanState,
) -> Result<u64> {
    with_transaction(conn, |tx| {
        delete_messages_for_source(tx, source_path)?;
        let inserted = insert_file_messages(tx, entries, source_path)?;
        mark_file_scanned(
            tx,
            source_path,
            state.mtime_secs,
            state.size,
            state.parsed_upto_bytes,
            state.file_id.as_deref(),
        )?;
        Ok(inserted)
    })
}

pub(super) fn append_file_messages(
    conn: &Connection,
    entries: &[RawEntry],
    source_path: &str,
    state: &ScanState,
) -> Result<u64> {
    with_transaction(conn, |tx| {
        let inserted = insert_file_messages(tx, entries, source_path)?;
        mark_file_scanned(
            tx,
            source_path,
            state.mtime_secs,
            state.size,
            state.parsed_upto_bytes,
            state.file_id.as_deref(),
        )?;
        Ok(inserted)
    })
}

/// 从 messages 表按 (agent, model, date) 聚合，返回 UsageRecord 列表。
///
/// 模型名归一化在聚合时完成（SQL 层不方便做，在 Rust 层处理）。
pub(super) fn load_aggregated(conn: &Connection) -> Result<Vec<UsageRecord>> {
    let mut stmt = conn.prepare(
        "SELECT agent, model, date,
                SUM(input_tokens)  AS in_tokens,
                SUM(output_tokens) AS out_tokens,
                SUM(cache_read_input_tokens)  AS cache_read_input_tokens,
                SUM(cache_creation_input_tokens) AS cache_creation_input_tokens
         FROM messages
         GROUP BY agent, model, date
         ORDER BY agent, model, date",
    )?;

    let rows = stmt.query_map([], |row| {
        let in_tokens: u64 = row.get(3)?;
        let out_tokens: u64 = row.get(4)?;
        let cache_read: u64 = row.get(5)?;
        let cache_create: u64 = row.get(6)?;
        Ok(RawAgg {
            agent: row.get(0)?,
            model: row.get(1)?,
            date: row.get(2)?,
            in_tokens,
            out_tokens,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_create,
        })
    })?;

    // 模型名归一化并二次聚合（claude-opus-4.7 → claude-opus-4-7 等）
    let mut acc: std::collections::BTreeMap<(String, String, String), UsageRecord> =
        std::collections::BTreeMap::new();
    for row in rows {
        let r = row?;
        let model = super::normalize_model_name(&r.model);
        let key = (r.agent.clone(), model.clone(), r.date.clone());
        let entry = acc.entry(key).or_insert_with(|| UsageRecord {
            agent: r.agent.clone(),
            model,
            date: r.date.clone(),
            in_tokens: 0,
            total_tokens: 0,
            out_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        });
        entry.in_tokens += r.in_tokens;
        entry.out_tokens += r.out_tokens;
        entry.total_tokens += r.in_tokens + r.out_tokens;
        entry.cache_read_input_tokens += r.cache_read_input_tokens;
        entry.cache_creation_input_tokens += r.cache_creation_input_tokens;
    }

    Ok(acc.into_values().collect())
}

/// 清理不再存在的源目录下的 stale 条目（scanned_files + messages）。
pub(super) fn cleanup_stale_entries(
    conn: &Connection,
    active_source_roots: &[&Path],
    active_extra_files: &[&Path],
) -> Result<()> {
    let paths: Vec<String> = {
        let mut stmt = conn.prepare("SELECT path FROM scanned_files")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.filter_map(Result::ok).collect()
    };

    for path_str in &paths {
        let path = Path::new(path_str);
        let is_active = active_source_roots
            .iter()
            .any(|root| path.starts_with(root))
            || active_extra_files.iter().any(|extra| path == *extra);
        if !is_active {
            // 先删该文件的所有 message，再删 scanned_files 记录
            conn.execute(
                "DELETE FROM messages WHERE source_path = ?1",
                params![path_str],
            )?;
            conn.execute(
                "DELETE FROM scanned_files WHERE path = ?1",
                params![path_str],
            )?;
        }
    }
    Ok(())
}

/// SQL 聚合行的中间结构（模型名未归一化）。
struct RawAgg {
    agent: String,
    model: String,
    date: String,
    in_tokens: u64,
    out_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (Connection, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let conn = open_db(&path).unwrap();
        init_schema(&conn).unwrap();
        (conn, dir)
    }

    fn sample_entry() -> RawEntry {
        RawEntry {
            agent: "test".to_string(),
            model: "m".to_string(),
            date: "2026-01-01".to_string(),
            input_tokens: 100,
            output_tokens: 50,
            cache_read_input_tokens: 10,
            cache_creation_input_tokens: 5,
            reasoning_output_tokens: 0,
            dedup_primary: None,
            dedup_secondary: None,
            is_sidechain: false,
            session_id: None,
            message_id: None,
        }
    }

    #[test]
    fn schema_initialization_is_idempotent() {
        let (conn, _dir) = temp_db();
        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap();
    }

    #[test]
    fn scan_state_round_trip() {
        let (conn, _dir) = temp_db();
        mark_file_scanned(&conn, "/tmp/file.jsonl", 100, 200, 180, Some("dev:inode")).unwrap();
        let state = load_scan_state(&conn, "/tmp/file.jsonl").unwrap();
        assert_eq!(
            state,
            ScanState {
                mtime_secs: 100,
                size: 200,
                parsed_upto_bytes: 180,
                file_id: Some("dev:inode".to_string()),
            }
        );
    }

    #[test]
    fn insert_and_aggregate_messages() {
        let (conn, _dir) = temp_db();
        let entries = vec![
            sample_entry(),
            RawEntry {
                agent: "test".to_string(),
                model: "m".to_string(),
                date: "2026-01-01".to_string(),
                input_tokens: 200,
                output_tokens: 100,
                cache_read_input_tokens: 20,
                cache_creation_input_tokens: 10,
                reasoning_output_tokens: 0,
                dedup_primary: None,
                dedup_secondary: None,
                is_sidechain: false,
                session_id: None,
                message_id: None,
            },
        ];
        let inserted = insert_file_messages(&conn, &entries, "/test/file.jsonl").unwrap();
        assert_eq!(inserted, 2);

        mark_file_scanned(&conn, "/test/file.jsonl", 1000, 200, 200, None).unwrap();
        assert!(file_unchanged(&conn, "/test/file.jsonl", 1000, 200));

        let records = load_aggregated(&conn).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].in_tokens, 300);
        assert_eq!(records[0].out_tokens, 150);
        assert_eq!(records[0].cache_read_input_tokens, 30);
    }

    #[test]
    fn dedup_keeps_larger_token_count() {
        let (conn, _dir) = temp_db();
        // 同 dedup_primary，第一个 token 少
        let smaller = RawEntry {
            agent: "codex".to_string(),
            model: "gpt-5".to_string(),
            date: "2026-05-27".to_string(),
            input_tokens: 50,
            output_tokens: 3,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            dedup_primary: Some("sess-A|ts-1".to_string()),
            dedup_secondary: Some("50/0/0/3/0".to_string()),
            is_sidechain: false,
            session_id: None,
            message_id: None,
        };
        let bigger = RawEntry {
            agent: "codex".to_string(),
            model: "gpt-5".to_string(),
            date: "2026-05-27".to_string(),
            input_tokens: 100,
            output_tokens: 7,
            cache_read_input_tokens: 20,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            dedup_primary: Some("sess-A|ts-1".to_string()),
            dedup_secondary: Some("100/20/0/7/0".to_string()),
            is_sidechain: false,
            session_id: None,
            message_id: None,
        };
        // 先插入小的，再插入大的 → 大的替换小的
        insert_file_messages(&conn, &[smaller.clone()], "/file1.jsonl").unwrap();
        insert_file_messages(&conn, &[bigger.clone()], "/file2.jsonl").unwrap();

        let records = load_aggregated(&conn).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].in_tokens, 100);
        assert_eq!(records[0].out_tokens, 7);
    }

    #[test]
    fn dedup_sidechain_does_not_replace_parent() {
        let (conn, _dir) = temp_db();
        let parent = RawEntry {
            agent: "claude".to_string(),
            model: "m".to_string(),
            date: "2026-05-27".to_string(),
            input_tokens: 100,
            output_tokens: 7,
            cache_read_input_tokens: 20,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            dedup_primary: Some("msg-1".to_string()),
            dedup_secondary: Some("100/20/0/7/0".to_string()),
            is_sidechain: false,
            session_id: None,
            message_id: None,
        };
        let replay = RawEntry {
            agent: "claude".to_string(),
            model: "m".to_string(),
            date: "2026-05-27".to_string(),
            input_tokens: 50,
            output_tokens: 7,
            cache_read_input_tokens: 50_000,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            dedup_primary: Some("msg-1".to_string()),
            dedup_secondary: Some("50/50000/0/7/0".to_string()),
            is_sidechain: true,
            session_id: None,
            message_id: None,
        };
        // 先插入 parent，再插入 sidechain replay → replay 不应替换 parent
        insert_file_messages(&conn, &[parent.clone()], "/file1.jsonl").unwrap();
        insert_file_messages(&conn, &[replay], "/file2.jsonl").unwrap();

        let records = load_aggregated(&conn).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].cache_read_input_tokens, 20); // parent 的值
    }

    #[test]
    fn dedup_parent_replaces_sidechain() {
        let (conn, _dir) = temp_db();
        let replay = RawEntry {
            agent: "claude".to_string(),
            model: "m".to_string(),
            date: "2026-05-27".to_string(),
            input_tokens: 1,
            output_tokens: 1,
            cache_read_input_tokens: 1,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            dedup_primary: Some("msg-1".to_string()),
            dedup_secondary: Some("1/1/0/1/0".to_string()),
            is_sidechain: true,
            session_id: None,
            message_id: None,
        };
        let parent = RawEntry {
            agent: "claude".to_string(),
            model: "m".to_string(),
            date: "2026-05-27".to_string(),
            input_tokens: 10,
            output_tokens: 10,
            cache_read_input_tokens: 10,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            dedup_primary: Some("msg-1".to_string()),
            dedup_secondary: Some("10/10/0/10/0".to_string()),
            is_sidechain: false,
            session_id: None,
            message_id: None,
        };
        // 先插入 sidechain，再插入 parent → parent 替换 sidechain
        insert_file_messages(&conn, &[replay], "/file1.jsonl").unwrap();
        insert_file_messages(&conn, &[parent.clone()], "/file2.jsonl").unwrap();

        let records = load_aggregated(&conn).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].in_tokens, 10); // parent 的值
    }

    #[test]
    fn entries_without_dedup_primary_always_inserted() {
        let (conn, _dir) = temp_db();
        let e1 = RawEntry {
            agent: "x".to_string(),
            model: "m".to_string(),
            date: "2026-01-01".to_string(),
            input_tokens: 1,
            output_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            dedup_primary: None,
            dedup_secondary: None,
            is_sidechain: false,
            session_id: None,
            message_id: None,
        };
        let e2 = RawEntry {
            agent: "x".to_string(),
            model: "m".to_string(),
            date: "2026-01-01".to_string(),
            input_tokens: 2,
            output_tokens: 2,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            dedup_primary: None,
            dedup_secondary: None,
            is_sidechain: false,
            session_id: None,
            message_id: None,
        };
        insert_file_messages(&conn, &[e1, e2], "/file.jsonl").unwrap();

        let records = load_aggregated(&conn).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].in_tokens, 3); // 1 + 2
    }

    #[test]
    fn delete_messages_for_source_and_reinsert() {
        let (conn, _dir) = temp_db();
        let old = vec![sample_entry()];
        insert_file_messages(&conn, &old, "/test/file.jsonl").unwrap();

        // 文件变化：删除旧数据
        delete_messages_for_source(&conn, "/test/file.jsonl").unwrap();

        let updated = vec![RawEntry {
            agent: "test".to_string(),
            model: "m".to_string(),
            date: "2026-01-01".to_string(),
            input_tokens: 500,
            output_tokens: 200,
            cache_read_input_tokens: 50,
            cache_creation_input_tokens: 10,
            reasoning_output_tokens: 0,
            dedup_primary: None,
            dedup_secondary: None,
            is_sidechain: false,
            session_id: None,
            message_id: None,
        }];
        insert_file_messages(&conn, &updated, "/test/file.jsonl").unwrap();
        mark_file_scanned(&conn, "/test/file.jsonl", 1001, 200, 200, None).unwrap();

        let records = load_aggregated(&conn).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].in_tokens, 500);
    }

    #[test]
    fn cleanup_stale_entries_removes_inactive_paths() {
        let (conn, _dir) = temp_db();
        let active_entries = vec![sample_entry()];
        let stale_entries = vec![RawEntry {
            agent: "removed".to_string(),
            model: "m".to_string(),
            date: "2026-01-01".to_string(),
            input_tokens: 1,
            output_tokens: 1,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            dedup_primary: None,
            dedup_secondary: None,
            is_sidechain: false,
            session_id: None,
            message_id: None,
        }];
        insert_file_messages(&conn, &active_entries, "/active/file.jsonl").unwrap();
        mark_file_scanned(&conn, "/active/file.jsonl", 1000, 200, 200, None).unwrap();
        insert_file_messages(&conn, &stale_entries, "/removed/file.jsonl").unwrap();
        mark_file_scanned(&conn, "/removed/file.jsonl", 1000, 200, 200, None).unwrap();

        cleanup_stale_entries(&conn, &[Path::new("/active")], &[]).unwrap();

        // active 保留，removed 被删（scanned_files + messages）
        assert!(file_unchanged(&conn, "/active/file.jsonl", 1000, 200));
        assert!(!file_unchanged(&conn, "/removed/file.jsonl", 1000, 200));
        let records = load_aggregated(&conn).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].agent, "test");
    }

    #[test]
    fn cleanup_stale_entries_preserves_extra_files() {
        let (conn, _dir) = temp_db();
        let entries = vec![sample_entry()];
        insert_file_messages(&conn, &entries, "/extra/copilot.log").unwrap();
        mark_file_scanned(&conn, "/extra/copilot.log", 1000, 200, 200, None).unwrap();

        cleanup_stale_entries(
            &conn,
            &[Path::new("/active")],
            &[Path::new("/extra/copilot.log")],
        )
        .unwrap();

        assert!(file_unchanged(&conn, "/extra/copilot.log", 1000, 200));
    }

    #[test]
    fn migration_v1_to_v3_resets_unreleased_aggregate_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let conn = open_db(&path).unwrap();

        // 先创建 v1 schema 并插入旧数据
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS scanned_files (
                path       TEXT PRIMARY KEY,
                mtime_secs INTEGER NOT NULL,
                size       INTEGER NOT NULL,
                raw_json   TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS usage_records (
                agent      TEXT NOT NULL,
                model      TEXT NOT NULL,
                date       TEXT NOT NULL,
                in_tokens  INTEGER NOT NULL DEFAULT 0,
                out_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_input_tokens  INTEGER NOT NULL DEFAULT 0,
                cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (agent, model, date)
            );
            INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '1');
            INSERT INTO usage_records (agent, model, date, in_tokens, out_tokens,
                cache_read_input_tokens, cache_creation_input_tokens)
                VALUES ('claude', 'opus', '2026-01-01', 999, 0, 0, 0);",
        )
        .unwrap();

        // 运行 v3 init_schema
        init_schema(&conn).unwrap();

        // v1 是未发布的旧聚合缓存；终版 schema 不混入旧聚合数据。
        assert!(
            conn.query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='usage_records'",
                [],
                |row| row.get::<_, String>(0),
            )
            .is_err()
        );

        // messages 表应存在
        assert!(
            conn.query_row(
                "SELECT name FROM sqlite_master WHERE type='table' AND name='messages'",
                [],
                |row| row.get::<_, String>(0),
            )
            .is_ok()
        );

        let records = load_aggregated(&conn).unwrap();
        assert!(records.is_empty());

        // schema_version 应为 3
        let version_str: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let version: u32 = version_str.parse().unwrap();
        assert_eq!(version, DB_VERSION);
    }

    #[test]
    fn migration_v2_to_v3_preserves_existing_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let conn = open_db(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS scanned_files (
                path       TEXT PRIMARY KEY,
                mtime_secs INTEGER NOT NULL,
                size       INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                agent TEXT NOT NULL,
                model TEXT NOT NULL,
                date TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_output_tokens INTEGER NOT NULL DEFAULT 0,
                session_id TEXT,
                message_id TEXT,
                dedup_primary TEXT,
                dedup_secondary TEXT,
                is_sidechain INTEGER NOT NULL DEFAULT 0,
                source_path TEXT NOT NULL
            );
            INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '2');
            INSERT INTO scanned_files (path, mtime_secs, size)
                VALUES ('/tmp/file.jsonl', 1000, 200);
            INSERT INTO messages (agent, model, date, input_tokens, source_path)
                VALUES ('claude', 'opus', '2026-01-01', 123, '/tmp/file.jsonl');",
        )
        .unwrap();

        init_schema(&conn).unwrap();

        let state = load_scan_state(&conn, "/tmp/file.jsonl").unwrap();
        assert_eq!(state.mtime_secs, 1000);
        assert_eq!(state.size, 200);
        assert_eq!(state.parsed_upto_bytes, 200);
        assert!(state.file_id.is_none());

        let records = load_aggregated(&conn).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].in_tokens, 123);
    }

    #[test]
    fn migration_without_schema_version_repairs_scanned_files_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let conn = open_db(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS scanned_files (
                path       TEXT PRIMARY KEY,
                mtime_secs INTEGER NOT NULL,
                size       INTEGER NOT NULL
            );",
        )
        .unwrap();

        init_schema(&conn).unwrap();
        mark_file_scanned(&conn, "/tmp/file.jsonl", 1000, 200, 180, Some("dev:inode")).unwrap();

        let state = load_scan_state(&conn, "/tmp/file.jsonl").unwrap();
        assert_eq!(state.parsed_upto_bytes, 180);
        assert_eq!(state.file_id.as_deref(), Some("dev:inode"));
    }

    #[test]
    fn aggregate_normalizes_model_names() {
        let (conn, _dir) = temp_db();
        let entries = vec![
            RawEntry {
                agent: "claude".to_string(),
                model: "claude-opus-4.7".to_string(),
                date: "2026-05-27".to_string(),
                input_tokens: 100,
                output_tokens: 7,
                cache_read_input_tokens: 20,
                cache_creation_input_tokens: 0,
                reasoning_output_tokens: 0,
                dedup_primary: None,
                dedup_secondary: None,
                is_sidechain: false,
                session_id: None,
                message_id: None,
            },
            RawEntry {
                agent: "claude".to_string(),
                model: "claude-opus-4-7".to_string(),
                date: "2026-05-27".to_string(),
                input_tokens: 200,
                output_tokens: 14,
                cache_read_input_tokens: 40,
                cache_creation_input_tokens: 5,
                reasoning_output_tokens: 0,
                dedup_primary: None,
                dedup_secondary: None,
                is_sidechain: false,
                session_id: None,
                message_id: None,
            },
        ];
        insert_file_messages(&conn, &entries, "/test/claude.jsonl").unwrap();

        let records = load_aggregated(&conn).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].model, "claude-opus-4-7");
        assert_eq!(records[0].in_tokens, 300);
    }

    #[test]
    fn aggregate_merges_provider_prefixed_model_names() {
        let (conn, _dir) = temp_db();
        let entries = vec![
            RawEntry {
                agent: "claude".to_string(),
                model: "MiniMax/MiniMax-M2.7".to_string(),
                date: "2026-05-27".to_string(),
                input_tokens: 100,
                output_tokens: 7,
                cache_read_input_tokens: 20,
                cache_creation_input_tokens: 0,
                reasoning_output_tokens: 0,
                dedup_primary: None,
                dedup_secondary: None,
                is_sidechain: false,
                session_id: None,
                message_id: None,
            },
            RawEntry {
                agent: "claude".to_string(),
                model: "MiniMax-M2.7".to_string(),
                date: "2026-05-27".to_string(),
                input_tokens: 200,
                output_tokens: 14,
                cache_read_input_tokens: 40,
                cache_creation_input_tokens: 5,
                reasoning_output_tokens: 0,
                dedup_primary: None,
                dedup_secondary: None,
                is_sidechain: false,
                session_id: None,
                message_id: None,
            },
        ];
        insert_file_messages(&conn, &entries, "/test/minimax.jsonl").unwrap();

        let records = load_aggregated(&conn).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].model, "MiniMax-M2.7");
        assert_eq!(records[0].in_tokens, 300);
    }

    #[test]
    fn aggregate_merges_context_variant_model_names() {
        let (conn, _dir) = temp_db();
        let entries = vec![
            RawEntry {
                agent: "claude".to_string(),
                model: "glm-5.2[1m]".to_string(),
                date: "2026-05-27".to_string(),
                input_tokens: 100,
                output_tokens: 7,
                cache_read_input_tokens: 20,
                cache_creation_input_tokens: 0,
                reasoning_output_tokens: 0,
                dedup_primary: None,
                dedup_secondary: None,
                is_sidechain: false,
                session_id: None,
                message_id: None,
            },
            RawEntry {
                agent: "claude".to_string(),
                model: "glm-5.2".to_string(),
                date: "2026-05-27".to_string(),
                input_tokens: 200,
                output_tokens: 14,
                cache_read_input_tokens: 40,
                cache_creation_input_tokens: 5,
                reasoning_output_tokens: 0,
                dedup_primary: None,
                dedup_secondary: None,
                is_sidechain: false,
                session_id: None,
                message_id: None,
            },
        ];
        insert_file_messages(&conn, &entries, "/test/glm.jsonl").unwrap();

        let records = load_aggregated(&conn).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].model, "glm-5.2");
        assert_eq!(records[0].in_tokens, 300);
    }
}
