//! `token_usage` table: per-user-message token breakdown.
//!
//! Each row keys on `(thread_id, user_message_id)` so the UI can show the
//! cost of a specific assistant reply without decompressing the message BLOB.
//! The thread-level cumulative lives as columns on `threads` (see `threads.rs`);
//! this table holds the per-request detail.

use anyhow::{Context as _, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::language_model::TokenUsage;

use super::ThreadsDatabase;

/// One row of `token_usage`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsageRecord {
    pub thread_id: String,
    pub user_message_id: String,
    pub usage: TokenUsage,
    pub completed_at: i64,
}

pub fn create_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS token_usage (
            thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
            user_message_id TEXT NOT NULL,
            input_tokens INTEGER NOT NULL DEFAULT 0,
            output_tokens INTEGER NOT NULL DEFAULT 0,
            cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
            cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
            completed_at INTEGER NOT NULL DEFAULT (unixepoch()),
            PRIMARY KEY (thread_id, user_message_id)
        );
        CREATE INDEX IF NOT EXISTS idx_token_usage_thread ON token_usage(thread_id, completed_at ASC);",
    )
    .context("create token_usage table")?;
    Ok(())
}

impl ThreadsDatabase {
    /// Upsert the per-user-message token usage. A later stop for the same user
    /// message id overwrites (e.g. a re-run after an edit replaces the prior count).
    pub fn upsert_token_usage(
        &self,
        thread_id: &str,
        user_message_id: &str,
        usage: &TokenUsage,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT INTO token_usage
                (thread_id, user_message_id, input_tokens, output_tokens,
                 cache_creation_input_tokens, cache_read_input_tokens, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch())
             ON CONFLICT(thread_id, user_message_id) DO UPDATE SET
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                cache_creation_input_tokens = excluded.cache_creation_input_tokens,
                cache_read_input_tokens = excluded.cache_read_input_tokens,
                completed_at = excluded.completed_at",
            params![
                thread_id,
                user_message_id,
                usage.input_tokens as i64,
                usage.output_tokens as i64,
                usage.cache_creation_input_tokens as i64,
                usage.cache_read_input_tokens as i64,
            ],
        )
        .context("upsert token_usage")?;
        Ok(())
    }

    /// All per-message usage for a thread, ordered by completion time.
    pub fn query_token_usage(&self, thread_id: &str) -> Result<Vec<TokenUsageRecord>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT thread_id, user_message_id, input_tokens, output_tokens,
                    cache_creation_input_tokens, cache_read_input_tokens, completed_at
             FROM token_usage WHERE thread_id = ?1 ORDER BY completed_at ASC",
        )?;
        let rows = stmt.query_map(params![thread_id], |row| {
            Ok(TokenUsageRecord {
                thread_id: row.get(0)?,
                user_message_id: row.get(1)?,
                usage: TokenUsage {
                    input_tokens: row.get::<_, i64>(2)? as u64,
                    output_tokens: row.get::<_, i64>(3)? as u64,
                    cache_creation_input_tokens: row.get::<_, i64>(4)? as u64,
                    cache_read_input_tokens: row.get::<_, i64>(5)? as u64,
                },
                completed_at: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}
