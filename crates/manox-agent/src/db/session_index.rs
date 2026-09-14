//! `session_index` — the persisted (size, mtime) → bounded-facts cache for
//! the sidebar's session scan.
//!
//! The cache is a disposable accelerator, never a second source of truth:
//! a row's FACTS (header identity, first user message, has-messages,
//! thread/host/subagent keys) are immutable in an append-only journal, and
//! the fingerprints decide whether they are still covered. A missing,
//! corrupt or read-only table degrades the scan to bounded re-reads — the
//! list's CONTENT never depends on this table, only its latency.

use std::path::PathBuf;

use rusqlite::Connection;

use crate::db::ThreadsDatabase;

/// One cached session file: the bounded list facts plus the fingerprints
/// that guard them. Times are epoch nanoseconds (i64 since 1970 holds
/// ~292 years).
#[derive(Debug, Clone)]
pub struct SessionIndexRow {
    pub path: PathBuf,
    /// File size when the facts were read. An append-only file only grows:
    /// growth never invalidates the facts, a shrink always does.
    pub size: u64,
    /// Last-seen mtime — the ACTIVITY layer (sorting), never a fact
    /// invalidator.
    pub mtime_ns: i64,
    pub session_id: String,
    pub cwd: String,
    pub created_at_ns: i64,
    pub parent_session: Option<String>,
    /// The header's free-form metadata, serialized (host / thread /
    /// subagent / team ride inside).
    pub metadata_json: Option<String>,
    pub first_user_text: String,
    pub has_messages: bool,
    /// The last bounded scan failed wearing this fingerprint (a headerless
    /// zombie): the row carries no facts, only the skip verdict.
    pub failed: bool,
    /// Sidecar fingerprint + serialized content, so a sidecar whose (size,
    /// mtime) still matches skips its read entirely.
    pub sidecar_size: u64,
    pub sidecar_mtime_ns: i64,
    pub sidecar_json: Option<String>,
}

pub fn create_table(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS session_index (
            path           TEXT PRIMARY KEY,
            size           INTEGER NOT NULL,
            mtime_ns       INTEGER NOT NULL,
            session_id     TEXT NOT NULL,
            cwd            TEXT NOT NULL,
            created_at_ns  INTEGER NOT NULL,
            parent_session TEXT,
            metadata_json  TEXT,
            first_user_text TEXT NOT NULL,
            has_messages   INTEGER NOT NULL,
            failed         INTEGER NOT NULL,
            sidecar_size   INTEGER NOT NULL,
            sidecar_mtime_ns INTEGER NOT NULL,
            sidecar_json   TEXT
        )",
        [],
    )?;
    Ok(())
}

impl ThreadsDatabase {
    /// Load the whole index (the cold-start seed for the hot map). An empty
    /// table is normal on first boot.
    pub fn load_session_index(&self) -> rusqlite::Result<Vec<SessionIndexRow>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT path, size, mtime_ns, session_id, cwd, created_at_ns,
                    parent_session, metadata_json, first_user_text, has_messages,
                    failed, sidecar_size, sidecar_mtime_ns, sidecar_json
             FROM session_index",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(SessionIndexRow {
                path: PathBuf::from(row.get::<_, String>(0)?),
                size: row.get::<_, i64>(1)?.max(0) as u64,
                mtime_ns: row.get(2)?,
                session_id: row.get(3)?,
                cwd: row.get(4)?,
                created_at_ns: row.get(5)?,
                parent_session: row.get(6)?,
                metadata_json: row.get(7)?,
                first_user_text: row.get(8)?,
                has_messages: row.get::<_, i64>(9)? != 0,
                failed: row.get::<_, i64>(10)? != 0,
                sidecar_size: row.get::<_, i64>(11)?.max(0) as u64,
                sidecar_mtime_ns: row.get(12)?,
                sidecar_json: row.get(13)?,
            })
        })?;
        rows.collect()
    }

    /// Replace the index wholesale in one transaction. The table is a
    /// rebuild-able cache of at most one row per session file, so a full
    /// swap (delete + insert) in a single tx is simpler than diffing and
    /// lands atomically.
    pub fn replace_session_index(&self, rows: &[SessionIndexRow]) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM session_index", [])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO session_index
                    (path, size, mtime_ns, session_id, cwd, created_at_ns,
                     parent_session, metadata_json, first_user_text, has_messages,
                     failed, sidecar_size, sidecar_mtime_ns, sidecar_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?;
            for row in rows {
                stmt.execute(rusqlite::params![
                    row.path.to_string_lossy(),
                    row.size as i64,
                    row.mtime_ns,
                    row.session_id,
                    row.cwd,
                    row.created_at_ns,
                    row.parent_session,
                    row.metadata_json,
                    row.first_user_text,
                    row.has_messages as i64,
                    row.failed as i64,
                    row.sidecar_size as i64,
                    row.sidecar_mtime_ns,
                    row.sidecar_json,
                ])?;
            }
        }
        tx.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> ThreadsDatabase {
        let conn = Connection::open_in_memory().unwrap();
        ThreadsDatabase::init_schema(&conn).unwrap();
        ThreadsDatabase {
            conn: std::sync::Mutex::new(conn),
        }
    }

    fn row(path: &str, size: u64, mtime: i64) -> SessionIndexRow {
        SessionIndexRow {
            path: PathBuf::from(path),
            size,
            mtime_ns: mtime,
            session_id: "s1".into(),
            cwd: "/p".into(),
            created_at_ns: 1_700_000_000_000_000_000,
            parent_session: None,
            metadata_json: Some(r#"{"host":"manox","thread":"t1"}"#.into()),
            first_user_text: "first".into(),
            has_messages: true,
            failed: false,
            sidecar_size: 42,
            sidecar_mtime_ns: mtime + 1,
            sidecar_json: Some(r#"{"pinned":true}"#.into()),
        }
    }

    #[test]
    fn index_round_trips_and_replaces_wholesale() {
        let db = db();
        assert!(db.load_session_index().unwrap().is_empty());
        db.replace_session_index(&[row("/a.jsonl", 10, 100), row("/b.jsonl", 20, 200)])
            .unwrap();
        let loaded = db.load_session_index().unwrap();
        assert_eq!(loaded.len(), 2);
        let a = loaded
            .iter()
            .find(|r| r.path == std::path::Path::new("/a.jsonl"))
            .unwrap();
        assert_eq!(a.size, 10);
        assert_eq!(a.mtime_ns, 100);
        assert!(a.has_messages);
        assert_eq!(a.session_id, "s1");
        assert_eq!(
            a.metadata_json.as_deref(),
            Some(r#"{"host":"manox","thread":"t1"}"#)
        );
        assert_eq!(a.sidecar_json.as_deref(), Some(r#"{"pinned":true}"#));

        // Wholesale replace drops vanished paths.
        db.replace_session_index(&[row("/b.jsonl", 25, 300)])
            .unwrap();
        let loaded = db.load_session_index().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].size, 25, "the refreshed row wins");
    }

    #[test]
    fn optional_columns_default_on_missing_facts() {
        let db = db();
        let mut bare = row("/a.jsonl", 1, 1);
        bare.parent_session = None;
        bare.metadata_json = None;
        bare.sidecar_json = None;
        db.replace_session_index(&[bare]).unwrap();
        let loaded = &db.load_session_index().unwrap()[0];
        assert!(loaded.parent_session.is_none());
        assert!(loaded.metadata_json.is_none());
        assert!(loaded.sidecar_json.is_none());
    }
}
