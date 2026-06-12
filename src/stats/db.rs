//! SQLite 持久化用量缓存（~/.config/cx/cx.db）。
//!
//! 替代旧的 JSON 文件缓存（~/.local/share/cx/stats-cache.json），
//! 减少本地文件系统 IO；即便 agent 卸载后数据目录被清除，
//! cx 仍保留其 tokens 数据。

use anyhow::{Context, Result};
use dirs::home_dir;
use rusqlite::{Connection, params};
use std::path::{Path, PathBuf};

use super::parser::RawEntry;
use super::types::UsageRecord;

pub(super) const DB_VERSION: u32 = 1;

pub(super) fn db_path() -> Result<PathBuf> {
    let home = home_dir().context("无法解析用户主目录")?;
    Ok(home.join(".config/cx/cx.db"))
}

pub(super) fn open_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建数据库目录失败: {}", parent.display()))?;
    }
    let conn = Connection::open(path)
        .with_context(|| format!("打开数据库失败: {}", path.display()))?;
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
        );",
    )?;

    let current_version: u32 = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if current_version < DB_VERSION {
        // v1 首次部署：全量清空后由上层重新聚合填充。
        // 后续版本升级应改为增量迁移（ALTER TABLE / INSERT INTO SELECT），
        // 避免丢失已缓存的用量数据。
        conn.execute("DELETE FROM usage_records", [])?;
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', ?1)",
            params![DB_VERSION.to_string()],
        )?;
    }

    Ok(())
}

/// 从 DB 加载所有聚合后的 UsageRecord。
///
/// 当前 `run_stats` 每次都重新聚合写入，此函数仅用于测试验证写入正确性。
#[cfg(test)]
pub(super) fn load_aggregated(conn: &Connection) -> Result<Vec<UsageRecord>> {
    let mut stmt = conn.prepare(
        "SELECT agent, model, date, in_tokens, out_tokens,
                cache_read_input_tokens, cache_creation_input_tokens
         FROM usage_records",
    )?;

    let rows = stmt.query_map([], |row| {
        let in_tokens: u64 = row.get(3)?;
        let out_tokens: u64 = row.get(4)?;
        Ok(UsageRecord {
            agent: row.get(0)?,
            model: row.get(1)?,
            date: row.get(2)?,
            in_tokens,
            total_tokens: in_tokens + out_tokens,
            out_tokens,
            cache_read_input_tokens: row.get(5)?,
            cache_creation_input_tokens: row.get(6)?,
        })
    })?;

    let mut records = Vec::new();
    for row in rows {
        records.push(row?);
    }
    Ok(records)
}

/// 检查文件是否未变化（mtime + size 匹配）。
pub(super) fn file_unchanged(conn: &Connection, path: &str, mtime: u64, size: u64) -> bool {
    conn.query_row(
        "SELECT mtime_secs, size FROM scanned_files WHERE path = ?1",
        params![path],
        |row| {
            let cached_mtime: u64 = row.get(0)?;
            let cached_size: u64 = row.get(1)?;
            Ok(cached_mtime == mtime && cached_size == size)
        },
    )
    .unwrap_or(false)
}

/// 获取某文件的缓存 raw entries。
///
/// 若缓存 JSON 反序列化失败（损坏/截断），返回错误而非静默空 Vec，
/// 让上层走 cache-miss 重新解析源文件。
pub(super) fn get_cached_raw(conn: &Connection, path: &str) -> Result<Vec<RawEntry>> {
    let json: String = conn.query_row(
        "SELECT raw_json FROM scanned_files WHERE path = ?1",
        params![path],
        |row| row.get(0),
    )?;
    serde_json::from_str(&json)
        .with_context(|| format!("缓存 JSON 反序列化失败: {path}, 将重新解析"))
}

/// 存储文件的 raw entries（upsert）。
pub(super) fn store_file_entries(
    conn: &Connection,
    path: &str,
    mtime: u64,
    size: u64,
    entries: &[RawEntry],
) -> Result<()> {
    let json = serde_json::to_string(entries)?;
    conn.execute(
        "INSERT OR REPLACE INTO scanned_files (path, mtime_secs, size, raw_json)
         VALUES (?1, ?2, ?3, ?4)",
        params![path, mtime, size, json],
    )?;
    Ok(())
}

