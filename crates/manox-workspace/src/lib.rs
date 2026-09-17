//! Durable Workspace domain (deepseek-harness workspace parity): directory
//! identity rows with an ordered, header-validated session account.
//!
//! A workspace is a stable uuid over an existing directory (`path` is the
//! realpath canon stamped at create, never rewritten). Sessions are
//! *accounted* to a workspace in manual order; membership is header-
//! validated — an accounted id is only returned when the thread's bound
//! project realpath-equals the row path — and every accepted mutation
//! durably prunes filtered candidates. The registry state carries the
//! display order, the global archive set (an archived session keeps its
//! account slot) and a two-write crash marker resolved at open.
//!
//! Storage is a crate-owned sqlite file (`<manox home>/workspaces.db`);
//! the cache is a fold shortcut nowhere — this db IS the authority.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// One durable workspace row projected for consumers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceView {
    pub workspace_id: String,
    pub path: String,
    pub title: String,
    /// Header-validated sessions in manually owned order.
    pub session_ids: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Reconnect-safe state-stream vocabulary (served over the wire by
/// manox-session-core's workspace namespace).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum WorkspaceEvent {
    Baseline {
        workspaces: Vec<WorkspaceView>,
        archived_session_ids: Vec<String>,
    },
    Upsert {
        workspace: WorkspaceView,
    },
    Remove {
        workspace_id: String,
    },
    Order {
        workspace_ids: Vec<String>,
    },
    Archived {
        archived_session_ids: Vec<String>,
    },
}

/// Workspace verb failures (wire-mapped by the serving namespace).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    NotFound(String),
    InvalidPath(String),
    SessionMismatch { session_id: String, path: String },
    Io(String),
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "workspace not found: {id}"),
            Self::InvalidPath(p) => write!(f, "cannot back a workspace: {p}"),
            Self::SessionMismatch { session_id, path } => {
                write!(f, "session {session_id} does not belong to {path}")
            }
            Self::Io(e) => write!(f, "workspace store io: {e}"),
        }
    }
}

impl std::error::Error for WorkspaceError {}

const FEED_CAPACITY: usize = 64;

/// The workspace registry: one sqlite file, one write chain (the internal
/// mutex), one change feed.
pub struct WorkspaceStore {
    path: PathBuf,
    /// The single write chain: verbs serialize here while each operation
    /// opens its own short-lived connection, so the registry holds no file
    /// descriptors between calls (fd pressure under parallel suites,
    /// review #805 follow-up).
    write: Mutex<()>,
    feed: broadcast::Sender<WorkspaceEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Row {
    id: String,
    path: String,
    title: String,
    session_ids: Vec<String>,
    created_at: String,
    updated_at: String,
}

impl WorkspaceStore {
    /// Open (creating on first use) the registry at the manox home and
    /// resolve any interrupted two-write mutation.
    pub fn open() -> Result<Self, WorkspaceError> {
        let path = manox_agent::paths::manox_config_dir()
            .map_err(|e| WorkspaceError::Io(e.to_string()))?
            .join("workspaces.db");
        Self::open_at(&path)
    }

