//! `threads` + `thread_data` tables: lightweight metadata columns and the
//! zstd-compressed message BLOB.

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::language_model::TokenUsage;
use crate::message::Message;

use super::ThreadsDatabase;

/// Summary used by the sidebar list (no message bodies). All columns the
/// sidebar renders come from the lightweight `threads` table — the message
/// BLOB is never touched by `list`.
#[derive(Debug, Clone)]
pub struct ThreadSummary {
    pub id: String,
    /// Mechanical first-user-message summary (fallback display text).
    pub summary: String,
    /// LLM-generated title, if one has been streamed.
    pub title: Option<String>,
    /// User-supplied rename; takes display precedence over `title`.
    pub title_override: Option<String>,
    pub model_id: String,
    pub provider_id: Option<String>,
    pub approval_mode: i64,
    pub project: String,
    pub depth: i32,
    pub parent_id: Option<String>,
    pub archived: bool,
    /// Pinned flag toggled from the title bar menu. Pinned threads float to
    /// the top of the sidebar list (sorted first by `pinned DESC`).
    pub pinned: bool,
    /// Unread flag: the thread finished a turn the user has not yet viewed.
    /// Set on a background thread's terminal `Stop`/`Error`, cleared when the
    /// user switches into the thread. `upsert` never touches this column — only
    /// `set_unread` does — so snapshots cannot clobber the read state.
    pub has_unread: bool,
    /// Error flag: the last turn ended with a fatal error that prevented the
    /// thread from continuing. Set on `ThreadEvent::Error`, cleared when a new
    /// turn starts (`mark_running`). Independent of `upsert` — like `has_unread`
    /// — so snapshot saves never clobber it.
    pub errored: bool,
    pub created_at: i64,
    pub interacted_at: i64,
    pub updated_at: i64,
    /// Sum of the four cumulative token columns. Precomputed in SQL so the
    /// sidebar can show total tokens without reading the BLOB.
    pub cumulative_total_tokens: u64,
}

impl ThreadSummary {
    /// Display title with precedence: user rename > LLM title > summary.
    pub fn display_title(&self) -> &str {
        self.title_override
            .as_deref()
            .filter(|t| !t.trim().is_empty())
            .or_else(|| self.title.as_deref().filter(|t| !t.trim().is_empty()))
            .unwrap_or(&self.summary)
    }
}

/// Complete persistent record of a `Thread`.
#[derive(Debug, Clone)]
pub struct ThreadRecord {
    pub id: String,
    pub summary: String,
    pub title: Option<String>,
    pub title_override: Option<String>,
    pub model_id: String,
    pub provider_id: Option<String>,
    pub cwd: String,
    pub project: String,
    /// Agent-language token (`"en"` / `"zh-CN"`) snapshotted at thread creation
    /// — the thread's immutable harness/tool-description language. Never flips
    /// post-creation; a global settings change only affects threads created
    /// afterwards, so an existing thread's prompt-cache prefix stays byte-stable.
    pub agent_language: String,
    /// Three-state approval mode (0 = OnRequest, 1 = AutoReview, 2 = Yolo).
    /// Persisted as INTEGER for schema-stability across enum reorderings.
    pub approval_mode: i64,
    /// Reasoning effort (0 = Low, 1 = Medium, …, 6 = Auto).
    /// Persisted as INTEGER matching `ReasoningEffort::as_i64`.
    pub reasoning_effort: i64,
    pub depth: i32,
    pub parent_id: Option<String>,
    pub archived: bool,
    pub pinned: bool,
    pub created_at: i64,
    pub interacted_at: i64,
    pub updated_at: i64,
    pub session_started_at: i64,
    /// Monotonic snapshot revision from `Thread::persist_revision`. `upsert`
    /// refuses to write a record whose revision is older than the row's
    /// current revision, so an out-of-order (fire-and-forget) older snapshot
    /// can never clobber a newer one already committed — the root cause of
    /// assistant content vanishing after a thread switch when the submit-time
    /// snapshot raced ahead of the turn-end snapshot.
    pub revision: u64,
    pub cumulative_token_usage: TokenUsage,
    pub messages: Vec<Message>,
    /// Per-user-message token usage keyed by `Message::id`. Mirrored to the
    /// `token_usage` table for SQL queries; kept here as a whole-thread snapshot
    /// so a single `load` restores everything the in-memory `Thread` needs.
    pub request_token_usage: std::collections::HashMap<String, TokenUsage>,
    /// Per-model cumulative usage keyed by model display name. Restored into
    /// `TokenMeter::per_model` so the env card shows per-model totals for a
    /// thread the moment it opens, without waiting for a fresh stream.
    pub per_model_token_usage: std::collections::HashMap<String, TokenUsage>,
}

