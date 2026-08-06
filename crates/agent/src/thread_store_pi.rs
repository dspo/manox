// The pi-backed `ThreadStore` facade (built with `feature = "harness-pi"`).
//
// The sidebar's session list comes from the pi session repository (jsonl)
// plus a per-session UI-metadata sidecar (`pi_extensions::session_meta`).
// The pi transcript persists itself, so `save_thread` is a no-op and the
// manox SQLite timeline/note records are not produced.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::OnceLock;

use gpui::{App, AppContext as _, Context, Entity, EventEmitter, WeakEntity};

use crate::db::ThreadSummary;
use crate::thread::{Thread, ThreadId};

/// Events emitted by `ThreadStore` to the sidebar.
#[derive(Debug, Clone)]
pub enum ThreadStoreEvent {
    /// The summary list changed (created / saved / deleted).
    SummariesUpdated,
    /// The set of running threads changed.
    RunningChanged,
}

pub struct ThreadStore {
    summaries: Vec<ThreadSummary>,
    /// Session file path per summary id, for sidecar writes and reopen.
    session_paths: HashMap<String, PathBuf>,
    known_projects: Vec<String>,
    /// Host db handle persisting `known_projects` (shared threads.db,
    /// `projects` table only — thread rows remain the manox store's domain).
    db: std::sync::Arc<crate::db::ThreadsDatabase>,
    running: HashSet<String>,
    /// Canonical entity lookup without retaining idle threads indefinitely.
    live_threads: HashMap<String, WeakEntity<Thread>>,
    sessions_dir: PathBuf,
}

impl EventEmitter<ThreadStoreEvent> for ThreadStore {}

static GLOBAL: OnceLock<Entity<ThreadStore>> = OnceLock::new();

#[cfg(any(test, feature = "test-support"))]
static TEST_OVERRIDE: std::sync::Mutex<Option<Entity<ThreadStore>>> = std::sync::Mutex::new(None);

/// Resolve the pi session directory under the manox config dir.
pub(crate) fn sessions_dir() -> PathBuf {
    crate::paths::manox_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("pi-sessions")
}

