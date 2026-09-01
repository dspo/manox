//! `terminal_sessions` table: lightweight per-terminal metadata for tab
//! restore (cwd / env / title). Scrollback is intentionally not persisted —
//! only enough to reopen a closed terminal in the same directory with the
//! same environment.

use anyhow::{Context as _, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use super::ThreadsDatabase;

/// One persisted terminal session. `env` is the override list (empty when the
/// terminal inherited the parent process environment).
#[derive(Debug, Clone)]
pub struct TerminalSession {
    pub id: String,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Serialized form of the `env` override list.
#[derive(Serialize, Deserialize)]
struct EnvPayload(Vec<(String, String)>);

pub fn create_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS terminal_sessions (
            id TEXT PRIMARY KEY,
            cwd TEXT NOT NULL,
            env_json TEXT NOT NULL DEFAULT '[]',
            title TEXT,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );",
    )?;
    Ok(())
}

impl ThreadsDatabase {
    /// Insert or replace a terminal session row.
    pub fn upsert_terminal_session(&self, session: &TerminalSession) -> Result<()> {
        let env_json =
            serde_json::to_string(&EnvPayload(session.env.clone())).context("encode env")?;
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT OR REPLACE INTO terminal_sessions
                (id, cwd, env_json, title, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                session.id,
                session.cwd,
                env_json,
                session.title,
                session.created_at,
                session.updated_at,
            ],
        )
        .context("upsert terminal_session")?;
        Ok(())
    }

    /// Load a single session by id.
    pub fn load_terminal_session(&self, id: &str) -> Result<Option<TerminalSession>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn
            .prepare("SELECT id, cwd, env_json, title, created_at, updated_at FROM terminal_sessions WHERE id = ?")
            .context("prepare load_terminal_session")?;
        let mut rows = stmt
            .query(params![id])
            .context("query load_terminal_session")?;
        let row = rows.next().context("step load_terminal_session")?;
        Ok(row.map(decode_row).transpose()?)
    }

    /// All sessions, newest first by `updated_at`.
    pub fn list_terminal_sessions(&self) -> Result<Vec<TerminalSession>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn
            .prepare(
                "SELECT id, cwd, env_json, title, created_at, updated_at
                 FROM terminal_sessions
                 ORDER BY updated_at DESC",
            )
            .context("prepare list_terminal_sessions")?;
        let sessions = stmt
            .query_map([], decode_row)
            .context("query list_terminal_sessions")?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("collect terminal_sessions")?;
        Ok(sessions)
    }

    /// Delete a session row (e.g. when the user closes a terminal for good).
    pub fn delete_terminal_session(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute("DELETE FROM terminal_sessions WHERE id = ?", params![id])
            .context("delete terminal_session")?;
        Ok(())
    }
}

fn decode_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TerminalSession> {
    let env_json: String = row.get("env_json")?;
    let env = serde_json::from_str::<EnvPayload>(&env_json)
        .map(|p| p.0)
        .unwrap_or_default();
    Ok(TerminalSession {
        id: row.get("id")?,
        cwd: row.get("cwd")?,
        env,
        title: row.get("title")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
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

    fn sample(id: &str, cwd: &str) -> TerminalSession {
        TerminalSession {
            id: id.into(),
            cwd: cwd.into(),
            env: vec![("FOO".into(), "bar".into())],
            title: Some("work".into()),
            created_at: 1000,
            updated_at: 2000,
        }
    }

    #[test]
    fn upsert_then_load_roundtrip() {
        let db = open_mem();
        db.upsert_terminal_session(&sample("t1", "/tmp")).unwrap();
        let loaded = db.load_terminal_session("t1").unwrap().unwrap();
        assert_eq!(loaded.cwd, "/tmp");
        assert_eq!(loaded.env, vec![("FOO".to_string(), "bar".into())]);
        assert_eq!(loaded.title.as_deref(), Some("work"));
    }

    #[test]
    fn list_orders_newest_first() {
        let db = open_mem();
        let mut a = sample("a", "/a");
        a.updated_at = 100;
        let mut b = sample("b", "/b");
        b.updated_at = 300;
        db.upsert_terminal_session(&a).unwrap();
        db.upsert_terminal_session(&b).unwrap();
        let list = db.list_terminal_sessions().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "b");
        assert_eq!(list[1].id, "a");
    }

    #[test]
    fn delete_removes_session() {
        let db = open_mem();
        db.upsert_terminal_session(&sample("t1", "/tmp")).unwrap();
        db.delete_terminal_session("t1").unwrap();
        assert!(db.load_terminal_session("t1").unwrap().is_none());
    }

    #[test]
    fn upsert_replaces_existing() {
        let db = open_mem();
        db.upsert_terminal_session(&sample("t1", "/old")).unwrap();
        let mut updated = sample("t1", "/new");
        updated.updated_at = 9999;
        db.upsert_terminal_session(&updated).unwrap();
        let loaded = db.load_terminal_session("t1").unwrap().unwrap();
        assert_eq!(loaded.cwd, "/new");
        assert_eq!(loaded.updated_at, 9999);
    }
}
