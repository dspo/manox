//! SQLite persistence.
//!
//! Single `threads` table: id / summary / model_id / cwd / project / messages(JSON) / updated_at.
//! `ThreadsDatabase` holds a `Mutex<Connection>`; all methods are synchronous and
//! blocking (callers may wrap them in `background_spawn`). `ThreadRecord` is the
//! serializable snapshot of a `Thread`.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context as _, Result};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::language_model::{MessageContent, Role};
use crate::message::Message;
use crate::paths;

/// Thread database handle.
pub struct ThreadsDatabase {
    conn: Mutex<Connection>,
}

/// Summary used by the sidebar list (no message bodies).
#[derive(Debug, Clone)]
pub struct ThreadSummary {
    pub id: String,
    pub summary: String,
    pub model_id: String,
    /// Absolute project directory the thread is bound to; empty when none was chosen.
    pub project: String,
    pub updated_at: i64,
}

/// Complete persistent record of a `Thread`.
#[derive(Debug, Clone)]
pub struct ThreadRecord {
    pub id: String,
    pub summary: String,
    pub model_id: String,
    pub cwd: String,
    /// Absolute project directory the thread is bound to; empty when none was chosen.
    pub project: String,
    /// Whether YOLO mode was active when the thread was saved.
    pub yolo: bool,
    pub messages: Vec<Message>,
}

/// Serializable message representation (`Message` does not derive `Serialize`; mapped explicitly here).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredMessage {
    role: Role,
    content: Vec<MessageContent>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS threads (
    id TEXT PRIMARY KEY,
    summary TEXT NOT NULL DEFAULT '',
    model_id TEXT NOT NULL DEFAULT '',
    cwd TEXT NOT NULL DEFAULT '',
    project TEXT NOT NULL DEFAULT '',
    yolo INTEGER NOT NULL DEFAULT 0,
    messages TEXT NOT NULL DEFAULT '[]',
    updated_at INTEGER NOT NULL
);
";

impl ThreadsDatabase {
    /// Open (creating if needed) the database file and ensure the schema.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建 db 目录失败: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("打开 threads db 失败: {}", path.display()))?;
        conn.execute_batch(SCHEMA).context("建 threads 表失败")?;
        // Migrate pre-`project` databases. SQLite has no `ADD COLUMN IF NOT EXISTS`;
        // a duplicate-column error on already-migrated databases is expected and ignored.
        let _ = conn.execute(
            "ALTER TABLE threads ADD COLUMN project TEXT NOT NULL DEFAULT ''",
            [],
        );
        // Migrate pre-`yolo` databases (same idempotent-ignore pattern).
        let _ = conn.execute(
            "ALTER TABLE threads ADD COLUMN yolo INTEGER NOT NULL DEFAULT 0",
            [],
        );
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Insert or update a `Thread` record.
    /// Upsert a thread record. When `touch` is true, `updated_at` is set to the
    /// current timestamp (reflecting real user activity like sending a message).
    /// When `touch` is false, the existing `updated_at` is preserved (used when
    /// saving state on thread switch without implying the user interacted with it).
    pub fn upsert(&self, rec: &ThreadRecord, touch: bool) -> Result<()> {
        let stored: Vec<StoredMessage> = rec
            .messages
            .iter()
            .map(|m| StoredMessage {
                role: m.role,
                content: m.content.clone(),
            })
            .collect();
        let messages_json = serde_json::to_string(&stored).context("序列化 messages 失败")?;
        let now = chrono::Utc::now().timestamp();

        let conn = self.conn.lock().expect("db mutex 中毒");
        if touch {
            conn.execute(
                "INSERT INTO threads (id, summary, model_id, cwd, project, yolo, messages, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                    summary = excluded.summary,
                    model_id = excluded.model_id,
                    cwd = excluded.cwd,
                    project = excluded.project,
                    yolo = excluded.yolo,
                    messages = excluded.messages,
                    updated_at = excluded.updated_at",
                params![
                    rec.id,
                    rec.summary,
                    rec.model_id,
                    rec.cwd,
                    rec.project,
                    rec.yolo,
                    messages_json,
                    now
                ],
            )
            .context("upsert thread 失败")?;
        } else {
            // Preserve the existing updated_at on conflict; only insert with
            // `now` when the record is brand new (no prior updated_at to keep).
            conn.execute(
                "INSERT INTO threads (id, summary, model_id, cwd, project, yolo, messages, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                    summary = excluded.summary,
                    model_id = excluded.model_id,
                    cwd = excluded.cwd,
                    project = excluded.project,
                    yolo = excluded.yolo,
                    messages = excluded.messages",
                params![
                    rec.id,
                    rec.summary,
                    rec.model_id,
                    rec.cwd,
                    rec.project,
                    rec.yolo,
                    messages_json,
                    now
                ],
            )
            .context("upsert thread 失败")?;
        }
        Ok(())
    }

