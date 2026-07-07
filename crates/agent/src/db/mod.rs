//! SQLite persistence.
//!
//! Five tables back the metadata model:
//! - `threads`: lightweight per-thread metadata + cumulative token columns.
//!   The sidebar list query reads only this table — never the message BLOB —
//!   so a long history stays cheap to enumerate.
//! - `thread_data`: a single zstd-compressed JSON BLOB per thread holding the
//!   full `messages` array and the `request_token_usage` map (the heavy state).
//! - `thread_events`: an append-only event stream (model_change / compaction /
//!   branch_summary / custom) mirroring pi's JSONL entry types as rows.
//! - `token_usage`: per-user-message token breakdown, queryable without
//!   decompressing the message BLOB.
//! - `terminal_sessions`: per-terminal metadata (cwd/env/title) for tab
//!   restore; scrollback is not persisted.
//!
//! `ThreadsDatabase` holds a `Mutex<Connection>`; all methods are synchronous
//! and blocking (callers wrap them in `background_spawn`).

mod events;
mod terminals;
mod threads;
mod token_usage;

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context as _, Result};
use rusqlite::Connection;

pub use events::{ThreadEventRecord, ThreadEventType};
pub use terminals::TerminalSession;
pub use threads::{ThreadRecord, ThreadSummary};
pub use token_usage::TokenUsageRecord;

use crate::paths;

/// Schema version. `open()` compares `PRAGMA user_version` against this; a
/// mismatch drops every table and recreates them. There is no incremental
/// migration — per the upgrade design, legacy data is not carried forward.
const SCHEMA_VERSION: i32 = 2;

/// Thread database handle.
pub struct ThreadsDatabase {
    conn: Mutex<Connection>,
}