/// Open the session directory, seed the summary list, and register the global
/// `Entity`. Call at App startup.
pub fn init(cx: &mut App) {
    let dir = sessions_dir();
    let db_path = crate::db::default_db_path().expect("Failed to resolve threads.db path");
    let db = std::sync::Arc::new(
        crate::db::ThreadsDatabase::open(&db_path)
            .unwrap_or_else(|e| panic!("Failed to open threads db ({}): {e}", db_path.display())),
    );
    let known_projects = db.list_projects().unwrap_or_default();
    let entity = cx.new(|_| ThreadStore {
        summaries: Vec::new(),
        session_paths: HashMap::new(),
        known_projects,
        running: HashSet::new(),
        live_threads: HashMap::new(),
        sessions_dir: dir,
        db,
    });
    entity.update(cx, |s, cx| s.refresh(cx));
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

    /// All registered project paths. The sidebar renders a folder for every
    /// path here.
    pub fn known_projects(&self) -> &[String] {
        &self.known_projects
    }

    /// Register a project path: in-memory list + persisted to the db
    /// `projects` table so sidebar folders survive restarts even when all
    /// their threads are archived.
    pub fn register_project(&mut self, path: String, cx: &mut Context<Self>) {
        if path.is_empty() || self.known_projects.contains(&path) {
            return;
        }
        self.known_projects.push(path.clone());
        if let Err(e) = self.db.register_project(&path) {
            tracing::warn!(error = %e, "failed to persist project registration");
        }
        cx.emit(ThreadStoreEvent::SummariesUpdated);
        cx.notify();
    }

    /// Whether the given thread id is currently running a turn.
    pub fn is_running(&self, id: &str) -> bool {
        self.running.contains(id)
    }

    /// Mark a thread as running (turn started).
    pub fn mark_running(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.running.insert(id.to_string()) {
            cx.emit(ThreadStoreEvent::RunningChanged);
        }
    }

    /// Mark a thread as idle (turn ended).
    pub fn mark_idle(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.running.remove(id) {
            cx.emit(ThreadStoreEvent::RunningChanged);
            cx.notify();
        }
    }

    /// Set the unread flag on a session (persisted in its sidecar).
    pub fn set_unread(&mut self, id: &str, unread: bool, cx: &mut Context<Self>) {
        if let Some(s) = self.summaries.iter_mut().find(|s| s.id == id)
            && s.has_unread == unread
        {
            return;
        }
        if let Some(s) = self.summaries.iter_mut().find(|s| s.id == id) {
            s.has_unread = unread;
        }
        self.write_meta(id, move |meta| meta.unread = unread, true, cx);
    }

    /// Set the errored flag on a session (persisted in its sidecar).
    pub fn set_errored(&mut self, id: &str, errored: bool, cx: &mut Context<Self>) {
        if let Some(s) = self.summaries.iter_mut().find(|s| s.id == id)
            && s.errored == errored
        {
            return;
        }
        if let Some(s) = self.summaries.iter_mut().find(|s| s.id == id) {
            s.errored = errored;
        }
        self.write_meta(id, move |meta| meta.errored = errored, true, cx);
    }

    /// Re-read the session directory and refresh the summary list. Runs off
    /// the UI thread so a large session folder cannot stall the main thread.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        let dir = self.sessions_dir.clone();
        let this = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            // The session directory scan uses tokio::fs, which needs a tokio
            // runtime context; the gpui executor is not one, so hop onto the
            // agent runtime and await the result back here.
            let list = crate::runtime::handle()
                .spawn(async move { load_summaries(&dir).await })
                .await
                .unwrap_or_default();
            this.update(cx, |s, cx| {
                s.session_paths = list.iter().map(|(sum, path)| (sum.id.clone(), path.clone())).collect();
                s.summaries = list.into_iter().map(|(sum, _)| sum).collect();
                // Backfill known projects from session cwds so folders show
                // up even without an explicit registration (first run and
                // pre-registration sessions alike).
                let candidates: Vec<String> = s
                    .summaries
                    .iter()
                    .map(|sum| sum.project.clone())
                    .filter(|p| !p.is_empty())
                    .collect();
                let new_projects = merge_new_projects(&mut s.known_projects, &candidates);
                for path in new_projects {
                    if let Err(e) = s.db.register_project(&path) {
                        tracing::warn!(error = %e, "failed to persist project registration");
                    }
                }
                cx.emit(ThreadStoreEvent::SummariesUpdated);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Load and restore a `Thread` by id (model resolved from the registry).
    pub fn load_thread(&mut self, id: &str, cx: &mut App) -> Option<Entity<Thread>> {
        if let Some(entity) = self.live_threads.get(id).and_then(WeakEntity::upgrade) {
            return Some(entity);
        }
        let path = self.session_paths.get(id)?.clone();
        let cwd = self
            .summaries
            .iter()
            .find(|s| s.id == id)
            .map(|s| PathBuf::from(s.project.clone()))
            .unwrap_or_else(|| PathBuf::from("."));
        let entity = Thread::open_existing(ThreadId(id.to_string()), cwd, path, cx);
        self.live_threads.insert(id.to_string(), entity.downgrade());
        Some(entity)
    }

    /// Create a fresh empty `Thread` (sidebar "new conversation" button).
    pub fn new_thread(&mut self, cwd: PathBuf, cx: &mut App) -> Entity<Thread> {
        let id = uuid::Uuid::new_v4().to_string();
        let entity = Thread::new(ThreadId(id.clone()), cwd, cx);
        self.live_threads.insert(id, entity.downgrade());
        entity
    }

    /// Archive (or unarchive) a session (persisted in its sidecar).
    pub fn archive_thread(&mut self, id: &str, archived: bool, cx: &mut Context<Self>) {
        self.write_meta(id, move |meta| meta.archived = archived, false, cx);
        self.refresh(cx);
    }

    /// Toggle the pinned flag on a session (persisted in its sidecar).
    pub fn pin_thread(&mut self, id: &str, pinned: bool, cx: &mut Context<Self>) {
        self.write_meta(id, move |meta| meta.pinned = pinned, false, cx);
        self.refresh(cx);
    }

    /// Append a `model_change` event. The pi transcript records model changes
    /// itself; nothing to do here.
    pub fn record_model_change(
        &self,
        _thread_id: &str,
        _from: Option<&str>,
        _to: &str,
        _cx: &mut Context<Self>,
    ) {
    }

    /// Append a typed event to the thread's timeline. The manox SQLite
    /// timeline is not produced by the pi backend.
    pub fn record_event(
        &self,
        _thread_id: &str,
        _event_type: crate::db::ThreadEventType,
        _data: &serde_json::Value,
        _cx: &mut Context<Self>,
    ) {
    }

    /// Persist a UI annotation card. Not produced by the pi backend.
    pub fn record_ui_note(
        &self,
        _thread_id: &str,
        _kind: crate::db::UiNoteKind,
        _anchor_user_id: Option<&str>,
        _data: &serde_json::Value,
        _cx: &mut Context<Self>,
    ) {
    }

    /// Load the sidecar meta for a session, apply `update`, and write back.
    /// `refresh_list` emits `SummariesUpdated` for the in-memory flags; the
    /// archive/pin path refreshes the whole list instead.
    fn write_meta(
        &self,
        id: &str,
        update: impl FnOnce(&mut pi_extensions::session_meta::SessionMeta) + Send + 'static,
        refresh_list: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(path) = self.session_paths.get(id).cloned() else {
            return;
        };
        let dir = self.sessions_dir.clone();
        cx.spawn(async move |_, _| {
            let handle = crate::runtime::handle();
            handle
                .spawn(async move {
                    if let Ok(mut meta) = pi_extensions::session_meta::load(&dir, &path).await {
                        update(&mut meta);
                        let _ = pi_extensions::session_meta::save(&dir, &path, &meta).await;
                    }
                })
                .await
                .ok();
        })
        .detach();
        if refresh_list {
            cx.emit(ThreadStoreEvent::SummariesUpdated);
            cx.notify();
        }
    }
}

/// Persist a `Thread` snapshot. The pi transcript persists itself — this
/// refreshes the sidebar list on real user activity.
pub fn save_thread(_thread: Entity<Thread>, touch: bool, cx: &mut App) {
    if touch {
        let store = global();
        store.update(cx, |s, cx| s.refresh(cx));
    }
}

/// Read every session plus its sidecar into the sidebar summary shape.
async fn load_summaries(dir: &std::path::Path) -> Vec<(ThreadSummary, PathBuf)> {
    let repo = pi::session::repository::SessionRepository::new(dir);
    let Ok(list) = repo.list().await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for info in list {
        let meta = pi_extensions::session_meta::load(dir, &info.path)
            .await
            .unwrap_or_default();
        let path = info.path.clone();
        out.push((session_info_to_summary(&info, &meta), path));
    }
    out
}

/// Map a pi session info + sidecar onto the sidebar summary shape.
fn session_info_to_summary(
    info: &pi::session::repository::SessionInfo,
    meta: &pi_extensions::session_meta::SessionMeta,
) -> ThreadSummary {
    let summary = if info.first_message.trim().is_empty() {
        "(no messages)".to_string()
    } else {
        info.first_message.clone()
    };
    ThreadSummary {
        id: info.id.clone(),
        summary: summary.clone(),
        title: meta.title.clone(),
        title_override: None,
        model_id: String::new(),
        provider_id: None,
        approval_mode: 0,
        project: info.cwd.clone(),
        depth: 0,
        parent_id: info.parent_session_path.clone(),
        archived: meta.archived,
        pinned: meta.pinned,
        has_unread: meta.unread,
        errored: meta.errored,
        created_at: info.created_at.timestamp(),
        interacted_at: info.modified_at.timestamp(),
        updated_at: info.modified_at.timestamp(),
        cumulative_total_tokens: 0,
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn init_for_test(db: std::sync::Arc<crate::db::ThreadsDatabase>, cx: &mut App) {
    let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    let entity = cx.new(|_| ThreadStore {
        summaries: Vec::new(),
        session_paths: HashMap::new(),
        known_projects: Vec::new(),
        db: db.clone(),
        running: HashSet::new(),
        live_threads: HashMap::new(),
        sessions_dir: dir,
    });
    *TEST_OVERRIDE.lock().unwrap() = Some(entity);
}

/// Append candidates not already known, preserving first-seen order;
/// returns the newly added paths (for persistence).
fn merge_new_projects(known: &mut Vec<String>, candidates: &[String]) -> Vec<String> {
    let mut added = Vec::new();
    for path in candidates {
        if !path.is_empty() && !known.contains(path) {
            known.push(path.clone());
            added.push(path.clone());
        }
    }
    added
}

#[cfg(any(test, feature = "test-support"))]
pub fn drop_for_test() {
    *TEST_OVERRIDE.lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> (std::sync::Arc<crate::db::ThreadsDatabase>, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "pi-store-test-{}.db",
            uuid::Uuid::new_v4()
        ));
        let db = std::sync::Arc::new(
            crate::db::ThreadsDatabase::open(&path).expect("open temp threads db"),
        );
        (db, path)
    }

    fn store_entity(
        cx: &mut gpui::TestAppContext,
        db: std::sync::Arc<crate::db::ThreadsDatabase>,
    ) -> gpui::Entity<ThreadStore> {
        let known_projects = db.list_projects().unwrap_or_default();
        cx.update(|cx| {
            cx.new(|_| ThreadStore {
                summaries: Vec::new(),
                session_paths: HashMap::new(),
                known_projects,
                db,
                running: HashSet::new(),
                live_threads: HashMap::new(),
                sessions_dir: std::env::temp_dir(),
            })
        })
    }

    #[test]
    fn register_project_persists_and_survives_reopen() {
        let (db, path) = temp_db();
        let mut cx = gpui::TestAppContext::single();
        let store = store_entity(&mut cx, db.clone());
        cx.update(|cx| {
            store.update(cx, |s, cx| s.register_project("/p/a".into(), cx));
        });
        // Persisted to the db...
        assert!(db.list_projects().unwrap().contains(&"/p/a".to_string()));
        // ...and a freshly initialized store (simulated restart) sees it.
        let reopened = store_entity(&mut cx, db.clone());
        let known = cx.update(|cx| reopened.read(cx).known_projects().to_vec());
        assert_eq!(known, vec!["/p/a".to_string()]);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn register_project_dedupes() {
        let (db, path) = temp_db();
        let mut cx = gpui::TestAppContext::single();
        let store = store_entity(&mut cx, db.clone());
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.register_project("/p/a".into(), cx);
                s.register_project("/p/a".into(), cx);
                s.register_project(String::new(), cx);
            });
        });
        let known = cx.update(|cx| store.read(cx).known_projects().to_vec());
        assert_eq!(known, vec!["/p/a".to_string()]);
        assert_eq!(db.list_projects().unwrap().len(), 1);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn merge_new_projects_keeps_order_and_skips_duplicates() {
        let mut known = vec!["/p/a".to_string()];
        let added = merge_new_projects(
            &mut known,
            &[
                "/p/b".into(),
                "/p/a".into(),
                String::new(),
                "/p/c".into(),
                "/p/b".into(),
            ],
        );
        assert_eq!(added, vec!["/p/b".to_string(), "/p/c".to_string()]);
        assert_eq!(
            known,
            vec!["/p/a".to_string(), "/p/b".to_string(), "/p/c".to_string()]
        );
    }
}

