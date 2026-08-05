//! Manox agent SQLite parser.
//!
//! Manox persists per-message token usage in `~/.config/cx/manox/threads.db`.
//! Two tables are relevant:
//!
//! - `threads` — holds thread metadata including `model_id`
//! - `token_usage` — holds per-message token breakdowns keyed by
//!   `(thread_id, user_message_id)` with a `completed_at` unix-seconds timestamp
//!
//! Strategy: raw-sum (no dedup by message id), consistent with mimo.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

use super::RawEntry;
use crate::stats::date::date_from_unix_secs;

const AGENT: &str = super::super::AGENT_MANOX;

pub(super) fn parse(db_path: &Path) -> Result<Vec<RawEntry>> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open manox database: {}", db_path.display()))?;

    let mut stmt = conn
        .prepare(
            "SELECT tu.thread_id, \
                    tu.input_tokens, \
                    tu.output_tokens, \
                    tu.cache_creation_input_tokens, \
                    tu.cache_read_input_tokens, \
                    tu.completed_at, \
                    t.model_id \
             FROM token_usage tu \
             JOIN threads t ON t.id = tu.thread_id",
        )
        .context("query manox token_usage")?;

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;

    let mut out = Vec::new();
    let mut row_errors = 0u32;
    for row in rows {
        let (thread_id, input, output, cache_create, cache_read, completed_at, model_id) = match row
        {
            Ok(t) => t,
            Err(_) => {
                row_errors += 1;
                continue;
            }
        };

        let input_tokens = input.max(0) as u64;
        let output_tokens = output.max(0) as u64;
        let cache_creation_input_tokens = cache_create.max(0) as u64;
        let cache_read_input_tokens = cache_read.max(0) as u64;

        if input_tokens == 0
            && output_tokens == 0
            && cache_creation_input_tokens == 0
            && cache_read_input_tokens == 0
        {
            continue;
        }

        let model = model_id.trim();
        if model.is_empty() {
            continue;
        }

        let date = date_from_unix_secs(completed_at);
        if date.is_empty() {
            continue;
        }

        out.push(RawEntry {
            agent: AGENT.to_string(),
            model: model.to_string(),
            date,
            input_tokens,
            output_tokens,
            cache_read_input_tokens,
            cache_creation_input_tokens,
            reasoning_output_tokens: 0,
            dedup_primary: None,
            dedup_secondary: None,
            is_sidechain: false,
            session_id: Some(thread_id),
            message_id: None,
            timestamp_secs: None,
        });
    }
    if row_errors > 0 {
        eprintln!(
            "cx: manox: {row_errors} rows skipped in {}",
            db_path.display()
        );
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_test_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("threads.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                model_id TEXT NOT NULL DEFAULT '',
                provider_id TEXT,
                created_at INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE token_usage (
                thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
                user_message_id TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                completed_at INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (thread_id, user_message_id)
            );",
        )
        .unwrap();

        // Thread with anthropic model
        conn.execute(
            "INSERT INTO threads (id, model_id, provider_id, created_at) \
             VALUES ('th_1', 'Anthropic/claude-opus-4.7[1m]/anthropic', 'Anthropic', 1779772000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO token_usage (thread_id, user_message_id, input_tokens, output_tokens, \
             cache_creation_input_tokens, cache_read_input_tokens, completed_at) \
             VALUES ('th_1', 'msg_1', 500, 200, 100, 5000, 1779772684)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO token_usage (thread_id, user_message_id, input_tokens, output_tokens, \
             cache_creation_input_tokens, cache_read_input_tokens, completed_at) \
             VALUES ('th_1', 'msg_2', 300, 150, 50, 3000, 1779773000)",
            [],
        )
        .unwrap();

        // Thread with non-anthropic model
        conn.execute(
            "INSERT INTO threads (id, model_id, provider_id, created_at) \
             VALUES ('th_2', '百炼/glm-5.2[1m]/anthropic', '百炼', 1779772000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO token_usage (thread_id, user_message_id, input_tokens, output_tokens, \
             cache_creation_input_tokens, cache_read_input_tokens, completed_at) \
             VALUES ('th_2', 'msg_3', 100, 50, 0, 1000, 1779774000)",
            [],
        )
        .unwrap();

        // Thread with completions wire API
        conn.execute(
            "INSERT INTO threads (id, model_id, provider_id, created_at) \
             VALUES ('th_5', 'OpenAI/gpt-5.4[1m]/completions', 'OpenAI', 1779772000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO token_usage (thread_id, user_message_id, input_tokens, output_tokens, \
             cache_creation_input_tokens, cache_read_input_tokens, completed_at) \
             VALUES ('th_5', 'msg_6', 200, 80, 0, 500, 1779777000)",
            [],
        )
        .unwrap();

        // Thread with zero usage (should be skipped)
        conn.execute(
            "INSERT INTO threads (id, model_id, provider_id, created_at) \
             VALUES ('th_3', 'test/model/openai', 'test', 1779772000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO token_usage (thread_id, user_message_id, input_tokens, output_tokens, \
             cache_creation_input_tokens, cache_read_input_tokens, completed_at) \
             VALUES ('th_3', 'msg_4', 0, 0, 0, 0, 1779775000)",
            [],
        )
        .unwrap();

        // Thread with empty model_id (should be skipped)
        conn.execute(
            "INSERT INTO threads (id, model_id, provider_id, created_at) \
             VALUES ('th_4', '', 'unknown', 1779772000)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO token_usage (thread_id, user_message_id, input_tokens, output_tokens, \
             cache_creation_input_tokens, cache_read_input_tokens, completed_at) \
             VALUES ('th_4', 'msg_5', 100, 50, 0, 0, 1779776000)",
            [],
        )
        .unwrap();

        (dir, db_path)
    }

    #[test]
    fn parses_all_non_zero_entries() {
        let (_dir, db_path) = setup_test_db();
        let entries = parse(&db_path).unwrap();
        // 4 non-zero entries (msg_4 zero, msg_5 empty model → both skipped)
        assert_eq!(entries.len(), 4);
    }

    #[test]
    fn sets_agent_and_session_id() {
        let (_dir, db_path) = setup_test_db();
        let entries = parse(&db_path).unwrap();
        for e in &entries {
            assert_eq!(e.agent, "manox");
            assert!(e.session_id.is_some());
        }
    }

    #[test]
    fn preserves_model_id_for_normalization() {
        let (_dir, db_path) = setup_test_db();
        let entries = parse(&db_path).unwrap();
        let models: Vec<&str> = entries.iter().map(|e| e.model.as_str()).collect();
        assert!(models.contains(&"Anthropic/claude-opus-4.7[1m]/anthropic"));
        assert!(models.contains(&"百炼/glm-5.2[1m]/anthropic"));
        assert!(models.contains(&"OpenAI/gpt-5.4[1m]/completions"));
    }

    #[test]
    fn skips_zero_usage_messages() {
        let (_dir, db_path) = setup_test_db();
        let entries = parse(&db_path).unwrap();
        for e in &entries {
            assert!(
                e.input_tokens > 0
                    || e.output_tokens > 0
                    || e.cache_read_input_tokens > 0
                    || e.cache_creation_input_tokens > 0
            );
        }
    }

    #[test]
    fn skips_empty_model_id() {
        let (_dir, db_path) = setup_test_db();
        let entries = parse(&db_path).unwrap();
        assert!(
            entries
                .iter()
                .all(|e| e.session_id.as_deref() != Some("th_4"))
        );
    }

    #[test]
    fn converts_unix_timestamp_to_date() {
        let (_dir, db_path) = setup_test_db();
        let entries = parse(&db_path).unwrap();
        // All test entries are on 2026-05-26
        for e in &entries {
            assert_eq!(e.date, "2026-05-26");
        }
    }
}
