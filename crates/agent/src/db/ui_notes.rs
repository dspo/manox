//! `thread_ui_notes` table: persisted UI annotations (`Error` / `Notice`
//! cards) that are not part of the model-facing canonical `Thread::messages`.
//!
//! These notes are an append-only UI concern: they record what the user saw
//! (a runtime error card, a slash-command acknowledgement) so a reloaded
//! thread reproduces them. They never enter `build_completion_request`, so
//! the request-prefix bytes — and thus provider prompt-cache hits — are
//! unaffected. Each note anchors to the user message whose turn it belongs
//! to (or `None` when emitted before the first user message) so the rebuild
//! can splice it back at the end of that turn.

use anyhow::{Context as _, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use super::ThreadsDatabase;

/// Persisted UI note kind. The string value is what gets stored in the
/// `kind` column, so it is a stable wire identifier — do not rename without
/// a schema bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UiNoteKind {
    /// A terminal runtime error from the agent (red danger styling).
    Error,
    /// A neutral system notice — slash-command acks, mode-change chips, etc.
    Notice,
    /// A plan the user dismissed without implementing — a free-form message
    /// superseded it. Renders as a collapsed read-only `PlanReview` record so
    /// the dismissed plan survives a thread switch / reload: the live card is
    /// UI-only and never enters `Thread::messages`, so without this note it
    /// would vanish the moment the conversation entity is rebuilt.
    PlanReview,
}

impl UiNoteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UiNoteKind::Error => "error",
            UiNoteKind::Notice => "notice",
            UiNoteKind::PlanReview => "plan_review",
        }
    }
}

/// Parse the stored `kind` column. An unknown value resolves to `Notice`
/// rather than failing the whole reload — a future wire string never breaks
/// historical threads; the worst case is a danger card renders neutral.
impl FromStr for UiNoteKind {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "error" => UiNoteKind::Error,
            "plan_review" => UiNoteKind::PlanReview,
            _ => UiNoteKind::Notice,
        })
    }
}

/// One row of `thread_ui_notes`. `data` carries the render payload —
/// currently `{ "text": String }` — and is left as raw JSON so future
/// note shapes extend without a schema change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiNoteRecord {
    pub id: i64,
    pub thread_id: String,
    pub seq: i64,
    /// User message id whose turn this note belongs to; `None` for notes
    /// emitted before any user message (placed at the top on rebuild).
    pub anchor_user_id: Option<String>,
    pub kind: UiNoteKind,
    pub data: serde_json::Value,
    pub ts: i64,
}

pub fn create_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS thread_ui_notes (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
            seq INTEGER NOT NULL,
            anchor_user_id TEXT,
            kind TEXT NOT NULL,
            data TEXT NOT NULL DEFAULT '{}',
            ts INTEGER NOT NULL DEFAULT (unixepoch())
        );
        CREATE INDEX IF NOT EXISTS idx_thread_ui_notes_thread_seq
            ON thread_ui_notes(thread_id, seq ASC);",
    )
    .context("create thread_ui_notes table")?;
    Ok(())
}

impl ThreadsDatabase {
    /// Append a UI note. `seq` is per-thread monotonic
    /// (`COALESCE(MAX(seq), 0) + 1`), preserving emit order on reload.
    pub fn record_ui_note(
        &self,
        thread_id: &str,
        kind: UiNoteKind,
        anchor_user_id: Option<&str>,
        data: &serde_json::Value,
    ) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let next_seq: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM thread_ui_notes WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get(0),
            )
            .context("query thread_ui_notes seq")?;
        let data_str = serde_json::to_string(data).context("serialize ui note data")?;
        conn.execute(
            "INSERT INTO thread_ui_notes (thread_id, seq, anchor_user_id, kind, data)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![thread_id, next_seq, anchor_user_id, kind.as_str(), data_str],
        )
        .context("insert thread_ui_note")?;
        Ok(())
    }

    /// Load all UI notes for a thread in emit order. Notes are append-only:
    /// there is no delete path, so compaction may leave notes anchored to
    /// user messages that no longer exist — the rebuild places such
    /// orphans at the tail.
    pub fn list_ui_notes(&self, thread_id: &str) -> Result<Vec<UiNoteRecord>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, thread_id, seq, anchor_user_id, kind, data, ts
             FROM thread_ui_notes WHERE thread_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![thread_id], |row| {
            let data_str: String = row.get(5)?;
            let data = serde_json::from_str(&data_str).unwrap_or(serde_json::Value::Null);
            let kind_str: String = row.get(4)?;
            let kind = UiNoteKind::from_str(&kind_str).unwrap_or(UiNoteKind::Notice);
            Ok(UiNoteRecord {
                id: row.get(0)?,
                thread_id: row.get(1)?,
                seq: row.get(2)?,
                anchor_user_id: row.get(3)?,
                kind,
                data,
                ts: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
}
