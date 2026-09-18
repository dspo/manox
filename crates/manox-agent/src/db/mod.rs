//! SQLite persistence.
//!
//! Seven tables back the metadata model:
//! - `threads`: lightweight per-thread metadata + cumulative token columns.
//!   The sidebar list query reads only this table — never the message BLOB —
//!   so a long history stays cheap to enumerate.
//! - `thread_data`: a single zstd-compressed JSON BLOB per thread holding the
//!   full `messages` array and the `request_token_usage` map (the heavy state).
//! - `thread_events`: an append-only lifecycle and Goal event stream. The
//!   current Goal is the strict fold of its `goal_*` events — no table stores
//!   the snapshot.
//! - `token_usage`: per-user-message token breakdown, queryable without
//!   decompressing the message BLOB.
//! - `terminal_sessions`: per-terminal metadata (cwd/env/title) for tab
//!   restore; scrollback is not persisted.
//! - `thread_right_pane`: one opaque JSON snapshot per thread backing the
//!   right pane's tab list / active tab / visibility (UI-layer owned shape).
//! - `projects`: registered project roots retained independently of threads.
//! - `session_index`: the sidebar scan's (size, mtime) → bounded-facts
//!   cache — a disposable accelerator, never a source of truth.
//!
//! UI annotation cards (Error / Notice / PlanReview) are NOT stored here —
//! they persist as `custom` entries in the session jsonl tree (see
//! `ui_notes::UI_NOTE_CUSTOM_TYPE`) so reload replays them in append order.
//!
//! `ThreadsDatabase` holds a `Mutex<Connection>`; all methods are synchronous
//! and blocking (callers wrap them in `background_spawn`).

mod events;
mod projects;
mod right_pane;
mod session_index;
mod terminals;
mod threads;
mod token_usage;
mod ui_notes;

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context as _, Result};
use rusqlite::Connection;

pub use crate::goal::{GoalActor, ThreadGoal};
pub use events::{ThreadEventRecord, ThreadEventType};
pub use session_index::SessionIndexRow;
pub use terminals::TerminalSession;
pub use threads::{ThreadRecord, ThreadSummary};
pub use token_usage::TokenUsageRecord;
pub use ui_notes::{HistoryEntry, PositionedNote, UI_NOTE_CUSTOM_TYPE, UiNoteKind, UiNoteRecord};

use crate::paths;

/// Thread database handle.
pub struct ThreadsDatabase {
    conn: Mutex<Connection>,
}

impl ThreadsDatabase {
    /// Open (creating if needed) the database file and ensure the schema.
    /// The store is shared by every manox process on the machine (the
    /// runtime is multi-instance), so the connection runs in WAL with a
    /// busy timeout: concurrent writers queue briefly instead of failing
    /// with `SQLITE_BUSY`, and readers never block writers.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create db directory: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("open threads db: {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("enable WAL journaling")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("set SQLite busy timeout")?;
        Self::init_schema(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Initialize the schema for a fresh database. Each `create_table` uses
    /// `CREATE TABLE IF NOT EXISTS`, so this is a no-op on an existing database
    /// whose tables already exist.
    ///
    /// Runtime never performs schema migration: no version comparison, no
    /// `ALTER TABLE`, no `DROP TABLE`. If the on-disk schema is stale, queries
    /// referencing missing columns will fail at first use — by design. Schema
    /// changes during development are applied manually to the developer's own
    /// database (sqlite3 CLI / `ALTER TABLE` / manual rebuild).
    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")
            .context("enable SQLite foreign keys")?;
        threads::create_table(conn)?;
        events::create_table(conn)?;
        token_usage::create_table(conn)?;
        terminals::create_table(conn)?;
        projects::create_table(conn)?;
        right_pane::create_table(conn)?;
        session_index::create_table(conn)?;
        Ok(())
    }
}