/// Decompressed payload of the `thread_data` BLOB.
#[derive(Serialize, Deserialize)]
struct ThreadData {
    messages: Vec<Message>,
    request_token_usage: std::collections::HashMap<String, TokenUsage>,
    per_model_token_usage: std::collections::HashMap<String, TokenUsage>,
}

const COMPRESSION_LEVEL: i32 = 3;

pub fn create_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS threads (
            id TEXT PRIMARY KEY,
            summary TEXT NOT NULL DEFAULT '',
            title TEXT,
            title_override TEXT,
            model_id TEXT NOT NULL DEFAULT '',
            provider_id TEXT,
            cwd TEXT,
            project TEXT,
            agent_language TEXT NOT NULL DEFAULT 'en',
            approval_mode INTEGER NOT NULL DEFAULT 1,
            reasoning_effort INTEGER NOT NULL DEFAULT 1,
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
        CREATE INDEX IF NOT EXISTS idx_threads_active_recent ON threads(archived, interacted_at DESC);
        CREATE INDEX IF NOT EXISTS idx_threads_parent_id ON threads(parent_id);
        CREATE INDEX IF NOT EXISTS idx_threads_pinned ON threads(archived, pinned DESC, interacted_at DESC);

        CREATE TABLE IF NOT EXISTS thread_data (
            thread_id TEXT PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
            data_type TEXT NOT NULL DEFAULT 'zstd',
            data BLOB NOT NULL
        );",
    )
    .context("create threads/thread_data tables")?;
    Ok(())
}

