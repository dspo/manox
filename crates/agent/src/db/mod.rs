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
/// version that *predates* the current one runs the incremental migration
/// chain. A version that pre-dates v2 (i.e. unknown legacy) is still treated
/// as legacy and dropped wholesale — only the v2 → v3 and v3 → v4 steps are
/// preserved.
const SCHEMA_VERSION: i32 = 4;

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

    /// Apply incremental migrations from `version` up to [`SCHEMA_VERSION`],
    /// or — if `version` is too old to be safely migrated — drop and recreate
    /// the schema. The v2 → v3 step adds the `approval_mode` and `pinned`
    /// columns to `threads`; the v3 → v4 step adds the `revision` column used
    /// by the stale-snapshot guard. Pre-v2 schemas are not understood and are
    /// wiped so a corrupt or unrelated database never silently loads.
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
            .context("begin schema migration transaction")?;
        if version < 2 {
            // Unknown legacy schema: safest course is a full rebuild. Pre-v2
            // databases pre-date the current persistence design; any rows they
            // hold would be misinterpreted by the new schema, so dropping them
            // is the only sound option.
            tx.execute_batch(
                "DROP TABLE IF EXISTS token_usage;
                 DROP TABLE IF EXISTS thread_events;
                 DROP TABLE IF EXISTS thread_data;
                 DROP TABLE IF EXISTS threads;",
            )
            .context("drop legacy tables")?;
        } else {
            // v2 → v3: add the columns that v3's `create_table` expects but
            // that `CREATE TABLE IF NOT EXISTS` can't add to an existing
            // pre-v3 table. The new columns default to 0/OnRequest; any
            // thread with `yolo = 1` is upgraded to Yolo by the read path
            // in `threads.rs`.
            if version < 3 {
                tx.execute(
                    "ALTER TABLE threads ADD COLUMN approval_mode INTEGER NOT NULL DEFAULT 0;",
                    [],
                )
                .context("alter threads add approval_mode")?;
                tx.execute(
                    "ALTER TABLE threads ADD COLUMN pinned INTEGER NOT NULL DEFAULT 0;",
                    [],
                )
                .context("alter threads add pinned")?;
            }
            // v3 → v4: add the monotonic `revision` column backing the
            // stale-snapshot guard in `upsert`. Defaults to 0 so legacy rows
            // are treated as the earliest revision and any newer snapshot
            // supersedes them.
            if version < 4 {
                tx.execute(
                    "ALTER TABLE threads ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;",
                    [],
                )
                .context("alter threads add revision")?;
            }
        }
        threads::create_table(&tx)?;
        events::create_table(&tx)?;
        token_usage::create_table(&tx)?;
        terminals::create_table(&tx)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)
            .context("set user_version")?;
        tx.commit().context("commit schema migration transaction")?;
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
            approval_mode: 2,
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
    fn schema_migration_v2_to_v3_preserves_yolo_rows() {
        // Build a v2 database by hand: create the v2 `threads` table without
        // `approval_mode`, insert a row with yolo = 1, and run `init_schema`
        // again. The migration must (a) add the `approval_mode` column, (b)
        // keep the row, and (c) bump `user_version` to the current target.
        let mut conn = Connection::open_in_memory().unwrap();
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
                yolo INTEGER NOT NULL DEFAULT 0,
                depth INTEGER NOT NULL DEFAULT 0,
                parent_id TEXT,
                archived INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT 0,
                interacted_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0,
                session_started_at INTEGER NOT NULL DEFAULT 0,
                cumulative_input_tokens INTEGER NOT NULL DEFAULT 0,
                cumulative_output_tokens INTEGER NOT NULL DEFAULT 0,
                cumulative_cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                cumulative_cache_read_input_tokens INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE thread_data (
                 thread_id TEXT PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
                 data_type TEXT NOT NULL DEFAULT 'zstd',
                 data BLOB NOT NULL
             );
             CREATE TABLE thread_events (
                 thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
                 seq INTEGER NOT NULL,
                 event_type TEXT NOT NULL,
                 payload TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (thread_id, seq)
             );
             CREATE TABLE token_usage (
                 thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
                 user_message_id TEXT NOT NULL,
                 input_tokens INTEGER NOT NULL DEFAULT 0,
                 output_tokens INTEGER NOT NULL DEFAULT 0,
                 cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                 cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                 completed_at INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (thread_id, user_message_id)
             );
             PRAGMA user_version = 2;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, summary, model_id, cwd, yolo, depth, archived, created_at, interacted_at, updated_at, session_started_at) VALUES ('yolo-row','','','/tmp',1,0,0,0,0,0,0)",
            [],
        )
        .unwrap();
        ThreadsDatabase::init_schema(&mut conn).unwrap();
        let v: i32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let (yolo, mode): (i64, i64) = conn
            .query_row(
                "SELECT yolo, approval_mode FROM threads WHERE id = 'yolo-row'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(yolo, 1);
        // After the ALTER, the new column defaults to 0 (OnRequest) for legacy
        // rows. The promotion of `yolo=1` rows onto ApprovalMode::Yolo is
        // verified end-to-end by `load_promotes_legacy_yolo_to_approval_mode_yolo`
        // above — this test only guards the schema column itself.
        assert_eq!(mode, 0);
        // The v2→v3 migration also adds the `pinned` column (added to the
        // v3 `create_table` by main; pre-existing v2 dbs don't have it, and
        // `CREATE TABLE IF NOT EXISTS` is a no-op on those, so the migration
        // has to do it explicitly).
        let pinned: i64 = conn
            .query_row(
                "SELECT pinned FROM threads WHERE id = 'yolo-row'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pinned, 0);
    }

    #[test]
    fn load_promotes_legacy_yolo_to_approval_mode_yolo() {
        // Build a v2 db by hand with a row whose yolo = 1, run the migration,
        // then load the record and verify the read path maps the legacy row
        // onto ApprovalMode::Yolo (i64 == 2). The previous test only asserts
        // the SQL column defaults to 0; the promotion happens in code at
        // load time, so the only way to guard it is to call `load` end to end.
        let mut conn = Connection::open_in_memory().unwrap();
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
                yolo INTEGER NOT NULL DEFAULT 0,
                depth INTEGER NOT NULL DEFAULT 0,
                parent_id TEXT,
                archived INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT 0,
                interacted_at INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER NOT NULL DEFAULT 0,
                session_started_at INTEGER NOT NULL DEFAULT 0,
                cumulative_input_tokens INTEGER NOT NULL DEFAULT 0,
                cumulative_output_tokens INTEGER NOT NULL DEFAULT 0,
                cumulative_cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                cumulative_cache_read_input_tokens INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE thread_data (
                 thread_id TEXT PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
                 data_type TEXT NOT NULL DEFAULT 'zstd',
                 data BLOB NOT NULL
             );
             CREATE TABLE thread_events (
                 thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
                 seq INTEGER NOT NULL,
                 event_type TEXT NOT NULL,
                 payload TEXT NOT NULL,
                 created_at INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (thread_id, seq)
             );
             CREATE TABLE token_usage (
                 thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
                 user_message_id TEXT NOT NULL,
                 input_tokens INTEGER NOT NULL DEFAULT 0,
                 output_tokens INTEGER NOT NULL DEFAULT 0,
                 cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                 cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
                 completed_at INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (thread_id, user_message_id)
             );
             PRAGMA user_version = 2;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO threads (id, summary, model_id, cwd, project, yolo, depth, archived, created_at, interacted_at, updated_at, session_started_at) VALUES ('legacy-yolo','','','/tmp','',1,0,0,0,0,0,0)",
            [],
        )
        .unwrap();
        ThreadsDatabase::init_schema(&mut conn).unwrap();
        let db = ThreadsDatabase {
            conn: Mutex::new(conn),
        };
        let rec = db.load("legacy-yolo").unwrap().expect("row must exist");
        assert!(rec.yolo, "legacy yolo flag preserved");
        assert_eq!(
            rec.approval_mode, 2,
            "v2 yolo=1 row must promote to ApprovalMode::Yolo at load time"
        );
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
}