    /// Open a registry at an explicit path (tests / foreign homes).
    pub fn open_at(path: &Path) -> Result<Self, WorkspaceError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| WorkspaceError::Io(e.to_string()))?;
        }
        let conn = Self::connect_at(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS workspaces (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                title TEXT NOT NULL,
                session_ids TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS registry_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                initialized INTEGER NOT NULL,
                workspace_ids TEXT NOT NULL,
                archived_session_ids TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS pending_mutation (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                op TEXT NOT NULL,
                workspace_id TEXT NOT NULL
            );",
        )
        .map_err(|e| WorkspaceError::Io(e.to_string()))?;
        let (feed, _) = broadcast::channel(FEED_CAPACITY);
        let store = Self {
            path: path.to_path_buf(),
            write: Mutex::new(()),
            feed,
        };
        store.recover_pending()?;
        Ok(store)
    }

    /// The change feed; subscribers pair it with [`Self::baseline_event`]
    /// for a reconnect-safe view.
    pub fn subscribe(&self) -> broadcast::Receiver<WorkspaceEvent> {
        self.feed.subscribe()
    }

    /// The current full state as one baseline event.
    pub fn baseline_event(&self) -> WorkspaceEvent {
        let (rows, archived) = self
            .with_db(|db| Ok((read_rows(db)?, read_state(db)?.archived_session_ids)))
            .unwrap_or_default();
        WorkspaceEvent::Baseline {
            workspaces: rows.into_iter().map(|row| self.view_of(row)).collect(),
            archived_session_ids: archived,
        }
    }

    // ── verbs ─────────────────────────────────────────────────────────────

    /// Adopt an existing directory; an already-adopted path answers the
    /// existing row (`created = false` semantics live in the caller's
    /// value pair).
    pub fn create(&self, path: &Path) -> Result<(WorkspaceView, bool), WorkspaceError> {
        let canon = canonical_dir(path)?;
        self.tx(|db| write_marker(db, "create", ""))?;
        let (view, created) = self.mutate(|db| {
            if let Some(row) = read_rows(db)?.into_iter().find(|r| r.path == canon) {
                return Ok((row, false));
            }
            let now = now_iso();
            let row = Row {
                id: uuid::Uuid::new_v4().to_string(),
                path: canon.clone(),
                title: title_of(Path::new(&canon)),
                session_ids: Vec::new(),
                created_at: now.clone(),
                updated_at: now,
            };
            db.execute(
                "INSERT INTO workspaces (id, path, title, session_ids, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    row.id,
                    row.path,
                    row.title,
                    serde_json::to_string(&row.session_ids).unwrap_or_default(),
                    row.created_at,
                    row.updated_at
                ],
            )
            .map_err(io)?;
            let mut state = read_state(db)?;
            state.workspace_ids.push(row.id.clone());
            write_state(db, &state)?;
            Ok((row, true))
        })?;
        self.tx(clear_marker)?;
        let view = Self::validated_view(view);
        if created {
            self.emit(WorkspaceEvent::Upsert {
                workspace: view.clone(),
            });
            self.emit_order();
        }
        Ok((view, created))
    }

    pub fn rename(&self, workspace_id: &str, title: &str) -> Result<WorkspaceView, WorkspaceError> {
        let view = self.mutate(|db| {
            let mut row = read_row(db, workspace_id)?.ok_or_else(|| not_found(workspace_id))?;
            row.title = title.to_string();
            row.updated_at = now_iso();
            update_row(db, &row)?;
            Ok(row)
        })?;
        let view = Self::validated_view(view);
        self.emit(WorkspaceEvent::Upsert {
            workspace: view.clone(),
        });
        Ok(view)
    }

    pub fn delete(&self, workspace_id: &str) -> Result<(), WorkspaceError> {
        self.tx(|db| write_marker(db, "delete", workspace_id))?;
        self.mutate(|db| {
            if read_row(db, workspace_id)?.is_none() {
                return Ok(());
            }
            db.execute(
                "DELETE FROM workspaces WHERE id = ?1",
                params![workspace_id],
            )
            .map_err(io)?;
            let mut state = read_state(db)?;
            state.workspace_ids.retain(|id| id != workspace_id);
            write_state(db, &state)?;
            Ok(())
        })?;
        self.tx(clear_marker)?;
        self.emit(WorkspaceEvent::Remove {
            workspace_id: workspace_id.to_string(),
        });
        self.emit_order();
        Ok(())
    }

    /// DOM-insertBefore-like display order move over workspace rows.
    pub fn insert_before(
        &self,
        workspace_id: &str,
        before: Option<&str>,
    ) -> Result<Vec<String>, WorkspaceError> {
        let order = self.mutate(|db| {
            let mut state = read_state(db)?;
            if !state.workspace_ids.iter().any(|id| id == workspace_id) {
                return Err(not_found(workspace_id));
            }
            move_id(&mut state.workspace_ids, workspace_id, before)?;
            write_state(db, &state)?;
            Ok(state.workspace_ids.clone())
        })?;
        self.emit(WorkspaceEvent::Order {
            workspace_ids: order.clone(),
        });
        Ok(order)
    }

    /// Account a session to a workspace; the session's bound project must
    /// realpath-equal the row path (header validation).
    pub fn attach_session(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<WorkspaceView, WorkspaceError> {
        let view = self.mutate(|db| {
            let mut row = read_row(db, workspace_id)?.ok_or_else(|| not_found(workspace_id))?;
            if !session_belongs(session_id, &row.path) {
                return Err(WorkspaceError::SessionMismatch {
                    session_id: session_id.to_string(),
                    path: row.path.clone(),
                });
            }
            if !row.session_ids.iter().any(|id| id == session_id) {
                row.session_ids.insert(0, session_id.to_string());
                row.updated_at = now_iso();
                update_row(db, &row)?;
            }
            prune_row(db, &mut row)?;
            Ok(row)
        })?;
        let view = self.view_of(view);
        self.emit(WorkspaceEvent::Upsert {
            workspace: view.clone(),
        });
        Ok(view)
    }

    /// DOM-insertBefore-like manual order move within one account.
    pub fn insert_session_before(
        &self,
        workspace_id: &str,
        session_id: &str,
        before: Option<&str>,
    ) -> Result<WorkspaceView, WorkspaceError> {
        let view = self.mutate(|db| {
            let mut row = read_row(db, workspace_id)?.ok_or_else(|| not_found(workspace_id))?;
            if !row.session_ids.iter().any(|id| id == session_id) {
                return Err(not_found(session_id));
            }
            move_id(&mut row.session_ids, session_id, before)?;
            row.updated_at = now_iso();
            update_row(db, &row)?;
            prune_row(db, &mut row)?;
            Ok(row)
        })?;
        let view = self.view_of(view);
        self.emit(WorkspaceEvent::Upsert {
            workspace: view.clone(),
        });
        Ok(view)
    }

    pub fn detach_session(
        &self,
        workspace_id: &str,
        session_id: &str,
    ) -> Result<WorkspaceView, WorkspaceError> {
        let view = self.mutate(|db| {
            let mut row = read_row(db, workspace_id)?.ok_or_else(|| not_found(workspace_id))?;
            row.session_ids.retain(|id| id != session_id);
            row.updated_at = now_iso();
            update_row(db, &row)?;
            prune_row(db, &mut row)?;
            Ok(row)
        })?;
        let view = self.view_of(view);
        self.emit(WorkspaceEvent::Upsert {
            workspace: view.clone(),
        });
        Ok(view)
    }

    /// Global archive set: an archived session keeps its account slot.
    pub fn archive_session(
        &self,
        session_id: &str,
        archived: bool,
    ) -> Result<Vec<String>, WorkspaceError> {
        let archived_ids = self.mutate(|db| {
            let mut state = read_state(db)?;
            let has = state.archived_session_ids.iter().any(|id| id == session_id);
            if archived && !has {
                state.archived_session_ids.push(session_id.to_string());
            }
            if !archived && has {
                state.archived_session_ids.retain(|id| id != session_id);
            }
            write_state(db, &state)?;
            Ok(state.archived_session_ids.clone())
        })?;
        self.emit(WorkspaceEvent::Archived {
            archived_session_ids: archived_ids.clone(),
        });
        Ok(archived_ids)
    }

    /// Registry rows in display order with header-validated accounts.
    pub fn list(&self) -> Result<Vec<WorkspaceView>, WorkspaceError> {
        let rows = self.with_db(|db| {
            let state = read_state(db)?;
            let mut rows = read_rows(db)?;
            rows.sort_by_key(|r| {
                state
                    .workspace_ids
                    .iter()
                    .position(|id| *id == r.id)
                    .unwrap_or(usize::MAX)
            });
            Ok(rows)
        })?;
        Ok(rows.into_iter().map(Self::validated_view).collect())
    }

    /// Live directory check; a missing directory never mutates the row.
    pub fn status(&self, workspace_id: &str) -> Result<&'static str, WorkspaceError> {
        let path = self
            .with_db(|db| Ok(read_row(db, workspace_id)?.map(|r| r.path)))?
            .ok_or_else(|| not_found(workspace_id))?;
        Ok(if Path::new(&path).is_dir() {
            "ok"
        } else {
            "missing-dir"
        })
    }

    /// First-boot adoption (NOT a schema migration): derive one row per
    /// registered project, accounts from the thread summaries' bound
    /// projects in store order; idempotent through the `initialized` flag.
    pub fn adopt_from_thread_store(&self) -> Result<usize, WorkspaceError> {
        let Some(store) = manox_agent::thread_store::try_global() else {
            return Ok(0);
        };
        let (projects, summaries) = store.read(|s| {
            (
                s.known_projects().to_vec(),
                s.summaries()
                    .iter()
                    .map(|sum| (sum.id.clone(), sum.project.clone()))
                    .collect::<Vec<_>>(),
            )
        });
        let initialized = self.with_db(|db| Ok(read_state(db)?.initialized))?;
        if initialized {
            return Ok(0);
        }
        let mut adopted = 0;
        for project in projects {
            let Ok(canon) = canonical_dir(Path::new(&project)) else {
                continue;
            };
            let (view, created) = self.create(Path::new(&project))?;
            if created {
                adopted += 1;
            }
            let workspace_id = view.workspace_id.clone();
            for (session_id, session_project) in &summaries {
                let Ok(session_canon) = canonical_dir(Path::new(session_project)) else {
                    continue;
                };
                if session_canon == canon {
                    let _ = self.attach_session(&workspace_id, session_id);
                }
            }
        }
        self.mutate(|db| {
            let mut state = read_state(db)?;
            state.initialized = true;
            write_state(db, &state)?;
            Ok(())
        })?;
        Ok(adopted)
    }

    // ── internals ─────────────────────────────────────────────────────────

    /// One short-lived connection. Multi-process home sharing (AGENTS.md):
    /// WAL plus a busy timeout so a concurrent writer retries instead of
    /// surfacing SQLITE_BUSY (review #805 [sugg] 7).
    fn connect_at(path: &Path) -> Result<Connection, WorkspaceError> {
        let conn = Connection::open(path).map_err(|e| WorkspaceError::Io(e.to_string()))?;
        if let Err(error) = conn.execute_batch("PRAGMA busy_timeout=5000;") {
            tracing::warn!(%error, "workspace busy_timeout failed; continuing without it");
        }
        // The journal-mode switch itself can contend with a concurrent
        // writer — the busy timeout above must be in place first, and the
        // switch gets one retry before falling back to delete-journal mode
        // (review #805 r2 [sugg] E).
        if let Err(first) = conn.execute_batch("PRAGMA journal_mode=WAL;") {
            tracing::warn!(%first, "workspace WAL switch contended; retrying once");
            if let Err(second) = conn.execute_batch("PRAGMA journal_mode=WAL;") {
                tracing::warn!(%second, "workspace WAL switch abandoned; delete-journal mode");
            }
        }
        Ok(conn)
    }

    fn connect(&self) -> Result<Connection, WorkspaceError> {
        Self::connect_at(&self.path)
    }

    fn with_db<R>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R, WorkspaceError>,
    ) -> Result<R, WorkspaceError> {
        let db = self.connect()?;
        f(&db)
    }

    /// One standalone transaction. The crash markers live in their OWN
    /// transactions around the data transaction — inside it they could
    /// never be observed (review #805 [sugg] 8).
    fn tx<R>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R, WorkspaceError>,
    ) -> Result<R, WorkspaceError> {
        let guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let db = self.connect()?;
        let out = db
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| WorkspaceError::Io(e.to_string()))
            .and_then(|()| match f(&db) {
                Ok(value) => db
                    .execute_batch("COMMIT")
                    .map_err(|e| WorkspaceError::Io(e.to_string()))
                    .map(|()| value),
                Err(error) => {
                    let _ = db.execute_batch("ROLLBACK");
                    Err(error)
                }
            });
        drop(guard);
        out
    }

    fn mutate<R>(
        &self,
        f: impl FnOnce(&Connection) -> Result<R, WorkspaceError>,
    ) -> Result<R, WorkspaceError> {
        let guard = self.write.lock().unwrap_or_else(|e| e.into_inner());
        let db = self.connect()?;
        let out = db
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| WorkspaceError::Io(e.to_string()))
            .and_then(|()| match f(&db) {
                Ok(value) => db
                    .execute_batch("COMMIT")
                    .map_err(|e| WorkspaceError::Io(e.to_string()))
                    .map(|()| value),
                Err(error) => {
                    let _ = db.execute_batch("ROLLBACK");
                    Err(error)
                }
            });
        drop(guard);
        out
    }

    /// The read-path view: header-validate the account in memory only —
    /// durable pruning belongs to the mutative verbs (`prune_row` inside
    /// their transactions), so `list()` never writes (review #805 r2
    /// [issue] C).
    fn validated_view(row: Row) -> WorkspaceView {
        let mut row = row;
        row.session_ids.retain(|id| session_belongs(id, &row.path));
        Self::view_of_row(row)
    }

    fn view_of_row(row: Row) -> WorkspaceView {
        WorkspaceView {
            workspace_id: row.id,
            path: row.path,
            title: row.title,
            session_ids: row.session_ids,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }

    fn view_of(&self, row: Row) -> WorkspaceView {
        WorkspaceView {
            workspace_id: row.id,
            path: row.path,
            title: row.title,
            session_ids: row.session_ids,
            created_at: row.created_at,
            updated_at: row.updated_at,
        }
    }

    fn emit(&self, event: WorkspaceEvent) {
        let _ = self.feed.send(event);
    }

    fn emit_order(&self) {
        if let Ok(order) = self.with_db(|db| Ok(read_state(db)?.workspace_ids.clone())) {
            self.emit(WorkspaceEvent::Order {
                workspace_ids: order,
            });
        }
    }

    /// Two-write crash recovery: the marker persists before the record /
    /// order pair can diverge, so open can finish or roll back an
    /// interrupted mutation instead of mistaking it for corruption.
    fn recover_pending(&self) -> Result<(), WorkspaceError> {
        self.mutate(|db| {
            let Some((op, workspace_id)) = db
                .query_row(
                    "SELECT op, workspace_id FROM pending_mutation WHERE id = 1",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(io)?
            else {
                return Ok(());
            };
            let present = read_row(db, &workspace_id)?.is_some();
            match (op.as_str(), present) {
                // Interrupted before the row landed (or completed): the
                // marker alone never resurrects a row.
                ("create", _) => {}
                // Interrupted between marker and delete: finish the delete.
                ("delete", true) => {
                    db.execute(
                        "DELETE FROM workspaces WHERE id = ?1",
                        params![workspace_id],
                    )
                    .map_err(io)?;
                    let mut state = read_state(db)?;
                    state.workspace_ids.retain(|id| id != &workspace_id);
                    write_state(db, &state)?;
                }
                ("delete", false) => {}
                _ => {}
            }
            clear_marker(db)?;
            Ok(())
        })
    }
}

// ── free helpers ──────────────────────────────────────────────────────────

fn io(error: rusqlite::Error) -> WorkspaceError {
    WorkspaceError::Io(error.to_string())
}

fn not_found(id: &str) -> WorkspaceError {
    WorkspaceError::NotFound(id.to_string())
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn title_of(path: &Path) -> String {
    match path.file_name().and_then(|s| s.to_str()) {
        Some(name) => name.to_string(),
        None => path.to_string_lossy().into_owned(),
    }
}

fn canonical_dir(path: &Path) -> Result<String, WorkspaceError> {
    let canon = std::fs::canonicalize(path)
        .map_err(|_| WorkspaceError::InvalidPath(path.to_string_lossy().into_owned()))?;
    if !canon.is_dir() {
        return Err(WorkspaceError::InvalidPath(
            path.to_string_lossy().into_owned(),
        ));
    }
    Ok(canon.to_string_lossy().into_owned())
}

/// Header validation: the thread's bound project must realpath-equal the
/// workspace path (a missing thread store fails closed).
fn session_belongs(session_id: &str, workspace_path: &str) -> bool {
    let Some(store) = manox_agent::thread_store::try_global() else {
        return false;
    };
    // Summary first (pure memory — materialized sessions, the common
    // case); the sidecar read only covers sessions whose journal has not
    // materialized and are therefore absent from the reconciled mirror
    // (review #805 r2 [issue] C: keep I/O off the locked hot path).
    let project = store.read(|s| {
        s.summary_by_id(session_id)
            .map(|sum| sum.project.clone())
            .or_else(|| s.sidecar_project(session_id))
    });
    match project {
        Some(project) if !project.is_empty() => {
            canonical_dir(Path::new(&project)).is_ok_and(|canon| canon == workspace_path)
        }
        _ => false,
    }
}

fn move_id(ids: &mut Vec<String>, id: &str, before: Option<&str>) -> Result<(), WorkspaceError> {
    let Some(from) = ids.iter().position(|x| x == id) else {
        return Err(not_found(id));
    };
    ids.remove(from);
    let at = match before {
        Some(anchor) => ids
            .iter()
            .position(|x| x == anchor)
            .ok_or_else(|| not_found(anchor))?,
        None => ids.len(),
    };
    ids.insert(at, id.to_string());
    Ok(())
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct RegistryState {
    initialized: bool,
    workspace_ids: Vec<String>,
    archived_session_ids: Vec<String>,
}

fn read_state(db: &Connection) -> Result<RegistryState, WorkspaceError> {
    let found = db
        .query_row(
            "SELECT initialized, workspace_ids, archived_session_ids FROM registry_state WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(io)?;
    Ok(match found {
        Some((initialized, order, archived)) => RegistryState {
            initialized: initialized != 0,
            workspace_ids: serde_json::from_str(&order).unwrap_or_default(),
            archived_session_ids: serde_json::from_str(&archived).unwrap_or_default(),
        },
        None => {
            let state = RegistryState::default();
            db.execute(
                "INSERT INTO registry_state (id, initialized, workspace_ids, archived_session_ids)
                 VALUES (1, 0, '[]', '[]')",
                [],
            )
            .map_err(io)?;
            state
        }
    })
}

fn write_state(db: &Connection, state: &RegistryState) -> Result<(), WorkspaceError> {
    db.execute(
        "INSERT INTO registry_state (id, initialized, workspace_ids, archived_session_ids)
         VALUES (1, ?1, ?2, ?3)
         ON CONFLICT(id) DO UPDATE SET
           initialized = excluded.initialized,
           workspace_ids = excluded.workspace_ids,
           archived_session_ids = excluded.archived_session_ids",
        params![
            state.initialized as i64,
            serde_json::to_string(&state.workspace_ids).unwrap_or_default(),
            serde_json::to_string(&state.archived_session_ids).unwrap_or_default()
        ],
    )
    .map_err(io)?;
    Ok(())
}

fn write_marker(db: &Connection, op: &str, workspace_id: &str) -> Result<(), WorkspaceError> {
    db.execute(
        "INSERT INTO pending_mutation (id, op, workspace_id) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET op = excluded.op, workspace_id = excluded.workspace_id",
        params![op, workspace_id],
    )
    .map_err(io)?;
    Ok(())
}

fn clear_marker(db: &Connection) -> Result<(), WorkspaceError> {
    db.execute("DELETE FROM pending_mutation WHERE id = 1", [])
        .map_err(io)?;
    Ok(())
}

fn read_rows(db: &Connection) -> Result<Vec<Row>, WorkspaceError> {
    let mut stmt = db
        .prepare("SELECT id, path, title, session_ids, created_at, updated_at FROM workspaces")
        .map_err(io)?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(io)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(io)?;
    Ok(rows
        .into_iter()
        .map(
            |(id, path, title, session_ids, created_at, updated_at)| Row {
                id,
                path,
                title,
                session_ids: serde_json::from_str(&session_ids).unwrap_or_default(),
                created_at,
                updated_at,
            },
        )
        .collect())
}

fn read_row(db: &Connection, id: &str) -> Result<Option<Row>, WorkspaceError> {
    Ok(read_rows(db)?.into_iter().find(|r| r.id == id))
}

fn update_row(db: &Connection, row: &Row) -> Result<(), WorkspaceError> {
    db.execute(
        "UPDATE workspaces SET path = ?2, title = ?3, session_ids = ?4, updated_at = ?5 WHERE id = ?1",
        params![
            row.id,
            row.path,
            row.title,
            serde_json::to_string(&row.session_ids).unwrap_or_default(),
            row.updated_at
        ],
    )
    .map_err(io)?;
    Ok(())
}

fn prune_row(db: &Connection, row: &mut Row) -> Result<(), WorkspaceError> {
    let before = row.session_ids.len();
    row.session_ids.retain(|id| session_belongs(id, &row.path));
    if row.session_ids.len() != before {
        update_row(db, row)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The store/runtime globals are process-singletons: serialize tests.
    fn test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Materialize a session file plus its sidecar carrying the bound
    /// project, then rescan: the store derives the summary from disk, so
    /// validation survives reconciles exactly like production (the
    /// sidecar alone feeds the unmaterialized fallback).
    fn seed_sidecar(id: &str, project: &str) {
        use manox_harness::session::jsonl::{JsonlSessionMetadata, JsonlSessionStorage};
        let sessions = manox_agent::paths::manox_config_dir()
            .unwrap()
            .join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join(format!("{id}.jsonl"));
        let handle = manox_agent::runtime::handle();
        if !path.exists() {
            handle.block_on(async {
                JsonlSessionStorage::create(
                    &path,
                    JsonlSessionMetadata {
                        id: id.to_string(),
                        cwd: project.to_string(),
                        created_at: chrono::Utc::now(),
                        parent_session_path: None,
                        metadata: None,
                    },
                )
                .await
                .expect("seed session file");
            });
        }
        std::fs::write(
            sessions.join(format!("{id}.meta.json")),
            format!(
                "{{\"project\":{}}}",
                serde_json::to_string(project).unwrap()
            ),
        )
        .unwrap();
        let store = manox_agent::thread_store::global();
        store.with_mut(|s| s.note_session_path(id, &path));
        handle.block_on(store.refresh_now());
    }

    /// One hermetic registry + thread store per test.
    fn rig() -> (tempfile::TempDir, WorkspaceStore) {
        manox_agent::runtime::hermetic_home_for_test();
        manox_agent::runtime::init();
        manox_agent::thread_store::drop_global_for_test();
        manox_agent::thread_store::init();
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::open_at(&dir.path().join("workspaces.db")).unwrap();
        (dir, store)
    }

    fn seed_thread(project: &str, id: &str) {
        manox_agent::thread_store::global().with_mut(|s| {
            s.insert_summary_for_test(id, None);
            s.set_project_for_test(id, project);
        });
    }

    #[test]
    fn create_is_idempotent_per_path_and_orders_registry() {
        let _g = test_lock();
        let (_dir, store) = rig();
        let target = tempfile::tempdir().unwrap();
        let (first, created) = store.create(target.path()).unwrap();
        assert!(created);
        let (second, created_again) = store.create(target.path()).unwrap();
        assert!(!created_again);
        assert_eq!(first.workspace_id, second.workspace_id);
        let other = tempfile::tempdir().unwrap();
        store.create(other.path()).unwrap();
        let order = store.list().unwrap();
        assert_eq!(order.len(), 2);
        assert_eq!(order[0].workspace_id, first.workspace_id);
    }

    #[test]
    fn attach_requires_header_validation_and_prunes_stale_accounts() {
        let _g = test_lock();
        let (_dir, store) = rig();
        let target = tempfile::tempdir().unwrap();
        let (view, _) = store.create(target.path()).unwrap();
        seed_thread(&target.path().to_string_lossy(), "s-in");
        seed_sidecar("s-in", &target.path().to_string_lossy());
        seed_thread("/nowhere-else", "s-out");
        seed_sidecar("s-out", "/nowhere-else");
        let attached = store.attach_session(&view.workspace_id, "s-in");
        assert!(attached.is_ok());
        let rejected = store.attach_session(&view.workspace_id, "s-out");
        assert!(matches!(
            rejected,
            Err(WorkspaceError::SessionMismatch { .. })
        ));
        // A project re-bind elsewhere prunes the account on next read.
        seed_thread("/nowhere-else", "s-in");
        seed_sidecar("s-in", "/nowhere-else");
        let listed = store.list().unwrap();
        assert!(listed[0].session_ids.is_empty());
    }

    #[test]
    fn manual_order_moves_are_insert_before_semantics() {
        let _g = test_lock();
        let (_dir, store) = rig();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (va, _) = store.create(a.path()).unwrap();
        let (vb, _) = store.create(b.path()).unwrap();
        seed_thread(&a.path().to_string_lossy(), "s-a");
        seed_sidecar("s-a", &a.path().to_string_lossy());
        store.attach_session(&va.workspace_id, "s-a").unwrap();
        let moved = store
            .insert_session_before(&va.workspace_id, "s-a", None)
            .unwrap();
        assert_eq!(moved.session_ids, vec!["s-a".to_string()]);
        let registry = store
            .insert_before(&vb.workspace_id, Some(&va.workspace_id))
            .unwrap();
        assert_eq!(registry[0], vb.workspace_id);
    }

    #[test]
    fn archive_keeps_the_account_slot() {
        let _g = test_lock();
        let (_dir, store) = rig();
        let target = tempfile::tempdir().unwrap();
        let (view, _) = store.create(target.path()).unwrap();
        seed_thread(&target.path().to_string_lossy(), "s-1");
        seed_sidecar("s-1", &target.path().to_string_lossy());
        store.attach_session(&view.workspace_id, "s-1").unwrap();
        let archived = store.archive_session("s-1", true).unwrap();
        assert_eq!(archived, vec!["s-1".to_string()]);
        let listed = store.list().unwrap();
        assert_eq!(listed[0].session_ids, vec!["s-1".to_string()]);
    }

    #[test]
    fn pending_delete_marker_completes_on_reopen() {
        let _g = test_lock();
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("workspaces.db");
        manox_agent::runtime::hermetic_home_for_test();
        manox_agent::runtime::init();
        manox_agent::thread_store::drop_global_for_test();
        manox_agent::thread_store::init();
        let store = WorkspaceStore::open_at(&db_path).unwrap();
        let target = tempfile::tempdir().unwrap();
        let (view, _) = store.create(target.path()).unwrap();
        drop(store);
        // Simulate a crash between the marker and the delete.
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO pending_mutation (id, op, workspace_id) VALUES (1, 'delete', ?1)
             ON CONFLICT(id) DO UPDATE SET op = excluded.op, workspace_id = excluded.workspace_id",
            params![view.workspace_id],
        )
        .unwrap();
        drop(conn);
        let reopened = WorkspaceStore::open_at(&db_path).unwrap();
        assert!(reopened.list().unwrap().is_empty());
        assert!(matches!(
            reopened.status(&view.workspace_id),
            Err(WorkspaceError::NotFound(_))
        ));
    }

    #[test]
    fn adoption_derives_rows_once() {
        let _g = test_lock();
        manox_agent::runtime::hermetic_home_for_test();
        manox_agent::runtime::init();
        manox_agent::thread_store::drop_global_for_test();
        manox_agent::thread_store::init();
        let dir = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        manox_agent::thread_store::global().with_mut(|s| {
            s.register_project(project.path().to_string_lossy().into_owned());
            s.insert_summary_for_test("s-1", None);
            s.set_project_for_test("s-1", &project.path().to_string_lossy());
        });
        seed_sidecar("s-1", &project.path().to_string_lossy());
        let store = WorkspaceStore::open_at(&dir.path().join("workspaces.db")).unwrap();
        let adopted = store.adopt_from_thread_store().unwrap();
        assert_eq!(adopted, 1);
        let listed = store.list().unwrap();
        assert_eq!(listed[0].session_ids, vec!["s-1".to_string()]);
        // Idempotent: a second adoption pass changes nothing.
        assert_eq!(store.adopt_from_thread_store().unwrap(), 0);
        assert_eq!(store.list().unwrap().len(), 1);
    }
}