impl ThreadsDatabase {
    /// Upsert a thread record. When `touch` is true, both `interacted_at` and
    /// `updated_at` advance to now (real user activity). When `touch` is false,
    /// only `updated_at` advances; `interacted_at` is preserved (e.g. saving
    /// state on thread switch without implying the user interacted with it).
    pub fn upsert(&self, rec: &ThreadRecord, touch: bool) -> Result<()> {
        let data = ThreadData {
            messages: rec.messages.clone(),
            request_token_usage: rec.request_token_usage.clone(),
            per_model_token_usage: rec.per_model_token_usage.clone(),
        };
        let json = serde_json::to_vec(&data).context("serialize thread data")?;
        let compressed =
            zstd::encode_all(json.as_slice(), COMPRESSION_LEVEL).context("zstd compress")?;
        let now = chrono::Utc::now().timestamp();
        // interacted_at only advances on real activity; otherwise keep the record's value.
        let interacted_at = if touch { now } else { rec.interacted_at };

        let mut conn = self.conn.lock().expect("db mutex poisoned");
        // Atomicity: the threads row, the BLOB, and the token_usage mirror must
        // land together or not at all — a partial write would leave a sidebar
        // entry that fails to load. One transaction wraps all three.
        let tx = conn.transaction().context("begin upsert transaction")?;
        // Stale-snapshot guard: `save_thread` is fire-and-forget, so an older
        // snapshot (e.g. taken at submit, before the turn produced any assistant
        // content) can commit after a newer snapshot (taken at turn end) if the
        // background executor reorders them. Without this guard the older write
        // would clobber the newer one — silently deleting assistant messages and
        // the token_usage mirror. The Mutex serializes upserts, so a
        // within-transaction SELECT-then-UPDATE is atomic: if this record's
        // revision is not newer than the row's, abandon the write entirely.
        let existing_revision: Option<i64> = tx
            .query_row(
                "SELECT revision FROM threads WHERE id = ?1",
                params![rec.id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .context("read existing revision")?;
        if existing_revision.is_some_and(|r| (rec.revision as i64) < r) {
            return Ok(());
        }
        tx.execute(
            "INSERT INTO threads (
                id, summary, title, title_override, model_id, provider_id, cwd, project,
                agent_language, approval_mode, reasoning_effort, depth, parent_id, archived, pinned, created_at, interacted_at, updated_at,
                session_started_at, revision, cumulative_input_tokens, cumulative_output_tokens,
                cumulative_cache_creation_input_tokens, cumulative_cache_read_input_tokens
             )
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)
             ON CONFLICT(id) DO UPDATE SET
                summary = excluded.summary,
                title = excluded.title,
                title_override = excluded.title_override,
                model_id = excluded.model_id,
                provider_id = excluded.provider_id,
                cwd = excluded.cwd,
                project = excluded.project,
                agent_language = excluded.agent_language,
                approval_mode = excluded.approval_mode,
                reasoning_effort = excluded.reasoning_effort,
                depth = excluded.depth,
                parent_id = excluded.parent_id,
                interacted_at = excluded.interacted_at,
                updated_at = excluded.updated_at,
                session_started_at = excluded.session_started_at,
                revision = excluded.revision,
                cumulative_input_tokens = excluded.cumulative_input_tokens,
                cumulative_output_tokens = excluded.cumulative_output_tokens,
                cumulative_cache_creation_input_tokens = excluded.cumulative_cache_creation_input_tokens,
                cumulative_cache_read_input_tokens = excluded.cumulative_cache_read_input_tokens",
            params![
                rec.id,
                rec.summary,
                rec.title,
                rec.title_override,
                rec.model_id,
                rec.provider_id,
                rec.cwd,
                rec.project,
                rec.agent_language,
                rec.approval_mode,
                rec.reasoning_effort,
                rec.depth,
                rec.parent_id,
                rec.archived as i64,
                rec.pinned as i64,
                rec.created_at,
                interacted_at,
                now,
                rec.session_started_at,
                rec.revision as i64,
                rec.cumulative_token_usage.input_tokens as i64,
                rec.cumulative_token_usage.output_tokens as i64,
                rec.cumulative_token_usage.cache_creation_input_tokens as i64,
                rec.cumulative_token_usage.cache_read_input_tokens as i64,
            ],
        )
        .context("upsert thread")?;
        tx.execute(
            "INSERT INTO thread_data (thread_id, data_type, data)
             VALUES (?1, 'zstd', ?2)
             ON CONFLICT(thread_id) DO UPDATE SET data_type = excluded.data_type, data = excluded.data",
            params![rec.id, compressed],
        )
        .context("upsert thread_data")?;
        // Replace the per-message mirror wholesale so rows for user messages
        // no longer in the map (e.g. after a future compaction) don't leak as
        // stale orphans.
        tx.execute(
            "DELETE FROM token_usage WHERE thread_id = ?1",
            params![rec.id],
        )
        .context("clear token_usage mirror")?;
        for (uid, u) in &rec.request_token_usage {
            tx.execute(
                "INSERT INTO token_usage
                    (thread_id, user_message_id, input_tokens, output_tokens,
                     cache_creation_input_tokens, cache_read_input_tokens, completed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch())
                 ON CONFLICT(thread_id, user_message_id) DO UPDATE SET
                    input_tokens = excluded.input_tokens,
                    output_tokens = excluded.output_tokens,
                    cache_creation_input_tokens = excluded.cache_creation_input_tokens,
                    cache_read_input_tokens = excluded.cache_read_input_tokens,
                    completed_at = excluded.completed_at",
                params![
                    rec.id,
                    uid,
                    u.input_tokens as i64,
                    u.output_tokens as i64,
                    u.cache_creation_input_tokens as i64,
                    u.cache_read_input_tokens as i64,
                ],
            )
            .context("upsert token_usage")?;
        }
        tx.commit().context("commit upsert transaction")?;
        Ok(())
    }

