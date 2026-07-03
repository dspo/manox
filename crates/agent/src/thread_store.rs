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
}

pub struct ThreadStore {
    db: Arc<ThreadsDatabase>,
    summaries: Vec<ThreadSummary>,
}

impl EventEmitter<ThreadStoreEvent> for ThreadStore {}

static GLOBAL: OnceLock<Entity<ThreadStore>> = OnceLock::new();

/// Open the db, load the summary list, and register the global `Entity`. Call at App startup.
pub fn init(cx: &mut App) {
    let path = default_db_path().expect("解析 threads.db 路径失败");
    let db = ThreadsDatabase::open(&path)
        .unwrap_or_else(|e| panic!("打开 threads db 失败 ({}): {e}", path.display()));
    let summaries = db.list().unwrap_or_else(|e| {
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

    /// Re-read the db, refresh the summary list, and emit.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Ok(list) = self.db.list() {
            self.summaries = list;
        }
        cx.emit(ThreadStoreEvent::SummariesUpdated);
        cx.notify();
    }

    /// Load and restore a `Thread` by id (model resolved from the registry by id; `None` if not found).
    pub fn load_thread(&self, id: &str, cx: &mut App) -> Option<Entity<Thread>> {
        let rec: ThreadRecord = self.db.load(id).ok()??;
        let model: Option<AnyLanguageModel> = registry::global().get_model(&rec.model_id);
        Some(Thread::restore(
            ThreadId(rec.id),
            PathBuf::from(&rec.cwd),
            rec.messages,
            model,
            cx,
        ))
    }

    /// Delete by id, then refresh.
    pub fn delete_thread(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Err(e) = self.db.delete(id) {
            tracing::warn!(error = %e, "删除 thread 失败");
        }
        self.refresh(cx);
    }

    /// Create a fresh empty `Thread` (used by the sidebar "new conversation" button).
    pub fn new_thread(&self, cwd: PathBuf, cx: &mut App) -> Entity<Thread> {
        Thread::new(ThreadId(uuid::Uuid::new_v4().to_string()), cwd, cx)
    }
}

/// Persist a `Thread` snapshot (upsert) asynchronously, then refresh the global list.
/// Called by `Workspace` on user message submit / turn end. No-ops when there is no snapshot (no model).
pub fn save_thread(thread: Entity<Thread>, cx: &mut App) {
    let store = global();
    let db = store.read(cx).db.clone();
    let Some(rec) = thread.read(cx).snapshot() else {
        return;
    };
    cx.spawn(async move |cx: &mut AsyncApp| {
        let db = db;
        let res = cx
            .background_executor()
            .spawn(async move { db.upsert(&rec) })
            .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "保存 thread 失败");
        }
        store.update(cx, |s, cx| {
            s.refresh(cx);
        });
    })
    .detach();
}
