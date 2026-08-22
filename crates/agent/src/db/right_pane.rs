//! `thread_right_pane` table: the right pane's per-thread tab state.
//!
//! One row per thread holding an opaque JSON snapshot (tabs, active index,
//! visibility) owned by the UI layer — the db stores TEXT only and never
//! interprets the shape. Restored when the thread reopens; rewritten on
//! every pane mutation while the thread is foreground.

use anyhow::{Context as _, Result};
use rusqlite::{Connection, params};

use super::ThreadsDatabase;

pub fn create_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS thread_right_pane (
            thread_id  TEXT PRIMARY KEY,
            state_json TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );",
    )?;
    Ok(())
}

impl ThreadsDatabase {
    /// Insert or replace a thread's right-pane snapshot.
    pub fn upsert_right_pane(&self, thread_id: &str, state_json: &str) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT OR REPLACE INTO thread_right_pane (thread_id, state_json, updated_at)
             VALUES (?, ?, ?)",
            params![thread_id, state_json, now],
        )
        .context("upsert thread_right_pane")?;
        Ok(())
    }

    /// Load a thread's right-pane snapshot JSON, if any.
    pub fn load_right_pane(&self, thread_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn
            .prepare("SELECT state_json FROM thread_right_pane WHERE thread_id = ?")
            .context("prepare load_right_pane")?;
        let mut rows = stmt
            .query(params![thread_id])
            .context("query load_right_pane")?;
        let row = rows.next().context("step load_right_pane")?;
        Ok(row.map(|r| r.get::<_, String>("state_json")).transpose()?)
    }

    /// Delete a thread's right-pane snapshot.
    pub fn delete_right_pane(&self, thread_id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "DELETE FROM thread_right_pane WHERE thread_id = ?",
            params![thread_id],
        )
        .context("delete thread_right_pane")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_mem() -> ThreadsDatabase {
        let conn = Connection::open_in_memory().unwrap();
        create_table(&conn).unwrap();
        ThreadsDatabase {
            conn: std::sync::Mutex::new(conn),
        }
    }

    #[test]
    fn upsert_then_load_roundtrip() {
        let db = open_mem();
        db.upsert_right_pane("t1", r#"{"visible":true,"active":0,"tabs":[]}"#)
            .unwrap();
        let loaded = db.load_right_pane("t1").unwrap().unwrap();
        assert_eq!(loaded, r#"{"visible":true,"active":0,"tabs":[]}"#);
    }

    #[test]
    fn load_missing_returns_none() {
        let db = open_mem();
        assert!(db.load_right_pane("nope").unwrap().is_none());
    }

    #[test]
    fn upsert_replaces_existing() {
        let db = open_mem();
        db.upsert_right_pane("t1", "old").unwrap();
        db.upsert_right_pane("t1", "new").unwrap();
        assert_eq!(db.load_right_pane("t1").unwrap().as_deref(), Some("new"));
    }

    #[test]
    fn delete_removes_row() {
        let db = open_mem();
        db.upsert_right_pane("t1", "state").unwrap();
        db.delete_right_pane("t1").unwrap();
        assert!(db.load_right_pane("t1").unwrap().is_none());
        // Deleting an absent row is a no-op.
        db.delete_right_pane("t1").unwrap();
    }
}