    /// Load a full record by id. Returns `None` if absent.
    pub fn load(&self, id: &str) -> Result<Option<ThreadRecord>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let row = conn.query_row(
            "SELECT id, summary, title, title_override, model_id, provider_id, cwd, project,
                    agent_language, approval_mode, reasoning_effort, depth, parent_id, archived, pinned, created_at, interacted_at, updated_at,
                    session_started_at, revision, cumulative_input_tokens, cumulative_output_tokens,
                    cumulative_cache_creation_input_tokens, cumulative_cache_read_input_tokens
             FROM threads WHERE id = ?1",
            params![id],
            |row| {
                Ok(ThreadRecord {
                    id: row.get(0)?,
                    summary: row.get(1)?,
                    title: row.get(2)?,
                    title_override: row.get(3)?,
                    model_id: row.get(4)?,
                    provider_id: row.get(5)?,
                    cwd: row.get(6)?,
                    project: row.get(7)?,
                    agent_language: row.get(8)?,
                    approval_mode: row.get(9)?,
                    reasoning_effort: row.get(10)?,
                    depth: row.get(11)?,
                    parent_id: row.get(12)?,
                    archived: row.get::<_, i64>(13)? != 0,
                    pinned: row.get::<_, i64>(14)? != 0,
                    created_at: row.get(15)?,
                    interacted_at: row.get(16)?,
                    updated_at: row.get(17)?,
                    session_started_at: row.get(18)?,
                    revision: row.get::<_, i64>(19)? as u64,
                    cumulative_token_usage: TokenUsage {
                        input_tokens: row.get::<_, i64>(20)? as u64,
                        output_tokens: row.get::<_, i64>(21)? as u64,
                        cache_creation_input_tokens: row.get::<_, i64>(22)? as u64,
                        cache_read_input_tokens: row.get::<_, i64>(23)? as u64,
                    },
                    // Filled from the BLOB below.
                    messages: Vec::new(),
                    request_token_usage: std::collections::HashMap::new(),
                    per_model_token_usage: std::collections::HashMap::new(),
                })
            },
        );
        let mut rec = match row {
            Ok(r) => r,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e).context("load thread"),
        };
        // A threads row without a matching thread_data BLOB is a damaged record
        // (only reachable via an external editor or a pre-transaction crash).
        // Degrade gracefully: return the metadata with empty messages + usage so
        // the sidebar entry still opens instead of erroring out permanently.
        let blob: Vec<u8> = match conn.query_row(
            "SELECT data FROM thread_data WHERE thread_id = ?1",
            params![id],
            |row| row.get(0),
        ) {
            Ok(b) => b,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(Some(rec)),
            Err(e) => return Err(e).context("load thread_data"),
        };
        let decompressed = zstd::decode_all(blob.as_slice()).context("zstd decode")?;
        let data: ThreadData =
            serde_json::from_slice(&decompressed).context("deserialize thread data")?;
        rec.messages = data.messages;
        rec.request_token_usage = data.request_token_usage;
        rec.per_model_token_usage = data.per_model_token_usage;
        Ok(Some(rec))
    }

    /// List thread summaries, newest by `interacted_at` first. When
    /// `include_archived` is false, archived threads are excluded (the sidebar
    /// default).
    pub fn list(&self, include_archived: bool) -> Result<Vec<ThreadSummary>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let sql = if include_archived {
            "SELECT id, summary, title, title_override, model_id, provider_id, approval_mode, project, depth,
                    parent_id, archived, pinned, created_at, interacted_at, updated_at,
                    cumulative_input_tokens + cumulative_output_tokens
                        + cumulative_cache_creation_input_tokens + cumulative_cache_read_input_tokens,
                    has_unread,
                    errored
                    FROM threads ORDER BY pinned DESC, interacted_at DESC"
        } else {
            "SELECT id, summary, title, title_override, model_id, provider_id, approval_mode, project, depth,
                    parent_id, archived, pinned, created_at, interacted_at, updated_at,
                    cumulative_input_tokens + cumulative_output_tokens
                        + cumulative_cache_creation_input_tokens + cumulative_cache_read_input_tokens,
                    has_unread,
                    errored
                    FROM threads WHERE archived = 0 ORDER BY pinned DESC, interacted_at DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(ThreadSummary {
                id: row.get(0)?,
                summary: row.get(1)?,
                title: row.get(2)?,
                title_override: row.get(3)?,
                model_id: row.get(4)?,
                provider_id: row.get(5)?,
                approval_mode: row.get(6)?,
                project: row.get(7)?,
                depth: row.get(8)?,
                parent_id: row.get(9)?,
                archived: row.get::<_, i64>(10)? != 0,
                pinned: row.get::<_, i64>(11)? != 0,
                created_at: row.get(12)?,
                interacted_at: row.get(13)?,
                updated_at: row.get(14)?,
                cumulative_total_tokens: row.get::<_, i64>(15)? as u64,
                has_unread: row.get::<_, i64>(16)? != 0,
                errored: row.get::<_, i64>(17)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Mark a thread archived (or unarchive).
    pub fn archive(&self, id: &str, archived: bool) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "UPDATE threads SET archived = ?1, updated_at = unixepoch() WHERE id = ?2",
            params![archived as i64, id],
        )
        .context("archive thread")?;
        Ok(())
    }

    /// Set the unread flag on a thread. Independent of `upsert` so snapshot
    /// saves can never clobber the read state.
    pub fn set_unread(&self, id: &str, unread: bool) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "UPDATE threads SET has_unread = ?1 WHERE id = ?2",
            params![unread as i64, id],
        )
        .context("set thread unread")?;
        Ok(())
    }

    /// Set the errored flag on a thread. Independent of `upsert` so snapshot
    /// saves can never clobber the error state.
    pub fn set_errored(&self, id: &str, errored: bool) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "UPDATE threads SET errored = ?1 WHERE id = ?2",
            params![errored as i64, id],
        )
        .context("set thread errored")?;
        Ok(())
    }

    /// Toggle the pinned flag on a thread. Pinned threads float to the top of
    /// the sidebar list (SQL `ORDER BY pinned DESC, interacted_at DESC`).
    pub fn pin(&self, id: &str, pinned: bool) -> Result<()> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        conn.execute(
            "UPDATE threads SET pinned = ?1, updated_at = unixepoch() WHERE id = ?2",
            params![pinned as i64, id],
        )
        .context("pin thread")?;
        Ok(())
    }

    /// Return distinct non-empty project paths ordered by most recent activity.
    /// Used by the project-chip dropdown to populate "recent projects".
    pub fn list_recent_projects(&self, limit: usize) -> Result<Vec<String>> {
        let conn = self.conn.lock().expect("db mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT project, MAX(interacted_at) AS last_used
             FROM threads
             WHERE project IS NOT NULL AND project != ''
             GROUP BY project
             ORDER BY last_used DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("read project row")?);
        }
        Ok(out)
    }
}

#[cfg(test)]
impl ThreadRecord {
    /// Minimal record for tests: empty metadata, zeroed timestamps, no tokens.
    pub fn for_test(id: &str, cwd: &str, messages: Vec<Message>) -> Self {
        Self {
            id: id.into(),
            summary: String::new(),
            title: None,
            title_override: None,
            model_id: String::new(),
            provider_id: None,
            cwd: cwd.into(),
            project: String::new(),
            agent_language: "en".into(),
            approval_mode: 0,
            reasoning_effort: 1,
            depth: 0,
            parent_id: None,
            archived: false,
            pinned: false,
            created_at: 0,
            interacted_at: 0,
            updated_at: 0,
            session_started_at: 0,
            revision: 0,
            cumulative_token_usage: TokenUsage::default(),
            messages,
            request_token_usage: std::collections::HashMap::new(),
            per_model_token_usage: std::collections::HashMap::new(),
        }
    }
}
