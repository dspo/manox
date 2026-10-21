//! The object store: projects and conversations as first-class durable
//! rows in `~/.manox/core/objects.db`.
//!
//! Division of durable authority (v1.0.0):
//!
//! - The **journal** records the conversation process — messages, thinking,
//!   tool calls and results, `cwd_change`. Reopening a conversation rebuilds
//!   its transcript from the journal.
//! - The **object store** records user will and object metadata — the
//!   project a conversation belongs to (never derived from any cwd), the
//!   model and permission picks, drafts, pin/archive/title flags. Reopening
//!   restores the conversation's configuration from its row.
//!
//! The two field sets are disjoint by construction: no value is mirrored
//! in both stores, so the "journal says A, the table says B" mismatch
//! class cannot arise. The store is written synchronously through direct
//! setter calls (the desktop app links this crate in-process); the engine
//! reads fresh at request and tool boundaries.
//!
//! Materialization: a conversation "actually happened" once a human has
//! sent it a message — `last_human_message_at IS NOT NULL`. The stamp is
//! written in the same critical section as the first journal append, and
//! boot reconciliation repairs both directions (journal without a stamp,
//! stamp without a journal), so un-materialized rows are exactly the
//! draft-refill candidates.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rusqlite::{Connection, OptionalExtension, params};

/// One project row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub id: String,
    pub path: String,
    pub alias: Option<String>,
    /// Folder position, ascending; manual moves rewrite it, registration
    /// takes the head.
    pub position: i64,
}

/// One conversation row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationRow {
    pub id: String,
    pub project_id: Option<String>,
    pub model_provider: Option<String>,
    pub model_id: Option<String>,
    pub permission_mode: Option<String>,
    pub reasoning_effort: Option<String>,
    pub draft: Option<String>,
    pub title: Option<String>,
    pub pinned: bool,
    pub archived: bool,
    pub unread: bool,
    pub errored: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// Unix seconds of the last human-authored message; `None` until the
    /// conversation actually happens.
    pub last_human_message_at: Option<i64>,
    /// Bumped by every configuration write; the engine compares its
    /// seen-version against this to notice mid-conversation changes.
    pub config_version: i64,
}

/// The shared sqlite connection. Every store call is a short synchronous
/// critical section (microseconds) — safe from the UI thread and the
/// engine actor alike; cross-process access serializes on WAL +
/// busy_timeout.
static GLOBAL: OnceLock<Mutex<Option<Connection>>> = OnceLock::new();
#[cfg(any(test, feature = "test-support"))]
static TEST_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);

fn globals() -> &'static Mutex<Option<Connection>> {
    GLOBAL.get_or_init(|| Mutex::new(None))
}

fn connect(path: &Path) -> Connection {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path).expect("open objects.db");
    conn.busy_timeout(std::time::Duration::from_secs(2))
        .expect("busy timeout");
    conn.pragma_update(None, "journal_mode", "WAL")
        .expect("wal mode");
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS projects (
            id         TEXT PRIMARY KEY,
            path       TEXT NOT NULL UNIQUE,
            alias      TEXT,
            position   INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS conversations (
            id                    TEXT PRIMARY KEY,
            project_id            TEXT REFERENCES projects(id),
            model_provider        TEXT,
            model_id              TEXT,
            permission_mode       TEXT,
            reasoning_effort      TEXT,
            draft                 TEXT,
            title                 TEXT,
            pinned                INTEGER NOT NULL DEFAULT 0,
            archived              INTEGER NOT NULL DEFAULT 0,
            unread                INTEGER NOT NULL DEFAULT 0,
            errored               INTEGER NOT NULL DEFAULT 0,
            created_at            INTEGER NOT NULL,
            updated_at            INTEGER NOT NULL,
            last_human_message_at INTEGER,
            config_version        INTEGER NOT NULL DEFAULT 0
        );
        "#,
    )
    .expect("schema");
    conn
}