/// Default db path: `$HOME/.manox/threads.db`.
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
        let conn = Connection::open_in_memory().unwrap();
        ThreadsDatabase::init_schema(&conn).unwrap();
        ThreadsDatabase {
            conn: Mutex::new(conn),
        }
    }

    /// A database written by an older build still carries the retired
    /// `agent_language` column: schema creation is `CREATE TABLE IF NOT EXISTS`
    /// and this runtime never migrates. Loading must therefore work against a
    /// table that has extra columns, and upserting must not depend on that
    /// column's presence.
    #[test]
    fn loads_and_upserts_against_a_pre_i18n_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                summary TEXT NOT NULL DEFAULT '',
                title TEXT,
                title_override TEXT,
                model_id TEXT NOT NULL DEFAULT '',
                provider_id TEXT,
                cwd TEXT,
                project TEXT,
                agent_language TEXT NOT NULL DEFAULT 'en',
                approval_mode INTEGER NOT NULL DEFAULT 0,
                reasoning_effort INTEGER NOT NULL DEFAULT 2,
                depth INTEGER NOT NULL DEFAULT 0,
                parent_id TEXT,
                archived INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0,
                has_unread INTEGER NOT NULL DEFAULT 0,
                errored INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                interacted_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
                session_started_at INTEGER NOT NULL DEFAULT (unixepoch()),
                revision INTEGER NOT NULL DEFAULT 0,
                cumulative_input_tokens INTEGER NOT NULL DEFAULT 0,
                cumulative_output_tokens INTEGER NOT NULL DEFAULT 0,
                cumulative_cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                cumulative_cache_read_input_tokens INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE thread_data (
                thread_id TEXT PRIMARY KEY,
                data_type TEXT NOT NULL,
                data BLOB NOT NULL
            );
            CREATE TABLE token_usage (
                thread_id TEXT NOT NULL,
                user_message_id TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                completed_at INTEGER,
                PRIMARY KEY (thread_id, user_message_id)
            );
            INSERT INTO threads (id, summary, agent_language, project, cwd)
                VALUES ('legacy', 'old row', 'zh-CN', '/tmp', '/tmp');",
        )
        .unwrap();
        let db = ThreadsDatabase {
            conn: Mutex::new(conn),
        };

        // The pre-existing row loads despite the extra column.
        let legacy = db.load("legacy").unwrap().expect("legacy row loads");
        assert_eq!(legacy.summary, "old row");
        assert_eq!(legacy.cwd, "/tmp");

        // A fresh record upserts into the same old table.
        let rec = sample_record("fresh");
        db.upsert(&rec, true).unwrap();
        let loaded = db.load("fresh").unwrap().expect("fresh row round-trips");
        assert_eq!(loaded.summary, rec.summary);
        assert_eq!(loaded.messages.len(), rec.messages.len());
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
        let mut per_model = HashMap::new();
        per_model.insert(
            "百炼/glm-5.2[1m]".to_string(),
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
            approval_mode: 1,
            reasoning_effort: 4,
            depth: 0,
            parent_id: None,
            archived: false,
            pinned: false,
            tag: None,
            created_at: 1_700_000_000,
            interacted_at: 1_700_000_100,
            updated_at: 1_700_000_200,
            session_started_at: 1_700_000_000,
            revision: 0,
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
            per_model_token_usage: per_model,
            background_tasks: vec![crate::background_task::TaskSnapshot {
                task_id: "monitor_1".into(),
                kind: crate::background_task::TaskKind::MonitorCommand,
                owner_thread_id: id.into(),
                description: "wait for CI".into(),
                status: crate::background_task::TaskStatus::Completed,
                created_at_ms: 1_700_000_000_000,
                ended_at_ms: Some(1_700_000_001_000),
                event_count: 2,
                total_bytes: 42,
                exit_code: Some(0),
                failure_summary: None,
                anchor_message_id: None,
                output_tail: String::new(),
            }],
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
        assert_eq!(loaded.approval_mode, 1);
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].role, Role::User);
        assert_eq!(loaded.background_tasks.len(), 1);
        assert_eq!(loaded.background_tasks[0].task_id, "monitor_1");
        assert!(!loaded.messages[0].id.is_empty());
        assert_eq!(loaded.cumulative_token_usage.input_tokens, 100);
        assert_eq!(loaded.cumulative_token_usage.cache_read_input_tokens, 20);
        let u = loaded.request_token_usage.get("u1").unwrap();
        assert_eq!(u.output_tokens, 50);
        let pm = loaded
            .per_model_token_usage
            .get("百炼/glm-5.2[1m]")
            .unwrap();
        assert_eq!(pm.input_tokens, 100);
        assert_eq!(pm.output_tokens, 50);
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
    fn archive_round_trip() {
        let db = open_mem();
        db.upsert(&sample_record("t1"), true).unwrap();
        assert!(!db.load("t1").unwrap().unwrap().archived);
        assert!(db.list(false).unwrap().iter().any(|s| s.id == "t1"));

        // Archive: row stays in `list(true)` but drops out of the default
        // active-only list, and the loaded record reflects the flag.
        db.archive("t1", true).unwrap();
        let rec = db.load("t1").unwrap().unwrap();
        assert!(rec.archived);
        assert!(!db.list(false).unwrap().iter().any(|s| s.id == "t1"));
        assert!(
            db.list(true)
                .unwrap()
                .iter()
                .any(|s| s.id == "t1" && s.archived)
        );

        // Unarchive: row comes back into the active list.
        db.archive("t1", false).unwrap();
        assert!(!db.load("t1").unwrap().unwrap().archived);
        assert!(db.list(false).unwrap().iter().any(|s| s.id == "t1"));
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

    #[test]
    fn upsert_rejects_stale_revision() {
        // A fire-and-forget save carrying an older revision must not overwrite a
        // newer row. This is the guard against the switch-then-return race:
        // the older snapshot would clobber the assistant turn the user already
        // sees after switching back.
        let db = open_mem();

        let mut v1 = sample_record("t1");
        v1.revision = 1;
        v1.summary = "first".into();
        db.upsert(&v1, true).unwrap();

        let mut v2 = sample_record("t1");
        v2.revision = 5;
        v2.summary = "fifth".into();
        db.upsert(&v2, true).unwrap();
        assert_eq!(db.load("t1").unwrap().unwrap().summary, "fifth");

        // An older revision (e.g. a lingering background save from before v2)
        // must be discarded, leaving the newer row intact.
        let mut stale = sample_record("t1");
        stale.revision = 2;
        stale.summary = "stale-overwrite".into();
        db.upsert(&stale, true).unwrap();

        let loaded = db.load("t1").unwrap().unwrap();
        assert_eq!(loaded.summary, "fifth");
        assert_eq!(loaded.revision, 5);
    }

    #[test]
    fn upsert_accepts_equal_revision() {
        // Equal revision is allowed so that non-state edits (rename, archive)
        // that don't bump persist_revision still take effect.
        let db = open_mem();
        let mut v1 = sample_record("t1");
        v1.revision = 3;
        db.upsert(&v1, true).unwrap();

        let mut v2 = sample_record("t1");
        v2.revision = 3;
        v2.title_override = Some("renamed".into());
        db.upsert(&v2, true).unwrap();

        let loaded = db.load("t1").unwrap().unwrap();
        assert_eq!(loaded.title_override.as_deref(), Some("renamed"));
    }

    #[test]
    fn upsert_does_not_overwrite_archived_or_pinned() {
        // Regression: a stale in-memory snapshot (archived=false) must not
        // clobber the DB's archived=true set by `archive()`. Same for pinned.
        // These flags are independent metadata managed exclusively by their
        // dedicated setters, not by the general upsert path.
        let db = open_mem();
        let rec = sample_record("t1");
        db.upsert(&rec, true).unwrap();

        // Archive and pin via the dedicated setters.
        db.archive("t1", true).unwrap();
        db.pin("t1", true).unwrap();
        let loaded = db.load("t1").unwrap().unwrap();
        assert!(loaded.archived);
        assert!(loaded.pinned);

        // A stale upsert carrying archived=false / pinned=false (e.g. from
        // an in-memory snapshot taken before the archive/pin) must not reset
        // those flags.
        let stale = sample_record("t1");
        assert!(!stale.archived);
        assert!(!stale.pinned);
        db.upsert(&stale, true).unwrap();

        let loaded = db.load("t1").unwrap().unwrap();
        assert!(loaded.archived, "archived flag must survive stale upsert");
        assert!(loaded.pinned, "pinned flag must survive stale upsert");
    }

    #[test]
    fn set_unread_is_independent_of_upsert() {
        // Regression: a stale snapshot upsert must not clobber the has_unread
        // flag set by `set_unread` — the sidebar's read state. Mirrors the
        // archived/pinned invariant: has_unread is owned exclusively by
        // `set_unread`, never by the general upsert path.
        let db = open_mem();
        db.upsert(&sample_record("t1"), true).unwrap();
        db.set_unread("t1", true).unwrap();
        assert!(db.list(false).unwrap()[0].has_unread);

        // A stale upsert carrying no knowledge of has_unread must leave it set.
        db.upsert(&sample_record("t1"), true).unwrap();
        assert!(db.list(false).unwrap()[0].has_unread);

        // Clearing and re-upserting must not resurrect the flag.
        db.set_unread("t1", false).unwrap();
        assert!(!db.list(false).unwrap()[0].has_unread);
        db.upsert(&sample_record("t1"), true).unwrap();
        assert!(!db.list(false).unwrap()[0].has_unread);
    }

    #[test]
    fn register_and_list_projects() {
        let db = open_mem();
        assert!(db.list_projects().unwrap().is_empty());

        db.register_project("/home/user/project-a").unwrap();
        db.register_project("/home/user/project-b").unwrap();

        let list = db.list_projects().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], "/home/user/project-a");
        assert_eq!(list[1], "/home/user/project-b");
    }

    #[test]
    fn register_project_is_idempotent() {
        let db = open_mem();
        db.register_project("/home/user/project-a").unwrap();
        db.register_project("/home/user/project-a").unwrap();
        assert_eq!(db.list_projects().unwrap().len(), 1);
    }

    #[test]
    fn register_empty_path_is_noop() {
        let db = open_mem();
        db.register_project("").unwrap();
        assert!(db.list_projects().unwrap().is_empty());
    }

    #[test]
    fn remove_project() {
        let db = open_mem();
        db.register_project("/home/user/project-a").unwrap();
        db.register_project("/home/user/project-b").unwrap();
        db.remove_project("/home/user/project-a").unwrap();

        let list = db.list_projects().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0], "/home/user/project-b");
    }

    #[test]
    fn open_enables_wal_on_file_databases() {
        let dir = tempfile::tempdir().unwrap();
        let db = ThreadsDatabase::open(&dir.path().join("threads.db")).unwrap();
        let mode: String = db
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        // WAL is persistent: a second open of the same file stays in WAL.
        drop(db);
        let second = ThreadsDatabase::open(&dir.path().join("threads.db")).unwrap();
        let mode: String = second
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    /// The multi-instance shape: several manox processes hold their own
    /// connections and hammer `upsert` (a read-then-write transaction).
    /// WAL + busy_timeout + the IMMEDIATE upsert transaction make the
    /// writers queue; a deferred read-then-write transaction would fail
    /// with SQLITE_BUSY_SNAPSHOT, which no busy handler retries (red under
    /// the pre-fix DEFERRED begin — verified by temporarily reverting the
    /// behavior).
    #[test]
    fn concurrent_upserts_through_separate_connections_queue_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("threads.db");
        ThreadsDatabase::open(&path).unwrap(); // create + schema

        let workers = 4u64;
        let rounds = 25u64;
        std::thread::scope(|scope| {
            for worker in 0..workers {
                let path = path.clone();
                scope.spawn(move || {
                    let db = ThreadsDatabase::open(&path).unwrap();
                    for round in 0..rounds {
                        let mut rec = sample_record(&format!("t{worker}"));
                        rec.revision = round * workers + worker;
                        db.upsert(&rec, true).unwrap();
                    }
                });
            }
        });
        let db = ThreadsDatabase::open(&path).unwrap();
        let all = db.list(true).unwrap();
        assert_eq!(all.len(), workers as usize, "{all:?}");
    }
}
