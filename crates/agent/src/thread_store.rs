//! Process-global Thread list state.
//!
//! Holds an `Arc<ThreadsDatabase>` plus the current summary list, as a gpui
//! `Entity` + `EventEmitter`. The sidebar subscribes to `SummariesUpdated` to
//! refresh its list. `save_thread` persists asynchronously on turn end or user
//! message submit and then refreshes.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use gpui::{App, AppContext as _, AsyncApp, Context, Entity, EventEmitter};

use crate::db::{ThreadSummary, ThreadsDatabase, default_db_path};
use crate::language_model::AnyLanguageModel;
use crate::provider::registry;
use crate::thread::{Thread, ThreadId};

/// Events emitted by `ThreadStore` to the sidebar.
#[derive(Debug, Clone)]
pub enum ThreadStoreEvent {
    /// The summary list changed (created / saved / deleted).
    SummariesUpdated,
    /// The set of running threads changed (a turn started or ended). The sidebar
    /// re-renders per-row running indicators by querying `is_running` for each
    /// summary id. Distinct from `SummariesUpdated` so the sidebar can re-render
    /// without re-querying the db.
    RunningChanged,
}

pub struct ThreadStore {
    db: Arc<ThreadsDatabase>,
    summaries: Vec<ThreadSummary>,
    /// Project paths registered in the `projects` table. The sidebar renders
    /// a folder for every path here, even when no active thread references it,
    /// so projects persist across archival of all their threads.
    known_projects: Vec<String>,
    /// Thread ids currently running a turn. Multiple threads can run concurrently;
    /// the sidebar shows a running indicator on every row whose id is in this set.
    running: HashSet<String>,
}

impl EventEmitter<ThreadStoreEvent> for ThreadStore {}

static GLOBAL: OnceLock<Entity<ThreadStore>> = OnceLock::new();

/// Test-only override of the process-global `ThreadStore`. `init_for_test`
/// stores an in-memory-db-backed entity here so persistence-bearing tests
/// don't touch the real `~/.config/cx/manox/threads.db`; `drop_for_test`
/// clears it so gpui's leaked-handle check at teardown doesn't trip on an
/// entity held alive by a process-global `OnceLock`.
#[cfg(any(test, feature = "test-support"))]
static TEST_OVERRIDE: std::sync::Mutex<Option<Entity<ThreadStore>>> = std::sync::Mutex::new(None);

/// Open the db, load the summary list, and register the global `Entity`. Call at App startup.
pub fn init(cx: &mut App) {
    let path = default_db_path().expect("Failed to resolve threads.db path");
    let db = ThreadsDatabase::open(&path)
        .unwrap_or_else(|e| panic!("Failed to open threads db ({}): {e}", path.display()));
    let summaries = db.list(false).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "Failed to load thread summaries, starting with empty list");
        Vec::new()
    });
    // Load known projects from the `projects` table. On first run (empty table),
    // seed from existing thread summaries so the sidebar immediately shows
    // projects the user already has threads for.
    let mut known_projects = db.list_projects().unwrap_or_default();
    if known_projects.is_empty() {
        let mut seen = HashSet::new();
        for s in &summaries {
            if !s.project.is_empty() && seen.insert(s.project.clone()) {
                known_projects.push(s.project.clone());
                let _ = db.register_project(&s.project);
            }
        }
    }
    tracing::info!(
        count = summaries.len(),
        projects = known_projects.len(),
        "ThreadStore initialized, loaded thread summaries"
    );
    let entity = cx.new(|_cx| ThreadStore {
        db: Arc::new(db),
        summaries,
        known_projects,
        running: HashSet::new(),
    });
    let _ = GLOBAL.set(entity);
}

/// Returns the global `ThreadStore` `Entity`. Panics if `init` was not called.
pub fn global() -> Entity<ThreadStore> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(entity) = TEST_OVERRIDE.lock().unwrap().clone() {
        return entity;
    }
    GLOBAL
        .get()
        .expect("ThreadStore not initialized; call agent::init first")
        .clone()
}

impl ThreadStore {
    pub fn summaries(&self) -> &[ThreadSummary] {
        &self.summaries
    }

    /// All registered project paths. The sidebar iterates this to render
    /// project folders, regardless of whether each has active threads.
    pub fn known_projects(&self) -> &[String] {
        &self.known_projects
    }