impl ThreadsDatabase {
    /// Open (creating if needed) the database file and ensure the schema.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create db directory: {}", parent.display()))?;
        }
        let mut conn = Connection::open(path)
            .with_context(|| format!("open threads db: {}", path.display()))?;
        Self::init_schema(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Recreate the schema when `user_version` does not match `SCHEMA_VERSION`.
    /// Legacy data is dropped wholesale — no migration path, by design. The
    /// drop + recreate + version bump runs in one transaction so a crash mid-way
    /// cannot leave a half-dropped database that the next open would mis-read.
    fn init_schema(conn: &mut Connection) -> Result<()> {
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .context("read user_version")?;
        if version == SCHEMA_VERSION {
            // Already at the current schema; ensure tables exist (idempotent for
            // a freshly-created database where user_version defaults to 0 but the
            // tables were just made — covered by the recreate path below on first
            // open, so this branch only fires for a second open of an existing db).
            threads::create_table(conn)?;
            events::create_table(conn)?;
            token_usage::create_table(conn)?;
            terminals::create_table(conn)?;
            return Ok(());
        }
        let tx = conn
            .transaction()
            .context("begin schema rebuild transaction")?;
        tx.execute_batch(
            "DROP TABLE IF EXISTS terminal_sessions;
             DROP TABLE IF EXISTS token_usage;
             DROP TABLE IF EXISTS thread_events;
             DROP TABLE IF EXISTS thread_data;
             DROP TABLE IF EXISTS threads;",
        )
        .context("drop legacy tables")?;
        threads::create_table(&tx)?;
        events::create_table(&tx)?;
        token_usage::create_table(&tx)?;
        terminals::create_table(&tx)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)
            .context("set user_version")?;
        tx.commit().context("commit schema rebuild transaction")?;
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
    use crate::language_model::{MessageContent, Role, TokenUsage};
    use crate::message::Message;
    use std::collections::HashMap;

    fn open_mem() -> ThreadsDatabase {
        let mut conn = Connection::open_in_memory().unwrap();
        ThreadsDatabase::init_schema(&mut conn).unwrap();
        ThreadsDatabase {
            conn: Mutex::new(conn),
        }
    }

    fn sample_record(id: &str) -> ThreadRecord {
        let mut usage = HashMap::new();
        usage.insert(
            "u1".to_string(),
            TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
        );
        ThreadRecord {
            id: id.into(),
            summary: "你好".into(),
            title: Some("关于登录".into()),
            title_override: None,
            model_id: "百炼/glm-5.2[1m]/anthropic".into(),
            provider_id: Some("百炼".into()),
            cwd: "/tmp".into(),
            project: "/tmp".into(),
            yolo: true,
            depth: 0,
            parent_id: None,
            archived: false,
            created_at: 1_700_000_000,
            interacted_at: 1_700_000_100,
            updated_at: 1_700_000_200,
            session_started_at: 1_700_000_000,
            cumulative_token_usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: 10,
                cache_read_input_tokens: 20,
            },
            messages: vec![
                Message::user("你好".into()),
                Message::assistant(vec![MessageContent::Text("hi".into())]),
            ],
            request_token_usage: usage,
        }
    }

    #[test]
    fn upsert_load_round_trip() {
        let db = open_mem();
        let rec = sample_record("t1");
        db.upsert(&rec, true).unwrap();

        let loaded = db.load("t1").unwrap().unwrap();
        assert_eq!(loaded.id, "t1");
        assert_eq!(loaded.summary, "你好");
        assert_eq!(loaded.title.as_deref(), Some("关于登录"));
        assert_eq!(loaded.provider_id.as_deref(), Some("百炼"));
        assert!(loaded.yolo);
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].role, Role::User);
        assert!(!loaded.messages[0].id.is_empty());
        assert_eq!(loaded.cumulative_token_usage.input_tokens, 100);
        assert_eq!(loaded.cumulative_token_usage.cache_read_input_tokens, 20);
        let u = loaded.request_token_usage.get("u1").unwrap();
        assert_eq!(u.output_tokens, 50);
    }

    #[test]
    fn list_excludes_archived_unless_requested() {
        let db = open_mem();
        db.upsert(&sample_record("t1"), true).unwrap();
        let mut archived = sample_record("t2");
        archived.archived = true;
        db.upsert(&archived, true).unwrap();

        let active = db.list(false).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, "t1");
        assert_eq!(active[0].cumulative_total_tokens, 100 + 50 + 10 + 20);

        let all = db.list(true).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn rename_and_archive() {
        let db = open_mem();
        db.upsert(&sample_record("t1"), true).unwrap();
        db.rename("t1", Some("我的重命名")).unwrap();
        db.archive("t1", true).unwrap();
        let loaded = db.load("t1").unwrap().unwrap();
        assert_eq!(loaded.title_override.as_deref(), Some("我的重命名"));
        assert!(loaded.archived);
    }

    #[test]
    fn delete_cascades() {
        let db = open_mem();
        db.upsert(&sample_record("t1"), true).unwrap();
        db.record_event(
            "t1",
            ThreadEventType::ModelChange,
            &serde_json::json!({"to":"m2"}),
        )
        .unwrap();
        db.upsert_token_usage("t1", "u1", &TokenUsage::default())
            .unwrap();
        db.delete("t1").unwrap();
        assert!(db.load("t1").unwrap().is_none());
        assert!(db.query_events("t1", None).unwrap().is_empty());
        assert!(db.query_token_usage("t1").unwrap().is_empty());
    }

    #[test]
    fn schema_rebuild_on_version_mismatch() {
        let mut conn = Connection::open_in_memory().unwrap();
        ThreadsDatabase::init_schema(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO threads (id, summary, model_id, cwd, yolo, created_at, interacted_at, updated_at, session_started_at) VALUES ('x','','','/tmp',0,0,0,0,0)",
            [],
        )
        .unwrap();
        // Simulate an older schema version: a second init at a lower version
        // must wipe the row we just inserted.
        conn.pragma_update(None, "user_version", 1).unwrap();
        ThreadsDatabase::init_schema(&mut conn).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        let v: i32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn events_seq_monotonic() {
        let db = open_mem();
        db.upsert(&sample_record("t1"), true).unwrap();
        for _ in 0..3 {
            db.record_event("t1", ThreadEventType::Custom, &serde_json::json!({}))
                .unwrap();
        }
        let evs = db.query_events("t1", None).unwrap();
        assert_eq!(evs.len(), 3);
        assert_eq!(evs[0].seq, 1);
        assert_eq!(evs[2].seq, 3);
    }
}
