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

use crate::db::{ThreadRecord, ThreadSummary, ThreadsDatabase, default_db_path};
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
    let path = default_db_path().expect("解析 threads.db 路径失败");
    let db = ThreadsDatabase::open(&path)
        .unwrap_or_else(|e| panic!("打开 threads db 失败 ({}): {e}", path.display()));
    let summaries = db.list(false).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "加载历史 threads 列表失败，以空列表启动");
        Vec::new()
    });
    tracing::info!(
        count = summaries.len(),
        "ThreadStore 初始化，加载历史 threads"
    );
    let entity = cx.new(|_cx| ThreadStore {
        db: Arc::new(db),
        summaries,
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
        .expect("ThreadStore 未初始化，请先调用 agent::init")
        .clone()
}

impl ThreadStore {
    pub fn summaries(&self) -> &[ThreadSummary] {
        &self.summaries
    }

    /// Whether the given thread id is currently running a turn.
    pub fn is_running(&self, id: &str) -> bool {
        self.running.contains(id)
    }

    /// Mark a thread as running (turn started). Emits `RunningChanged` so the
    /// sidebar re-renders the affected row.
    pub fn mark_running(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.running.insert(id.to_string()) {
            cx.emit(ThreadStoreEvent::RunningChanged);
            cx.notify();
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
        let rec: ThreadRecord = self.db.load(id).ok()??;
        let model: Option<AnyLanguageModel> = registry::global().get_model(&rec.model_id);
        Some(Thread::restore(rec, model, cx))
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
                tracing::warn!(error = %e, "archive thread 失败");
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
                tracing::warn!(error = %e, "pin thread 失败");
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
        let db = self.db.clone();
        let thread_id = thread_id.to_string();
        let data = serde_json::json!({ "from": from, "to": to });
        cx.spawn(async move |_, cx| {
            let res = cx
                .background_executor()
                .spawn(async move {
                    db.record_event(&thread_id, crate::db::ThreadEventType::ModelChange, &data)
                })
                .await;
            if let Err(e) = res {
                tracing::warn!(error = %e, "record model_change 失败");
            }
        })
        .detach();
    }
}

/// Persist a `Thread` snapshot (upsert) asynchronously, then refresh the global list.
/// Called by `Workspace` on user message submit / turn end. No-ops when there is no
/// snapshot (no model) or when the thread has had no real interaction (empty new
/// conversation screen — avoids creating phantom "(新对话)" sidebar entries).
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
    cx.spawn(async move |cx: &mut AsyncApp| {
        let db = db;
        let res = cx
            .background_executor()
            .spawn(async move { db.upsert(&rec, touch) })
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "保存 thread 失败");
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