/// 全量替换聚合后的 usage_records。
///
/// 在事务中执行 DELETE + INSERT，避免中途崩溃导致表为空。
pub(super) fn replace_usage_records(conn: &Connection, records: &[UsageRecord]) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch("DELETE FROM usage_records")?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO usage_records
             (agent, model, date, in_tokens, out_tokens,
              cache_read_input_tokens, cache_creation_input_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for r in records {
            stmt.execute(params![
                r.agent,
                r.model,
                r.date,
                r.in_tokens,
                r.out_tokens,
                r.cache_read_input_tokens,
                r.cache_creation_input_tokens,
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// 清理不再存在的源目录下的 stale 文件条目。
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
            || active_extra_files
                .iter()
                .any(|extra| path == *extra);
        if !is_active {
            conn.execute("DELETE FROM scanned_files WHERE path = ?1", params![path_str])?;
        }
    }
    Ok(())
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
        }
    }

    #[test]
    fn schema_initialization_is_idempotent() {
        let (conn, _dir) = temp_db();
        init_schema(&conn).unwrap();
        init_schema(&conn).unwrap();
    }

    #[test]
    fn store_and_load_raw_entries() {
        let (conn, _dir) = temp_db();
        let entries = vec![sample_entry()];
        store_file_entries(&conn, "/test/file.jsonl", 1000, 200, &entries).unwrap();

        assert!(file_unchanged(&conn, "/test/file.jsonl", 1000, 200));
        assert!(!file_unchanged(&conn, "/test/file.jsonl", 1001, 200));

        let loaded = get_cached_raw(&conn, "/test/file.jsonl").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].input_tokens, 100);
    }

    #[test]
    fn get_cached_raw_corrupt_json_returns_error() {
        let (conn, _dir) = temp_db();
        // 直接写入损坏 JSON。
        conn.execute(
            "INSERT OR REPLACE INTO scanned_files (path, mtime_secs, size, raw_json)
             VALUES (?1, ?2, ?3, ?4)",
            params!["/corrupt/file.jsonl", 1000, 200, "NOT VALID JSON!!!"],
        )
        .unwrap();
        // mtime+size 匹配 → file_unchanged 返回 true，但 get_cached_raw 应报错。
        assert!(file_unchanged(&conn, "/corrupt/file.jsonl", 1000, 200));
        let result = get_cached_raw(&conn, "/corrupt/file.jsonl");
        assert!(result.is_err(), "损坏 JSON 应返回错误而非空 Vec");
    }

    #[test]
    fn store_file_entries_upsert_replaces_previous() {
        let (conn, _dir) = temp_db();
        let first = vec![RawEntry {
            agent: "a".to_string(),
            model: "old".to_string(),
            date: "2026-01-01".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning_output_tokens: 0,
            dedup_primary: None,
            dedup_secondary: None,
            is_sidechain: false,
        }];
        let second = vec![sample_entry()];
        store_file_entries(&conn, "/test/file.jsonl", 1000, 200, &first).unwrap();
        store_file_entries(&conn, "/test/file.jsonl", 1000, 200, &second).unwrap();

        let loaded = get_cached_raw(&conn, "/test/file.jsonl").unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].model, "m"); // second write wins
    }

    #[test]
    fn replace_and_load_usage_records() {
        let (conn, _dir) = temp_db();
        let records = vec![UsageRecord {
            agent: "claude".to_string(),
            model: "opus-4".to_string(),
            date: "2026-01-01".to_string(),
            in_tokens: 1000,
            total_tokens: 1500,
            out_tokens: 500,
            cache_read_input_tokens: 100,
            cache_creation_input_tokens: 50,
        }];
        replace_usage_records(&conn, &records).unwrap();

        let loaded = load_aggregated(&conn).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].in_tokens, 1000);
    }

    #[test]
    fn schema_migration_clears_usage_records() {
        let (conn, _dir) = temp_db();
        // 先插入一些数据。
        let records = vec![UsageRecord {
            agent: "claude".to_string(),
            model: "opus-4".to_string(),
            date: "2026-01-01".to_string(),
            in_tokens: 999,
            total_tokens: 999,
            out_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        }];
        replace_usage_records(&conn, &records).unwrap();
        assert_eq!(load_aggregated(&conn).unwrap().len(), 1);

        // 手动将 schema_version 设为 0，触发 init_schema 的迁移逻辑。
        conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '0')",
            [],
        )
        .unwrap();
        init_schema(&conn).unwrap();
        // 迁移应清空 usage_records。
        assert_eq!(load_aggregated(&conn).unwrap().len(), 0);
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
        }];
        store_file_entries(&conn, "/active/file.jsonl", 1000, 200, &active_entries).unwrap();
        store_file_entries(&conn, "/removed/file.jsonl", 1000, 200, &stale_entries).unwrap();

        cleanup_stale_entries(&conn, &[Path::new("/active")], &[]).unwrap();

        // active 保留，removed 被删。
        assert!(file_unchanged(&conn, "/active/file.jsonl", 1000, 200));
        assert!(!file_unchanged(&conn, "/removed/file.jsonl", 1000, 200));
    }

    #[test]
    fn cleanup_stale_entries_preserves_extra_files() {
        let (conn, _dir) = temp_db();
        let entries = vec![sample_entry()];
        store_file_entries(&conn, "/extra/copilot.log", 1000, 200, &entries).unwrap();

        cleanup_stale_entries(&conn, &[Path::new("/active")], &[Path::new("/extra/copilot.log")])
            .unwrap();

        assert!(file_unchanged(&conn, "/extra/copilot.log", 1000, 200));
    }
}