fn store_path() -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(path) = TEST_PATH.lock().unwrap().clone() {
        return path;
    }
    crate::paths::manox_config_dir()
        .map(|dir| dir.join("core").join("objects.db"))
        .unwrap_or_else(|_| PathBuf::from("objects.db"))
}

/// Open (creating) the store and install the process-global connection.
/// Called at agent init; tests point it at a scratch path via
/// [`set_path_for_test`].
pub fn init() {
    let conn = connect(&store_path());
    *globals().lock().unwrap() = Some(conn);
}

/// Point the store at a scratch file (test-support only — the real state
/// home must never be touched by tests). `None` restores the default path.
#[cfg(any(test, feature = "test-support"))]
pub fn set_path_for_test(path: Option<PathBuf>) {
    *TEST_PATH.lock().unwrap() = path;
    *globals().lock().unwrap() = None;
}

fn with<R>(f: impl FnOnce(&Connection) -> R) -> R {
    let guard = globals()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(conn) = guard.as_ref() else {
        panic!("object store not initialized; call object_store::init first")
    };
    f(conn)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Serializes tests that swap the process-global connection (the same
/// discipline the thread store's tests use).
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The zero-sized handle to the process-global store; every method is one
/// short synchronous statement. Clone and share freely.
#[derive(Debug, Clone, Copy, Default)]
pub struct ObjectStore;

impl ObjectStore {
    /// The store if the process initialized one.
    pub fn try_global() -> Option<Self> {
        globals().lock().unwrap().is_some().then_some(ObjectStore)
    }

    // ── projects ────────────────────────────────────────────────────────

    /// Register a project path (idempotent). A newly registered project
    /// takes the HEAD position — later-added renders higher until a manual
    /// order says otherwise. Returns the project id.
    pub fn register_project(&self, path: &str) -> String {
        with(|conn| {
            let at = now();
            if let Some(id) = conn
                .query_row(
                    "SELECT id FROM projects WHERE path = ?1",
                    params![path],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .expect("project lookup")
            {
                return id;
            }
            let id = uuid::Uuid::new_v4().to_string();
            let head: i64 = conn
                .query_row(
                    "SELECT COALESCE(MIN(position), 0) FROM projects",
                    [],
                    |row| row.get(0),
                )
                .expect("project head");
            conn.execute(
                "INSERT INTO projects (id, path, alias, position, created_at, updated_at)
                 VALUES (?1, ?2, NULL, ?3, ?4, ?4)",
                params![id, path, head - 1, at],
            )
            .expect("project insert");
            id
        })
    }

    /// Set (or clear, on `None`) a project's display alias.
    pub fn set_project_alias(&self, path: &str, alias: Option<&str>) -> bool {
        with(|conn| {
            conn.execute(
                "UPDATE projects SET alias = ?1, updated_at = ?2 WHERE path = ?3",
                params![alias, now(), path],
            )
            .expect("alias update")
                > 0
        })
    }

    /// Move a project before another (manual folder order); `before = None`
    /// appends to the tail.
    pub fn move_project(&self, path: &str, before: Option<&str>) -> bool {
        with(|conn| {
            let positions: Vec<(String, i64)> = {
                let mut stmt = conn
                    .prepare("SELECT path, position FROM projects ORDER BY position")
                    .expect("order query");
                let rows = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                    .expect("order rows");
                rows.filter_map(Result::ok).collect()
            };
            if !positions.iter().any(|(p, _)| p == path) {
                return false;
            }
            // Rewrite positions as a dense sequence with the move applied:
            // drop the dragged path, re-insert before the anchor (or at the
            // tail), then renumber ascending.
            let mut ordered: Vec<&String> = positions
                .iter()
                .map(|(p, _)| p)
                .filter(|p| p.as_str() != path)
                .collect();
            let at = match before {
                Some(_) => ordered
                    .iter()
                    .position(|p| Some(p.as_str()) == before)
                    .unwrap_or(ordered.len()),
                None => ordered.len(),
            };
            let path = path.to_string();
            ordered.insert(at, &path);
            for (ix, p) in ordered.iter().enumerate() {
                conn.execute(
                    "UPDATE projects SET position = ?1, updated_at = ?2 WHERE path = ?3",
                    params![ix as i64, now(), p],
                )
                .expect("position update");
            }
            true
        })
    }

    /// A project's display alias, when the user set one.
    pub fn project_alias(&self, path: &str) -> Option<String> {
        with(|conn| {
            conn.query_row(
                "SELECT alias FROM projects WHERE path = ?1",
                params![path],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .expect("alias query")
            .flatten()
        })
    }

    /// The path of a project id, when it exists.
    pub fn project_path(&self, id: &str) -> Option<String> {
        with(|conn| {
            conn.query_row(
                "SELECT path FROM projects WHERE id = ?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .expect("project path query")
        })
    }

    /// All projects in committed folder order.
    pub fn list_projects(&self) -> Vec<ProjectRow> {
        with(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, path, alias, position FROM projects ORDER BY position")
                .expect("project list query");
            let rows = stmt
                .query_map([], |row| {
                    Ok(ProjectRow {
                        id: row.get(0)?,
                        path: row.get(1)?,
                        alias: row.get(2)?,
                        position: row.get(3)?,
                    })
                })
                .expect("project list rows");
            rows.filter_map(Result::ok).collect()
        })
    }

    // ── conversations ───────────────────────────────────────────────────

    /// Create a conversation row (idempotent on the id). Creating is cheap
    /// and immediate: the row exists before any journal or directory does.
    pub fn create_conversation(&self, id: &str, project_path: Option<&str>) -> bool {
        // Resolve the project BEFORE taking the store lock: register_project
        // locks it too, and the std Mutex is not reentrant.
        let project_id = project_path.map(|path| self.register_project(path));
        with(|conn| {
            let at = now();
            conn.execute(
                "INSERT OR IGNORE INTO conversations (id, project_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?3)",
                params![id, project_id, at],
            )
            .expect("conversation insert")
                > 0
        })
    }

    /// One configuration write: the field assignments plus the automatic
    /// config-version bump and updated_at. Placeholders bind positionally —
    /// fields first, then the timestamp, then the conversation id.
    ///
    /// The row is materialized first when the store has never seen the id.
    /// A conversation id is minted by the client, and its row is the
    /// object's birth — no journal and no session directory exist until the
    /// user speaks — so a write that arrives ahead of any create still
    /// addresses a real object. Refusing it would either abort the caller
    /// or drop the pick silently, depending on the build profile.
    fn bump(conn: &Connection, id: &str, assignments: &str, fields: &[&dyn rusqlite::ToSql]) {
        let at = now();
        let materialized = conn
            .execute(
                "INSERT OR IGNORE INTO conversations (id, created_at, updated_at) \
                 VALUES (?1, ?2, ?2)",
                params![id, at],
            )
            .expect("conversation ensure");
        if materialized > 0 {
            tracing::debug!(conversation = id, "config write materialized the row");
        }
        let mut params: Vec<&dyn rusqlite::ToSql> = fields.to_vec();
        params.push(&at);
        params.push(&id);
        conn.execute(
            &format!(
                "UPDATE conversations SET {assignments}, config_version = config_version + 1, \
                 updated_at = ?{n} WHERE id = ?{k}",
                n = params.len() - 1,
                k = params.len()
            ),
            rusqlite::params_from_iter(params),
        )
        .expect("conversation update");
    }

    /// Attach a conversation to a project (registering the path first).
    /// Never moves any cwd — belonging and working directory are disjoint.
    pub fn attach(&self, id: &str, project_path: &str) -> bool {
        let project_id = self.register_project(project_path);
        with(|conn| {
            Self::bump(conn, id, "project_id = ?1", &[&project_id]);
            true
        })
    }

    /// Detach: the conversation becomes project-less; its cwd is untouched.
    pub fn detach(&self, id: &str) {
        with(|conn| {
            Self::bump(conn, id, "project_id = NULL", &[]);
        })
    }

    /// Set the model pick. Bumps the config version so the engine's next
    /// request notices and announces the switch.
    pub fn set_model(&self, id: &str, provider: Option<&str>, model_id: Option<&str>) {
        with(|conn| {
            Self::bump(
                conn,
                id,
                "model_provider = ?1, model_id = ?2",
                &[&provider, &model_id],
            );
        })
    }

    /// Set the permission mode pick (read by the engine before every tool).
    pub fn set_permission_mode(&self, id: &str, mode: Option<&str>) {
        with(|conn| {
            Self::bump(conn, id, "permission_mode = ?1", &[&mode]);
        })
    }

    /// Set the reasoning effort pick.
    pub fn set_reasoning_effort(&self, id: &str, effort: Option<&str>) {
        with(|conn| {
            Self::bump(conn, id, "reasoning_effort = ?1", &[&effort]);
        })
    }

    /// Persist the composer draft (debounced by the caller). Pure UI state:
    /// no config-version bump — the model never needs to know.
    pub fn set_draft(&self, id: &str, draft: Option<&str>) {
        with(|conn| {
            conn.execute(
                "UPDATE conversations SET draft = ?1, updated_at = ?2 WHERE id = ?3",
                params![draft, now(), id],
            )
            .expect("draft update");
        })
    }

    /// Pin or unpin.
    pub fn set_pinned(&self, id: &str, pinned: bool) {
        with(|conn| {
            Self::bump(conn, id, "pinned = ?1", &[&pinned]);
        })
    }

    /// Archive or unarchive. An archived conversation keeps its row — the
    /// object outlives its listing visibility.
    pub fn set_archived(&self, id: &str, archived: bool) {
        with(|conn| {
            Self::bump(conn, id, "archived = ?1", &[&archived]);
        })
    }

    /// Rename (the display title).
    pub fn set_title(&self, id: &str, title: Option<&str>) {
        with(|conn| {
            Self::bump(conn, id, "title = ?1", &[&title]);
        })
    }

    /// The unread / errored display bits.
    pub fn set_unread(&self, id: &str, unread: bool) {
        with(|conn| {
            conn.execute(
                "UPDATE conversations SET unread = ?1, updated_at = ?2 WHERE id = ?3",
                params![unread, now(), id],
            )
            .expect("unread update");
        })
    }

    pub fn set_errored(&self, id: &str, errored: bool) {
        with(|conn| {
            conn.execute(
                "UPDATE conversations SET errored = ?1, updated_at = ?2 WHERE id = ?3",
                params![errored, now(), id],
            )
            .expect("errored update");
        })
    }

    /// The first human message landed: stamp materialization and the
    /// ordering key in one write, and clear the draft (it was sent).
    pub fn mark_human_message(&self, id: &str, at: i64) {
        with(|conn| {
            conn.execute(
                "UPDATE conversations SET last_human_message_at = ?1, draft = NULL, \
                 updated_at = ?1 WHERE id = ?2",
                params![at, id],
            )
            .expect("materialization stamp");
        })
    }

    /// One conversation's row, if it exists.
    pub fn conversation(&self, id: &str) -> Option<ConversationRow> {
        with(|conn| {
            conn.query_row(
                "SELECT id, project_id, model_provider, model_id, permission_mode, \
                 reasoning_effort, draft, title, pinned, archived, unread, errored, \
                 created_at, updated_at, last_human_message_at, config_version \
                 FROM conversations WHERE id = ?1",
                params![id],
                conversation_from_row,
            )
            .optional()
            .expect("conversation query")
        })
    }

    /// The materialized conversations, newest human message first (pinned
    /// first within their bucket is the caller's render concern; this is
    /// the ordering key).
    pub fn list_conversations(&self) -> Vec<ConversationRow> {
        with(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, project_id, model_provider, model_id, permission_mode, \
                     reasoning_effort, draft, title, pinned, archived, unread, errored, \
                     created_at, updated_at, last_human_message_at, config_version \
                     FROM conversations \
                     WHERE last_human_message_at IS NOT NULL \
                     ORDER BY last_human_message_at DESC, id",
                )
                .expect("conversation list query");
            let rows = stmt
                .query_map([], conversation_from_row)
                .expect("conversation list rows");
            rows.filter_map(Result::ok).collect()
        })
    }

    /// The most recent never-materialized conversation for a project (or
    /// for project-less): the draft-refill candidate. `None` when every
    /// conversation under that scope actually happened.
    pub fn latest_draft(&self, project_path: Option<&str>) -> Option<ConversationRow> {
        let project_id = project_path.map(|path| {
            with(|conn| {
                conn.query_row(
                    "SELECT id FROM projects WHERE path = ?1",
                    params![path],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .expect("project lookup")
            })
        });
        with(|conn| {
            conn.query_row(
                "SELECT id, project_id, model_provider, model_id, permission_mode, \
                 reasoning_effort, draft, title, pinned, archived, unread, errored, \
                 created_at, updated_at, last_human_message_at, config_version \
                 FROM conversations \
                 WHERE last_human_message_at IS NULL AND \
                   project_id IS ?1 \
                 ORDER BY created_at DESC LIMIT 1",
                params![project_id],
                conversation_from_row,
            )
            .optional()
            .expect("draft query")
        })
    }
}

fn conversation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ConversationRow> {
    Ok(ConversationRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        model_provider: row.get(2)?,
        model_id: row.get(3)?,
        permission_mode: row.get(4)?,
        reasoning_effort: row.get(5)?,
        draft: row.get(6)?,
        title: row.get(7)?,
        pinned: row.get::<_, i64>(8)? != 0,
        archived: row.get::<_, i64>(9)? != 0,
        unread: row.get::<_, i64>(10)? != 0,
        errored: row.get::<_, i64>(11)? != 0,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
        last_human_message_at: row.get(14)?,
        config_version: row.get(15)?,
    })
}

/// Boot reconciliation: align `last_human_message_at` with the filesystem
/// fact (a journal file under the core root). A journal whose row never
/// stamped gets the stamp backfilled from its first user-role message; a
/// stamp whose journal vanished (the user deleted the file) is cleared —
/// the conversation returns to its draft state. Bounded: only the header
/// and first rows of an unstamped journal are read.
pub fn reconcile_materialization(core_root: &Path) {
    let Some(store) = ObjectStore::try_global() else {
        return;
    };
    let mut on_disk: std::collections::HashSet<String> = std::collections::HashSet::new();
    if let Ok(buckets) = std::fs::read_dir(core_root) {
        for bucket in buckets.flatten() {
            let Ok(sessions) = std::fs::read_dir(bucket.path()) else {
                continue;
            };
            for session in sessions.flatten() {
                if session.path().join("session.jsonl").is_file()
                    && let Some(id) = session.file_name().to_str()
                {
                    on_disk.insert(id.to_string());
                }
            }
        }
    }
    for row in store.list_all_conversations() {
        let materialized = row.last_human_message_at.is_some();
        match (materialized, on_disk.contains(&row.id)) {
            (false, true) => {
                // A journal exists but the stamp never landed: backfill it
                // from the journal's first user-role message.
                if let Some(at) = first_human_message_at(&find_journal(core_root, &row.id)) {
                    store.mark_human_message(&row.id, at);
                }
            }
            (true, false) => {
                // The journal is gone: the conversation never happened.
                with(|conn| {
                    let _ = conn.execute(
                        "UPDATE conversations SET last_human_message_at = NULL, updated_at = ?1 \
                         WHERE id = ?2",
                        params![now(), row.id],
                    );
                });
            }
            _ => {}
        }
    }
}

impl ObjectStore {
    /// Every conversation row regardless of materialization (reconciliation
    /// input; the listing APIs filter).
    fn list_all_conversations(&self) -> Vec<ConversationRow> {
        with(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, project_id, model_provider, model_id, permission_mode, \
                     reasoning_effort, draft, title, pinned, archived, unread, errored, \
                     created_at, updated_at, last_human_message_at, config_version \
                     FROM conversations",
                )
                .expect("all conversations query");
            let rows = stmt
                .query_map([], conversation_from_row)
                .expect("all conversations rows");
            rows.filter_map(Result::ok).collect()
        })
    }
}