    /// Register a project path in both the in-memory list and the db.
    /// Idempotent: no-ops if the path is already known.
    pub fn register_project(&mut self, path: String, cx: &mut Context<Self>) {
        if path.is_empty() || self.known_projects.contains(&path) {
            return;
        }
        self.known_projects.push(path.clone());
        cx.emit(ThreadStoreEvent::SummariesUpdated);
        cx.notify();
        let db = self.db.clone();
        cx.spawn(async move |_, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { db.register_project(&path) })
                .await;
            if let Err(e) = res {
                tracing::warn!(error = %e, "Failed to register project");
            }
        })
        .detach();
    }

    /// Whether the given thread id is currently running a turn.
    pub fn is_running(&self, id: &str) -> bool {
        self.running.contains(id)
    }

    /// Mark a thread as running (turn started). Emits `RunningChanged` so the
    /// sidebar re-renders the affected row. Also clears the `errored` flag —
    /// a new turn means the previous terminal error is no longer relevant.
    pub fn mark_running(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.running.insert(id.to_string()) {
            cx.emit(ThreadStoreEvent::RunningChanged);
        }
        // Clear the error flag: a new turn supersedes any prior terminal error.
        if let Some(s) = self.summaries.iter_mut().find(|s| s.id == id)
            && s.errored
        {
            s.errored = false;
            cx.emit(ThreadStoreEvent::SummariesUpdated);
            cx.notify();
            let db = self.db.clone();
            let id = id.to_string();
            cx.spawn(async move |_, cx| {
                let res = cx
                    .background_executor()
                    .spawn(async move { db.set_errored(&id, false) })
                    .await;
                if let Err(e) = res {
                    tracing::warn!(error = %e, "Failed to clear thread errored");
                }
            })
            .detach();
        }
    }

    /// Mark a thread as idle (turn ended). Emits `RunningChanged` so the
    /// sidebar re-renders the affected row.
    pub fn mark_idle(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.running.remove(id) {
            cx.emit(ThreadStoreEvent::RunningChanged);
            cx.notify();
        }
    }

    /// Set the unread flag on a thread. Updates the in-memory summary
    /// immediately so the sidebar re-renders without a db round-trip, then
    /// persists fire-and-forget on the background executor. A no-op when the
    /// value is unchanged, so switching into an already-read thread does not
    /// trigger a redraw or a pointless write.
    pub fn set_unread(&mut self, id: &str, unread: bool, cx: &mut Context<Self>) {
        let Some(s) = self.summaries.iter_mut().find(|s| s.id == id) else {
            // Not in the active list (archived / not yet loaded): still persist
            // so the flag is correct if it surfaces later.
            let db = self.db.clone();
            let id = id.to_string();
            cx.spawn(async move |_, cx| {
                let res = cx
                    .background_executor()
                    .spawn(async move { db.set_unread(&id, unread) })
                    .await;
                if let Err(e) = res {
                    tracing::warn!(error = %e, "Failed to set thread unread");
                }
            })
            .detach();
            return;
        };
        if s.has_unread == unread {
            return;
        }
        s.has_unread = unread;
        cx.emit(ThreadStoreEvent::SummariesUpdated);
        cx.notify();
        let db = self.db.clone();
        let id = id.to_string();
        cx.spawn(async move |_, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { db.set_unread(&id, unread) })
                .await;
            if let Err(e) = res {
                tracing::warn!(error = %e, "Failed to set thread unread");
            }
        })
        .detach();
    }

    /// Set the errored flag on a thread. Symmetric to `set_unread`: updates the
    /// in-memory summary immediately, then persists fire-and-forget. A no-op
    /// when the value is unchanged.
    pub fn set_errored(&mut self, id: &str, errored: bool, cx: &mut Context<Self>) {
        let Some(s) = self.summaries.iter_mut().find(|s| s.id == id) else {
            let db = self.db.clone();
            let id = id.to_string();
            cx.spawn(async move |_, cx| {
                let res = cx
                    .background_executor()
                    .spawn(async move { db.set_errored(&id, errored) })
                    .await;
                if let Err(e) = res {
                    tracing::warn!(error = %e, "Failed to set thread errored");
                }
            })
            .detach();
            return;
        };
        if s.errored == errored {
            return;
        }
        s.errored = errored;
        cx.emit(ThreadStoreEvent::SummariesUpdated);
        cx.notify();
        let db = self.db.clone();
        let id = id.to_string();
        cx.spawn(async move |_, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { db.set_errored(&id, errored) })
                .await;
            if let Err(e) = res {
                tracing::warn!(error = %e, "Failed to set thread errored");
            }
        })
        .detach();
    }

    /// Re-read the db, refresh the summary list, and emit. The `db.list()`
    /// query runs off the UI thread so a large history (or a busy SQLite lock)
    /// can't stall the main thread on turn end / submit / delete.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let list = cx
                .background_executor()
                .spawn(async move { db.list(false) })
                .await;
            this.update(cx, |s, cx| {
                if let Ok(list) = list {
                    s.summaries = list;
                }
                cx.emit(ThreadStoreEvent::SummariesUpdated);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Fetch distinct recent project paths from the db on the background
    /// executor. Returns up to `limit` paths ordered by most recent activity.
    pub fn fetch_recent_projects(
        &self,
        limit: usize,
        cx: &mut Context<Self>,
    ) -> gpui::Task<Vec<String>> {
        let db = self.db.clone();
        cx.spawn(async move |_this, cx| {
            cx.background_executor()
                .spawn(async move { db.list_recent_projects(limit).unwrap_or_default() })
                .await
        })
    }

    /// Load and restore a `Thread` by id (model resolved from the registry by id; `None` if not found).
    pub fn load_thread(&self, id: &str, cx: &mut App) -> Option<Entity<Thread>> {
        let rec = match self.db.load(id) {
            Ok(Some(r)) => r,
            Ok(None) => {
                tracing::warn!(thread_id = id, "load_thread: thread not in db");
                return None;
            }
            Err(e) => {
                tracing::warn!(thread_id = id, error = ?e, "load_thread: db load failed");
                return None;
            }
        };
        let model: Option<AnyLanguageModel> = registry::global().get_model(&rec.model_id);
        let entity = Thread::restore(rec, model, cx);
        // Backfill the UI-note cache from its own append-only table. Best-effort:
        // a read failure leaves the thread without historical Error/Notice
        // cards but otherwise intact. This is the only place `ui_notes` is
        // populated; `restore` and `new` leave it empty.
        match self.db.list_ui_notes(id) {
            Ok(notes) => entity.update(cx, |t, _| t.set_ui_notes(notes)),
            Err(e) => {
                tracing::warn!(thread_id = id, error = ?e, "load_thread: ui_notes load failed")
            }
        }
        Some(entity)
    }

    /// Create a fresh empty `Thread` (used by the sidebar "new conversation" button).
    pub fn new_thread(&self, cwd: PathBuf, cx: &mut App) -> Entity<Thread> {
        Thread::new(ThreadId(uuid::Uuid::new_v4().to_string()), cwd, cx)
    }

    /// Archive (or unarchive) a thread. Refreshes so it leaves/enters the active list.
    pub fn archive_thread(&self, id: &str, archived: bool, cx: &mut Context<Self>) {
        let db = self.db.clone();
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { db.archive(&id, archived) })
                .await;
            if let Err(e) = res {
                tracing::warn!(error = %e, "Failed to archive thread");
            }
            this.update(cx, |s, cx| {
                s.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Toggle the pinned flag on a thread. Pinned threads float to the top of
    /// the sidebar list (SQL `ORDER BY pinned DESC, interacted_at DESC`).
    /// Refreshes so the sidebar re-sorts after the toggle.
    pub fn pin_thread(&self, id: &str, pinned: bool, cx: &mut Context<Self>) {
        let db = self.db.clone();
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { db.pin(&id, pinned) })
                .await;
            if let Err(e) = res {
                tracing::warn!(error = %e, "Failed to pin thread");
            }
            this.update(cx, |s, cx| {
                s.refresh(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Append a `model_change` event to the thread's event stream. Called by the
    /// workspace when `ThreadEvent::ModelChanged` fires (mid-conversation switch).
    pub fn record_model_change(
        &self,
        thread_id: &str,
        from: Option<&str>,
        to: &str,
        cx: &mut Context<Self>,
    ) {
        let data = serde_json::json!({ "from": from, "to": to });
        self.record_event(
            thread_id,
            crate::db::ThreadEventType::ModelChange,
            &data,
            cx,
        );
    }

    /// Append a typed event to the thread's event stream. Used for `compaction`
    /// (and any future non-model event the workspace needs on the timeline).
    /// Fire-and-forget on the background executor; a db failure warns and does
    /// not surface to the caller — event recording is best-effort.
    pub fn record_event(
        &self,
        thread_id: &str,
        event_type: crate::db::ThreadEventType,
        data: &serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let db = self.db.clone();
        let thread_id = thread_id.to_string();
        let data = data.clone();
        cx.spawn(async move |_, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { db.record_event(&thread_id, event_type, &data) })
                .await;
            if let Err(e) = res {
                tracing::warn!(error = %e, "Failed to record thread event");
            }
        })
        .detach();
    }

    /// Persist a UI annotation (`Error` / `Notice` card) to the
    /// `thread_ui_notes` table. Best-effort on the background executor; a db
    /// failure warns and does not surface — the live `ConversationState`
    /// already shows the item this turn, only the reload copy is at stake.
    /// `anchor_user_id` ties the note to the turn it belongs to (`None` for
    /// notes emitted before the first user message) so the rebuild can splice
    /// it back at the right spot.
    pub fn record_ui_note(
        &self,
        thread_id: &str,
        kind: crate::db::UiNoteKind,
        anchor_user_id: Option<&str>,
        data: &serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        let db = self.db.clone();
        let thread_id = thread_id.to_string();
        let anchor = anchor_user_id.map(str::to_owned);
        let data = data.clone();
        cx.spawn(async move |_, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { db.record_ui_note(&thread_id, kind, anchor.as_deref(), &data) })
                .await;
            if let Err(e) = res {
                tracing::warn!(error = %e, "Failed to record ui note");
            }
        })
        .detach();
    }
}

/// Persist a `Thread` snapshot (upsert) asynchronously, then refresh the global list.
/// Called by `Workspace` on user message submit / turn end. No-ops when there is no
/// snapshot (no model) or when the thread has had no real interaction (empty new
/// conversation screen — avoids creating phantom "(New Conversation)" sidebar entries).
/// `touch`: when true, `updated_at` is bumped to now (real user activity); when false,
/// the existing timestamp is preserved (e.g. saving state on thread switch without
/// implying the user interacted with it).
pub fn save_thread(thread: Entity<Thread>, touch: bool, cx: &mut App) {
    let store = global();
    if !thread.read(cx).has_interacted() {
        return;
    }
    let db = store.read(cx).db.clone();
    let Some(rec) = thread.read(cx).snapshot() else {
        return;
    };
    // Register the thread's project so the sidebar retains the folder even
    // when all threads in that project are archived.
    if !rec.project.is_empty() {
        let project = rec.project.clone();
        store.update(cx, |s, cx| s.register_project(project, cx));
    }
    cx.spawn(async move |cx: &mut AsyncApp| {
        let db = db;
        let res = cx
            .background_executor()
            .spawn(async move { db.upsert(&rec, touch) })
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "Failed to save thread");
        }
        if touch {
            store.update(cx, |s, cx| {
                s.refresh(cx);
            });
        }
    })
    .detach();
}

/// Test-only initializer that primes the process-global `ThreadStore` with a
/// caller-provided db (typically `:memory:`) so persistence-bearing tests
/// don't touch the real `~/.config/cx/manox/threads.db`. The override lives in
/// `TEST_OVERRIDE` (not `GLOBAL`) precisely so `drop_for_test` can release the
/// entity before teardown — a `OnceLock` can't be cleared, which would trip
/// gpui's leaked-handle check. Pair every call with `drop_for_test`.
#[cfg(any(test, feature = "test-support"))]
pub fn init_for_test(db: Arc<ThreadsDatabase>, cx: &mut App) {
    let entity = cx.new(|_cx| ThreadStore {
        db,
        summaries: Vec::new(),
        known_projects: Vec::new(),
        running: HashSet::new(),
    });
    *TEST_OVERRIDE.lock().unwrap() = Some(entity);
}

/// Release the test-only `ThreadStore` entity so its gpui handle is dropped
/// before `TestAppContext` tears down. Call this at the end of any test that
/// used `init_for_test` (a Drop guard is the robust pattern).
#[cfg(any(test, feature = "test-support"))]
pub fn drop_for_test() {
    *TEST_OVERRIDE.lock().unwrap() = None;
}
