//! SQLite persistence.
//!
//! Five tables back the metadata model:
//! - `threads`: lightweight per-thread metadata + cumulative token columns.
//!   The sidebar list query reads only this table — never the message BLOB —
//!   so a long history stays cheap to enumerate.
//! - `thread_data`: a single zstd-compressed JSON BLOB per thread holding the
//!   full `messages` array and the `request_token_usage` map (the heavy state).
//! - `thread_events`: an append-only event stream (model_change / compaction /
//!   branch_summary / custom) as queryable rows.
//! - `token_usage`: per-user-message token breakdown, queryable without
//!   decompressing the message BLOB.
//! - `terminal_sessions`: per-terminal metadata (cwd/env/title) for tab
//!   restore; scrollback is not persisted.
//! - `thread_ui_notes`: persisted UI annotations (`Error` / `Notice` cards)
//!   that are not part of the model-facing message list — reload splices them
//!   back into the conversation without touching the request prefix.
//!
//! `ThreadsDatabase` holds a `Mutex<Connection>`; all methods are synchronous
//! and blocking (callers wrap them in `background_spawn`).

mod events;
mod projects;
mod terminals;
mod threads;
mod token_usage;
mod ui_notes;

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context as _, Result};
use rusqlite::Connection;

pub use events::{ThreadEventRecord, ThreadEventType};
pub use terminals::TerminalSession;
pub use threads::{ThreadRecord, ThreadSummary};
pub use token_usage::TokenUsageRecord;
pub use ui_notes::{UiNoteKind, UiNoteRecord};

use crate::paths;

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
        let conn = Connection::open(path)
            .with_context(|| format!("open threads db: {}", path.display()))?;
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
        threads::create_table(conn)?;
        events::create_table(conn)?;
        token_usage::create_table(conn)?;
        terminals::create_table(conn)?;
        ui_notes::create_table(conn)?;
        projects::create_table(conn)?;
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
        let conn = Connection::open_in_memory().unwrap();
        ThreadsDatabase::init_schema(&conn).unwrap();
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
            approval_mode: 2,
            reasoning_effort: 4,
            depth: 0,
            parent_id: None,
            archived: false,
            pinned: false,
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
        assert_eq!(loaded.approval_mode, 2);
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].role, Role::User);
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
    fn ui_notes_round_trip() {
        // `init_schema` must create the table, and record/list must round-trip
        // kind + anchor + payload with monotonic per-thread seq.
        let db = open_mem();
        db.upsert(&sample_record("t1"), true).unwrap();

        db.record_ui_note(
            "t1",
            UiNoteKind::Error,
            Some("u1"),
            &serde_json::json!({ "text": "boom" }),
        )
        .unwrap();
        db.record_ui_note(
            "t1",
            UiNoteKind::Notice,
            None,
            &serde_json::json!({ "text": "hi" }),
        )
        .unwrap();

        let notes = db.list_ui_notes("t1").unwrap();
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].seq, 1);
        assert_eq!(notes[1].seq, 2);
        assert_eq!(notes[0].kind, UiNoteKind::Error);
        assert_eq!(notes[0].anchor_user_id.as_deref(), Some("u1"));
        assert_eq!(
            notes[0].data.get("text").and_then(|v| v.as_str()),
            Some("boom")
        );
        assert_eq!(notes[1].kind, UiNoteKind::Notice);
        assert!(notes[1].anchor_user_id.is_none());

        // Seq is per-thread, so a second thread starts fresh at 1.
        db.upsert(&sample_record("t2"), true).unwrap();
        db.record_ui_note(
            "t2",
            UiNoteKind::Notice,
            None,
            &serde_json::json!({ "text": "x" }),
        )
        .unwrap();
        let t2 = db.list_ui_notes("t2").unwrap();
        assert_eq!(t2.len(), 1);
        assert_eq!(t2[0].seq, 1);

        // Unknown kind strings degrade to Notice rather than panicking, so a
        // future wire value never breaks reload.
        let conn = db.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO thread_ui_notes (thread_id, seq, kind, data) VALUES ('t1', 99, 'bogus', '{}')",
            [],
        )
        .unwrap();
        drop(conn);
        let all = db.list_ui_notes("t1").unwrap();
        assert_eq!(all.last().unwrap().kind, UiNoteKind::Notice);
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
}