fn find_journal(core_root: &Path, id: &str) -> PathBuf {
    if let Ok(buckets) = std::fs::read_dir(core_root) {
        for bucket in buckets.flatten() {
            let candidate = bucket.path().join(id).join("session.jsonl");
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::new()
}

/// The first user-role message timestamp in a journal (bounded read of the
/// leading rows).
fn first_human_message_at(journal: &Path) -> Option<i64> {
    use std::io::BufRead;
    let file = std::fs::File::open(journal).ok()?;
    for line in std::io::BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .take(2048)
    {
        let Ok(row) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if row.get("type").and_then(|t| t.as_str()) != Some("message") {
            continue;
        }
        let message = row.get("message")?;
        if message.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        // A user message the harness authored (author attribution present)
        // is not a human message.
        if message
            .get("ui")
            .and_then(|ui| ui.get("author"))
            .is_some_and(|a| !a.is_null())
        {
            continue;
        }
        let ts = row.get("timestamp").and_then(|t| t.as_str())?;
        return chrono::DateTime::parse_from_rfc3339(ts)
            .ok()
            .map(|t| t.timestamp());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn scratch() -> (tempfile::TempDir, std::sync::MutexGuard<'static, ()>) {
        let guard = test_lock();
        let dir = tempfile::tempdir().unwrap();
        set_path_for_test(Some(dir.path().join("objects.db")));
        init();
        (dir, guard)
    }

    #[test]
    fn registration_takes_the_head_and_is_idempotent() {
        let (_dir, _lock) = scratch();
        let store = ObjectStore;
        let a = store.register_project("/p/a");
        let b = store.register_project("/p/b");
        assert_ne!(a, b);
        // Same path: the same id, no duplicate row.
        assert_eq!(store.register_project("/p/a"), a);
        let order: Vec<String> = store
            .list_projects()
            .iter()
            .map(|p| p.path.clone())
            .collect();
        assert_eq!(
            order,
            vec!["/p/b".to_string(), "/p/a".to_string()],
            "newest registered first"
        );
    }

    #[test]
    fn alias_and_manual_order_round_trip() {
        let (_dir, _lock) = scratch();
        let store = ObjectStore;
        store.register_project("/p/a");
        store.register_project("/p/b");
        assert!(store.set_project_alias("/p/a", Some("主项目")));
        assert!(store.move_project("/p/a", Some("/p/b")));
        let projects = store.list_projects();
        assert_eq!(projects[0].path, "/p/a");
        assert_eq!(projects[0].alias.as_deref(), Some("主项目"));
    }

    #[test]
    fn conversation_lifecycle_and_config_versioning() {
        let (_dir, _lock) = scratch();
        let store = ObjectStore;
        assert!(store.create_conversation("c-1", Some("/p/proj")));
        assert!(!store.create_conversation("c-1", None), "idempotent create");

        store.set_draft("c-1", Some("half-typed thought"));
        let row = store.conversation("c-1").unwrap();
        assert_eq!(row.draft.as_deref(), Some("half-typed thought"));
        assert_eq!(row.config_version, 0, "drafts do not bump config");
        assert!(row.last_human_message_at.is_none(), "never happened yet");
        assert!(store.latest_draft(Some("/p/proj")).unwrap().id == "c-1");
        assert!(
            store.list_conversations().is_empty(),
            "unmaterialized rows never list"
        );

        store.set_model("c-1", Some("BigModel"), Some("glm-5.3"));
        store.set_permission_mode("c-1", Some("workspace-write"));
        let row = store.conversation("c-1").unwrap();
        assert_eq!(row.config_version, 2);
        assert_eq!(row.model_id.as_deref(), Some("glm-5.3"));
        assert_eq!(row.permission_mode.as_deref(), Some("workspace-write"));

        // The human speaks: materialization clears the draft and orders it.
        store.mark_human_message("c-1", 1_700_000_500);
        let row = store.conversation("c-1").unwrap();
        assert_eq!(row.last_human_message_at, Some(1_700_000_500));
        assert!(row.draft.is_none(), "the draft was sent");
        assert!(store.latest_draft(Some("/p/proj")).is_none());
        assert_eq!(store.list_conversations().len(), 1);
    }

    #[test]
    fn attach_and_detach_never_touch_cwd_free_rows() {
        let (_dir, _lock) = scratch();
        let store = ObjectStore;
        store.create_conversation("c-1", None);
        assert!(store.conversation("c-1").unwrap().project_id.is_none());
        store.attach("c-1", "/p/proj");
        let attached = store.conversation("c-1").unwrap();
        assert!(attached.project_id.is_some());
        assert!(attached.config_version >= 1);
        store.detach("c-1");
        assert!(store.conversation("c-1").unwrap().project_id.is_none());
    }

    #[test]
    fn reconciliation_repairs_both_directions() {
        let (dir, _lock) = scratch();
        let store = ObjectStore;
        store.register_project("/p/proj");

        // A journal on disk whose row never stamped: backfilled.
        let sid = "s-on-disk";
        let session_dir = dir.path().join("bucket").join(sid);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("session.jsonl"),
            concat!(
                "{\"type\":\"session\",\"version\":4,\"id\":\"s-on-disk\",\"cwd\":\"/p\",\"timestamp\":\"2026-10-21T00:00:00Z\"}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]},\"timestamp\":\"2026-10-21T00:00:01Z\"}\n"
            ),
        )
        .unwrap();
        store.create_conversation(sid, Some("/p/proj"));
        reconcile_materialization(dir.path());
        let row = store.conversation(sid).unwrap();
        assert_eq!(row.last_human_message_at, Some(1_792_540_801));

        // A stamped row whose journal vanished: returned to draft state.
        store.create_conversation("s-gone", Some("/p/proj"));
        store.mark_human_message("s-gone", 1_700_000_000);
        reconcile_materialization(dir.path());
        assert!(
            store
                .conversation("s-gone")
                .unwrap()
                .last_human_message_at
                .is_none()
        );
    }

    /// A config write addressed at a conversation the store has never seen
    /// materializes its row: the client mints the id and may land a pick
    /// before (or instead of) its explicit create — the row is the object's
    /// birth, and nothing may abort or vanish over the ordering.
    #[test]
    fn a_config_write_materializes_an_unknown_conversation() {
        let (_dir, _lock) = scratch();
        let store = ObjectStore;
        assert!(store.conversation("fresh").is_none());

        store.set_permission_mode("fresh", Some("read-only"));
        let row = store
            .conversation("fresh")
            .expect("the pick materialized the row");
        assert_eq!(row.permission_mode.as_deref(), Some("read-only"));
        assert_eq!(row.config_version, 1);
        assert!(row.project_id.is_none(), "materialized project-less");
        assert!(row.last_human_message_at.is_none(), "still a draft");

        // Attaching an unseen conversation lands its belonging the same way.
        store.attach("attached", "/p/proj");
        let row = store.conversation("attached").expect("attach materialized");
        assert!(row.project_id.is_some());
        assert_eq!(row.config_version, 1);
    }

    /// The engine's change-notice watermark contract: config writes bump
    /// the version, non-config writes do not.
    #[test]
    fn only_config_writes_bump_the_version() {
        let (_dir, _lock) = scratch();
        let store = ObjectStore;
        store.create_conversation("c-1", None);
        let before = store.conversation("c-1").unwrap().config_version;
        store.set_draft("c-1", Some("x"));
        store.set_pinned("c-1", true);
        store.set_archived("c-1", true);
        store.set_title("c-1", Some("t"));
        let after = store.conversation("c-1").unwrap();
        assert_eq!(
            after.config_version,
            before + 3,
            "pin/archive/title bump; draft does not"
        );
    }
}
