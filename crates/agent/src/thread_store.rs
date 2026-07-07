//! Process-global Thread list state.
//!
//! Holds an `Arc<ThreadsDatabase>` plus the current summary list, as a gpui
//! `Entity` + `EventEmitter`. The sidebar subscribes to `SummariesUpdated` to
//! refresh its list. `save_thread` persists asynchronously on turn end or user
//! message submit and then refreshes.

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
    /// The running thread changed (a turn started or ended). Carries the
    /// thread id that is now running, or `None` when no thread is running.
    /// Distinct from `SummariesUpdated` so the sidebar can re-render a single
    /// row without re-querying the db.
    RunningChanged(Option<String>),
}

pub struct ThreadStore {
    db: Arc<ThreadsDatabase>,
    summaries: Vec<ThreadSummary>,
    /// The thread id currently running a turn, or `None` when idle. Set by the
    /// workspace on `ThreadEvent::TurnStarted` and cleared on terminal
    /// `Stop`/`Error`. Drives the sidebar's running indicator on the matching
    /// thread item.
    running: Option<String>,
}

impl EventEmitter<ThreadStoreEvent> for ThreadStore {}

static GLOBAL: OnceLock<Entity<ThreadStore>> = OnceLock::new();

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
        running: None,
    });
    let _ = GLOBAL.set(entity);
}

/// Returns the global `ThreadStore` `Entity`. Panics if `init` was not called.
pub fn global() -> Entity<ThreadStore> {
    GLOBAL
        .get()
        .expect("ThreadStore 未初始化，请先调用 agent::init")
        .clone()
}

impl ThreadStore {
    pub fn summaries(&self) -> &[ThreadSummary] {
        &self.summaries
    }

    /// The thread id currently running a turn, or `None` when idle.
    pub fn running(&self) -> Option<&str> {
        self.running.as_deref()
    }

    /// Set the running thread id (or clear it). Emits `RunningChanged` so the
    /// sidebar re-renders the affected rows. Called by the workspace on
    /// `ThreadEvent::TurnStarted` / terminal `Stop` / `Error`.
    pub fn set_running(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        if self.running == id {
            return;
        }
        self.running = id;
        cx.emit(ThreadStoreEvent::RunningChanged(self.running.clone()));
        cx.notify();
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

    /// Load and restore a `Thread` by id (model resolved from the registry by id; `None` if not found).
    pub fn load_thread(&self, id: &str, cx: &mut App) -> Option<Entity<Thread>> {
        let rec: ThreadRecord = self.db.load(id).ok()??;
        let model: Option<AnyLanguageModel> = registry::global().get_model(&rec.model_id);
        Some(Thread::restore(rec, model, cx))
    }

    /// Delete by id, then refresh. Fires `SessionEnd` (fail-open) so plugins
    /// can tear down per-session state — a deleted thread's session is over.
    /// The thread's cwd is loaded first so handlers get the real project dir as
    /// `CLAUDE_PROJECT_DIR` (the record is gone after `delete`, so load before).
    pub fn delete_thread(&mut self, id: &str, cx: &mut Context<Self>) {
        let cwd = self.db.load(id).ok().flatten().map(|r| r.cwd);
        crate::hook::fire(
            crate::hook::HookEvent::SessionEnd,
            cwd.as_deref(),
            serde_json::json!({"thread_id": id}),
        );
        if let Err(e) = self.db.delete(id) {
            tracing::warn!(error = %e, "删除 thread 失败");
        }
        self.refresh(cx);
    }

    /// Create a fresh empty `Thread` (used by the sidebar "new conversation" button).
    pub fn new_thread(&self, cwd: PathBuf, cx: &mut App) -> Entity<Thread> {
        Thread::new(ThreadId(uuid::Uuid::new_v4().to_string()), cwd, cx)
    }

    /// Rename a thread (sets the user title override). `name == None` clears it.
    /// Runs the write off the UI thread, then refreshes so the sidebar updates.
    pub fn rename_thread(&self, id: &str, name: Option<String>, cx: &mut Context<Self>) {
        let db = self.db.clone();
        let id = id.to_string();
        cx.spawn(async move |this, cx| {
            let res = cx
                .background_executor()
                .spawn(async move { db.rename(&id, name.as_deref()) })
                .await;
            if let Err(e) = res {
                tracing::warn!(error = %e, "rename thread 失败");
            }
            this.update(cx, |s, cx| {
                s.refresh(cx);
            })
            .ok();
        })
        .detach();
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
