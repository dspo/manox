//! Process-global `TerminalStore` — mirrors `agent::thread_store`.
//!
//! Holds an `Arc<ThreadsDatabase>` plus the current session-summary list as a
//! gpui `Entity` + `EventEmitter`. `save_terminal` snapshots a `Terminal`'s
//! id/cwd/title on the UI thread and persists them off-thread, then refreshes
//! the summary list so the sidebar can list reopenable terminals.

use std::sync::{Arc, OnceLock};

use chrono::Utc;
use gpui::{App, AppContext as _, AsyncApp, Context, Entity, EventEmitter};

use agent::db::{TerminalSession, ThreadsDatabase, default_db_path};

use crate::Terminal;

/// Events emitted by `TerminalStore` to listeners (sidebar in stage 9).
#[derive(Debug, Clone)]
pub enum TerminalStoreEvent {
    /// The session summary list changed (created / saved / deleted).
    SummariesUpdated,
}

pub struct TerminalStore {
    db: Arc<ThreadsDatabase>,
    summaries: Vec<TerminalSession>,
}

impl EventEmitter<TerminalStoreEvent> for TerminalStore {}

static GLOBAL: OnceLock<Entity<TerminalStore>> = OnceLock::new();

/// Test-only override of the process-global `TerminalStore`. `init_for_test`
/// stores an in-memory-db-backed entity here so persistence-bearing tests
/// don't touch the real `~/.config/cx/manox/threads.db`; `drop_for_test`
/// clears it so gpui's leaked-handle check at teardown doesn't trip on an
/// entity held alive by a process-global `OnceLock`. Mirrors `thread_store`.
#[cfg(any(test, feature = "test-support"))]
static TEST_OVERRIDE: std::sync::Mutex<Option<Entity<TerminalStore>>> = std::sync::Mutex::new(None);

/// Open the db, load the session list, and register the global `Entity`. Call at App startup.
pub fn init(cx: &mut App) {
    let path = default_db_path().expect("resolve threads.db path");
    let db = ThreadsDatabase::open(&path)
        .unwrap_or_else(|e| panic!("open threads db failed ({}): {e}", path.display()));
    let summaries = db.list_terminal_sessions().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "load terminal sessions failed, starting empty");
        Vec::new()
    });
    tracing::info!(
        count = summaries.len(),
        "TerminalStore initialized, loaded terminal sessions"
    );
    let entity = cx.new(|_cx| TerminalStore {
        db: Arc::new(db),
        summaries,
    });
    let _ = GLOBAL.set(entity);
}

/// Returns the global `TerminalStore` `Entity`. Panics if `init` was not called.
pub fn global() -> Entity<TerminalStore> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(entity) = TEST_OVERRIDE.lock().unwrap().clone() {
        return entity;
    }
    GLOBAL
        .get()
        .expect("TerminalStore not initialized, call terminal::init first")
        .clone()
}

/// Test-only initializer that primes the process-global `TerminalStore` with a
/// caller-provided db (typically `:memory:`) so persistence-bearing tests
/// don't touch the real `~/.config/cx/manox/threads.db`. The override lives in
/// `TEST_OVERRIDE` (not `GLOBAL`) precisely so `drop_for_test` can release the
/// entity before teardown — a `OnceLock` can't be cleared, which would trip
/// gpui's leaked-handle check. Pair every call with `drop_for_test`.
#[cfg(any(test, feature = "test-support"))]
pub fn init_for_test(db: Arc<ThreadsDatabase>, cx: &mut App) {
    let summaries = db.list_terminal_sessions().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "load terminal sessions failed, starting empty");
        Vec::new()
    });
    let entity = cx.new(|_cx| TerminalStore { db, summaries });
    *TEST_OVERRIDE.lock().unwrap() = Some(entity);
}

/// Release the test-only `TerminalStore` entity so its gpui handle is dropped
/// before `TestAppContext` tears down. Call this at the end of any test that
/// used `init_for_test` (a Drop guard is the robust pattern).
#[cfg(any(test, feature = "test-support"))]
pub fn drop_for_test() {
    *TEST_OVERRIDE.lock().unwrap() = None;
}

impl TerminalStore {
    pub fn summaries(&self) -> &[TerminalSession] {
        &self.summaries
    }

    /// Direct db lookup (synchronous) — used when reopening a specific tab.
    pub fn load_session(&self, id: &str) -> Option<TerminalSession> {
        self.db.load_terminal_session(id).ok().flatten()
    }

    /// Re-read the db, refresh the summary list, and emit. Runs the query
    /// off-thread so a busy SQLite lock can't stall the UI.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let list = cx
                .background_executor()
                .spawn(async move { db.list_terminal_sessions() })
                .await;
            this.update(cx, |s, cx| {
                if let Ok(list) = list {
                    s.summaries = list;
                }
                cx.emit(TerminalStoreEvent::SummariesUpdated);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Persist a terminal's session metadata (id/cwd/title) off-thread, then
    /// refresh the summary list. Scrollback is never persisted. `created_at`
    /// is preserved across re-saves; `updated_at` is bumped to now.
    pub fn save_terminal(&self, terminal: &Entity<Terminal>, cx: &mut Context<Self>) {
        // Snapshot on the UI thread — the Entity can only be read here.
        let (id, cwd, title) = terminal.read_with(cx, |t, _| {
            (
                t.id.clone(),
                t.cwd.to_string_lossy().to_string(),
                t.title.clone(),
            )
        });
        let db = self.db.clone();
        let now = Utc::now().timestamp();
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            // Preserve the original created_at if this session already exists.
            let created_at = db
                .load_terminal_session(&id)
                .ok()
                .flatten()
                .map(|s| s.created_at)
                .unwrap_or(now);
            let session = TerminalSession {
                id,
                cwd,
                env: Vec::new(),
                title,
                created_at,
                updated_at: now,
            };
            if let Err(e) = db.upsert_terminal_session(&session) {
                tracing::warn!(error = %e, "save terminal session failed");
            }
            this.update(cx, |s, cx| s.refresh(cx)).ok();
        })
        .detach();
    }

    /// Delete a session row and refresh.
    pub fn delete_session(&self, id: &str, cx: &mut Context<Self>) {
        let db = self.db.clone();
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            if let Err(e) = db.delete_terminal_session(&id) {
                tracing::warn!(error = %e, "delete terminal session failed");
            }
            this.update(cx, |s, cx| s.refresh(cx)).ok();
        })
        .detach();
    }
}
