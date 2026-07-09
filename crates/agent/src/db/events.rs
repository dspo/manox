//! `thread_events` table: an append-only event stream per thread. Event types
//! (model_change / compaction / branch_summary / custom) map to queryable rows.

use anyhow::{Context as _, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use super::ThreadsDatabase;

/// Event kind recorded in `thread_events`. The string value is what gets stored
/// in the `event_type` column, so it is a stable wire identifier — do not
/// rename it without a schema bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadEventType {
    ModelChange,
    Compaction,
    BranchSummary,
    Custom,
}

impl ThreadEventType {
    fn as_str(self) -> &'static str {
        match self {
            ThreadEventType::ModelChange => "model_change",
            ThreadEventType::Compaction => "compaction",
            ThreadEventType::BranchSummary => "branch_summary",
            ThreadEventType::Custom => "custom",
        }
    }
}

/// One row of `thread_events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThreadEventRecord {
    pub id: i64,
    pub thread_id: String,
    pub seq: i64,
    pub event_type: String,
    pub ts: i64,
    pub data: serde_json::Value,
}

pub fn create_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS thread_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
            seq INTEGER NOT NULL,
            event_type TEXT NOT NULL,
            ts INTEGER NOT NULL DEFAULT (unixepoch()),
            data TEXT NOT NULL DEFAULT '{}'
        );
        CREATE INDEX IF NOT EXISTS idx_thread_events_thread_seq ON thread_events(thread_id, seq ASC);
        CREATE INDEX IF NOT EXISTS idx_thread_events_type ON thread_events(event_type);",
    )
    .context("create thread_events table")?;
    Ok(())
}

impl ThreadsDatabase {
    /// Append an event. `seq` is per-thread monotonic (COALESCE(MAX(seq),0)+1).
    pub fn record_event(
        &self,
        thread_id: &str,
        event_type: ThreadEventType,
        data: &serde_json::Value,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let next_seq: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM thread_events WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get(0),
            )
            .context("query thread_events seq")?;
        let data_str = serde_json::to_string(data).context("serialize event data")?;
        conn.execute(
            "INSERT INTO thread_events (thread_id, seq, event_type, data)
             VALUES (?1, ?2, ?3, ?4)",
            params![thread_id, next_seq, event_type.as_str(), data_str],
        )
        .context("insert thread_event")?;
        Ok(())
    }

    /// Query events for a thread, optionally filtered by type, ordered by `seq`.
    pub fn query_events(
        &self,
        thread_id: &str,
        event_type: Option<ThreadEventType>,
    ) -> Result<Vec<ThreadEventRecord>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = if event_type.is_some() {
            conn.prepare(
                "SELECT id, thread_id, seq, event_type, ts, data
                 FROM thread_events WHERE thread_id = ?1 AND event_type = ?2
                 ORDER BY seq ASC",
            )?
        } else {
            conn.prepare(
                "SELECT id, thread_id, seq, event_type, ts, data
                 FROM thread_events WHERE thread_id = ?1 ORDER BY seq ASC",
            )?
        };
        let mapper = |row: &rusqlite::Row| -> rusqlite::Result<ThreadEventRecord> {
            let data_str: String = row.get(5)?;
            let data = serde_json::from_str(&data_str).unwrap_or(serde_json::Value::Null);
            Ok(ThreadEventRecord {
                id: row.get(0)?,
                thread_id: row.get(1)?,
                seq: row.get(2)?,
                event_type: row.get(3)?,
                ts: row.get(4)?,
                data,
            })
        };
        let rows = match event_type {
            Some(et) => stmt.query_map(params![thread_id, et.as_str()], mapper)?,
            None => stmt.query_map(params![thread_id], mapper)?,
        };
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}
