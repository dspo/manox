//! `projects` table: independent record of project paths that should appear
//! in the sidebar, regardless of whether any active (non-archived) thread
//! references them. Projects are registered when a thread is first bound to
//! them and survive archival of all threads in that project.

use anyhow::{Context as _, Result};
use rusqlite::{Connection, params};

use super::ThreadsDatabase;

pub fn create_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects (
            path TEXT PRIMARY KEY,
            created_at INTEGER NOT NULL DEFAULT (unixepoch())
        );",
    )
    .context("create projects table")?;
    Ok(())
}

impl ThreadsDatabase {
    /// Register a project path so it persists in the sidebar even when all its
    /// threads are archived. `INSERT OR IGNORE` makes this idempotent — calling
    /// on every `save_thread` with a project is safe and cheap.
    pub fn register_project(&self, path: &str) -> Result<()> {
        if path.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "INSERT OR IGNORE INTO projects (path) VALUES (?1)",
            params![path],
        )
        .context("register project")?;
        Ok(())
    }

    /// List all registered project paths ordered by registration time
    /// (oldest first). The sidebar merges this with live thread summaries
    /// to produce the full project section.
    pub fn list_projects(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT path FROM projects ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("read project row")?);
        }
        Ok(out)
    }

    /// Remove a project path. Reserved for a future "remove project from
    /// sidebar" interaction — currently unused.
    #[allow(dead_code)]
    pub fn remove_project(&self, path: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "DELETE FROM projects WHERE path = ?1",
            params![path],
        )
        .context("remove project")?;
        Ok(())
    }
}