    /// Load a full record by id. Returns `None` if absent.
    pub fn load(&self, id: &str) -> Result<Option<ThreadRecord>> {
        let conn = self.conn.lock().expect("db mutex 中毒");
        let mut stmt = conn.prepare(
            "SELECT id, summary, model_id, cwd, project, yolo, messages FROM threads WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        let id: String = row.get(0)?;
        let summary: String = row.get(1)?;
        let model_id: String = row.get(2)?;
        let cwd: String = row.get(3)?;
        let project: String = row.get(4)?;
        let yolo: bool = row.get::<_, i64>(5)? != 0;
        let messages_json: String = row.get(6)?;
        let stored: Vec<StoredMessage> = serde_json::from_str(&messages_json)
            .with_context(|| format!("反序列化 messages 失败 (thread {id})"))?;
        let messages = stored
            .into_iter()
            .map(|s| Message {
                role: s.role,
                content: s.content,
            })
            .collect();
        Ok(Some(ThreadRecord {
            id,
            summary,
            model_id,
            cwd,
            project,
            yolo,
            messages,
        }))
    }

    /// List all `Thread` summaries, newest first by `updated_at`.
    pub fn list(&self) -> Result<Vec<ThreadSummary>> {
        let conn = self.conn.lock().expect("db mutex 中毒");
        let mut stmt = conn.prepare(
            "SELECT id, summary, model_id, project, updated_at FROM threads ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ThreadSummary {
                id: row.get(0)?,
                summary: row.get(1)?,
                model_id: row.get(2)?,
                project: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Delete by id.
    pub fn delete(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex 中毒");
        conn.execute("DELETE FROM threads WHERE id = ?1", params![id])
            .context("delete thread 失败")?;
        Ok(())
    }
}

/// Default db path: `$HOME/.config/cx/manox/threads.db`.
pub fn default_db_path() -> Result<std::path::PathBuf> {
    Ok(paths::manox_config_dir()?.join("threads.db"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::Role;
    use crate::message::Message;

    #[test]
    fn upsert_load_list_delete() {
        let dir = std::env::temp_dir().join("manox-db-test");
        let path = dir.join("threads.db");
        let _ = std::fs::remove_file(&path);
        let db = ThreadsDatabase::open(&path).expect("open");

        let rec = ThreadRecord {
            id: "t1".into(),
            summary: "你好".into(),
            model_id: "百炼/glm-5.2[1m]/anthropic".into(),
            cwd: "/tmp".into(),
            project: "/tmp".into(),
            yolo: true,
            messages: vec![
                Message::user("你好".into()),
                Message::assistant(vec![MessageContent::Text("hi".into())]),
            ],
        };
        db.upsert(&rec, true).expect("upsert");

        let loaded = db.load("t1").expect("load").expect("present");
        assert_eq!(loaded.id, "t1");
        assert_eq!(loaded.summary, "你好");
        assert_eq!(loaded.project, "/tmp");
        assert!(loaded.yolo, "yolo flag must round-trip");
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].role, Role::User);

        let list = db.list().expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "t1");
        assert_eq!(list[0].project, "/tmp");

        db.delete("t1").expect("delete");
        assert!(db.load("t1").expect("load").is_none());
    }
}
