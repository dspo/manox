// The pi-backed `ThreadStore` facade (built with `feature = "harness-pi"`).
//
// The sidebar's session list comes from the pi session repository (jsonl)
// plus a per-session UI-metadata sidecar (`pi_extensions::session_meta`).
// The pi transcript persists itself, so `save_thread` is a no-op and the
// manox SQLite timeline/note records are not produced.
// Archived sessions are excluded from the sidebar list but stay in
// `session_paths` so their sidecar remains addressable.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

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
    /// Archived rows kept addressable so a surface can list them behind a
    /// "more" affordance; the main sidebar list renders `summaries` only.
    archived_summaries: Vec<ThreadSummary>,
    /// Session file path per summary id, for sidecar writes and reopen.
    session_paths: HashMap<String, PathBuf>,
    known_projects: Vec<String>,
    /// Host db handle persisting `known_projects` (shared threads.db,
    /// `projects` table only — thread rows remain the manox store's domain).
    db: std::sync::Arc<crate::db::ThreadsDatabase>,
    running: HashSet<String>,
    /// Threads with a tool authorization pending a user verdict (a parked
    /// thread's approval card is not visible, so the sidebar badge is the
    /// only signal until the user switches back). In-memory only: cleared on
    /// attach, on terminal events, and when the run resumes past the call.
    pending_auth: HashSet<String>,
    /// Threads whose last turn parked on a plan-review verdict awaiting the
    /// user's choice. Mirrors `pending_auth` (the sidebar shows a static
    /// icon, not a spinner, while a verdict is due); cleared on verdict,
    /// terminal events, and error.
    pending_plan: HashSet<String>,
    /// Threads with live monitors or background bash: no turn is in flight,
    /// but the loop can still self-advance on external events. Populated
    /// from `BackgroundTaskUpdated` via the legacy registry's per-thread
    /// running-task check.
    background_work: HashSet<String>,
    /// Canonical entity lookup without retaining idle threads indefinitely.
    live_threads: HashMap<String, WeakEntity<Thread>>,
    sessions_dir: PathBuf,
}

impl EventEmitter<ThreadStoreEvent> for ThreadStore {}

/// `Mutex<Option<_>>` (not a `OnceLock`) so test-support can reset the
/// global between gpui test apps — the store entity otherwise leaks past the
/// test context's leak detector.
static GLOBAL: std::sync::Mutex<Option<Entity<ThreadStore>>> = std::sync::Mutex::new(None);

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
        archived_summaries: Vec::new(),
        session_paths: HashMap::new(),
        known_projects,
        running: HashSet::new(),
        pending_auth: HashSet::new(),
        pending_plan: HashSet::new(),
        background_work: HashSet::new(),
        live_threads: HashMap::new(),
        sessions_dir: dir,
        db,
    });
    entity.update(cx, |s, cx| s.refresh(cx));
    *GLOBAL.lock().unwrap() = Some(entity);
}

/// Returns the global `ThreadStore` `Entity`. Panics if `init` was not called.
pub fn global() -> Entity<ThreadStore> {
    try_global().expect("ThreadStore not initialized; call agent::init first")
}

/// The global store when initialized (`agent::init`, or `init_for_test`);
/// `None` before init so teardown paths (team disband) can skip archival
/// instead of panicking in store-less environments.
pub fn try_global() -> Option<Entity<ThreadStore>> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(entity) = TEST_OVERRIDE.lock().unwrap().clone() {
        return Some(entity);
    }
    GLOBAL.lock().unwrap().clone()
}

/// Drop the global store entity — test-support only, so a gpui test app can
/// tear down without the leak detector tripping on the process-global entity.
#[cfg(any(test, feature = "test-support"))]
pub fn drop_global_for_test() {
    *GLOBAL.lock().unwrap() = None;
}

impl ThreadStore {
    pub fn summaries(&self) -> &[ThreadSummary] {
        &self.summaries
    }

    /// Archived rows, partitioned out of `summaries` so the sidebar list
    /// stays clean while surfaces can still render them on demand.
    pub fn archived_summaries(&self) -> &[ThreadSummary] {
        &self.archived_summaries
    }

    /// Mutable lookup across both partitions by id.
    fn summary_mut(&mut self, id: &str) -> Option<&mut ThreadSummary> {
        self.summaries
            .iter_mut()
            .find(|s| s.id == id)
            .or_else(|| self.archived_summaries.iter_mut().find(|s| s.id == id))
    }

    /// Immutable lookup across both partitions by id.
    fn summary_by_id(&self, id: &str) -> Option<&ThreadSummary> {
        self.summaries
            .iter()
            .find(|s| s.id == id)
            .or_else(|| self.archived_summaries.iter().find(|s| s.id == id))
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
            cx.notify();
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
        if let Some(s) = self.summary_mut(id)
            && s.has_unread == unread
        {
            return;
        }
        if let Some(s) = self.summary_mut(id) {
            s.has_unread = unread;
        }
        self.write_meta(id, move |meta| meta.unread = unread, cx);
    }

    /// Whether a thread has a tool authorization pending a user verdict.
    pub fn pending_auth_contains(&self, id: &str) -> bool {
        self.pending_auth.contains(id)
    }

    /// Mark/unmark a thread as awaiting a tool-authorization verdict. Fires
    /// `SummariesUpdated` so the sidebar badge appears without waiting for a
    /// rescan. In-memory only — the badge is a live-state signal, never
    /// persisted.
    pub fn mark_pending_auth(&mut self, id: &str, pending: bool, cx: &mut Context<Self>) {
        let changed = if pending {
            self.pending_auth.insert(id.to_string())
        } else {
            self.pending_auth.remove(id)
        };
        if changed {
            cx.emit(ThreadStoreEvent::SummariesUpdated);
            cx.notify();
        }
    }

    /// Whether a thread's turn is parked on a plan-review verdict.
    pub fn pending_plan_contains(&self, id: &str) -> bool {
        self.pending_plan.contains(id)
    }

    /// Mark/unmark a thread as awaiting a plan-review verdict. Same lifecycle
    /// and event as `mark_pending_auth`: the sidebar's blue static icon (not
    /// the spinner) signals the wait until the user decides.
    pub fn mark_pending_plan(&mut self, id: &str, pending: bool, cx: &mut Context<Self>) {
        let changed = if pending {
            self.pending_plan.insert(id.to_string())
        } else {
            self.pending_plan.remove(id)
        };
        if changed {
            cx.emit(ThreadStoreEvent::SummariesUpdated);
            cx.notify();
        }
    }

    /// Whether a thread has live monitors or background bash (the loop can
    /// still self-advance even with no turn in flight).
    pub fn background_work_contains(&self, id: &str) -> bool {
        self.background_work.contains(id)
    }

    /// Mark/unmark a thread as carrying live background work. Fires
    /// `RunningChanged` (the spinner-driving event) so the sidebar re-evaluates
    /// the rotating state without a list rescan.
    pub fn mark_background_work(&mut self, id: &str, active: bool, cx: &mut Context<Self>) {
        let changed = if active {
            self.background_work.insert(id.to_string())
        } else {
            self.background_work.remove(id)
        };
        if changed {
            cx.emit(ThreadStoreEvent::RunningChanged);
            cx.notify();
        }
    }

    /// Set the errored flag on a session (persisted in its sidecar).
    pub fn set_errored(&mut self, id: &str, errored: bool, cx: &mut Context<Self>) {
        if let Some(s) = self.summary_mut(id)
            && s.errored == errored
        {
            return;
        }
        if let Some(s) = self.summary_mut(id) {
            s.errored = errored;
        }
        self.write_meta(id, move |meta| meta.errored = errored, cx);
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
                let (session_paths, summaries, archived) = project_session_lists(list);
                s.session_paths = session_paths;
                s.summaries = summaries;
                s.archived_summaries = archived;
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
            .summary_by_id(id)
            .map(|s| PathBuf::from(s.project.clone()))
            .unwrap_or_else(|| PathBuf::from("."));
        let entity = Thread::open_existing(ThreadId(id.to_string()), cwd, path, cx);
        // Re-surface the bound project from the sidecar so the chip shows it.
        if let Some(sum) = self.summary_by_id(id)
            && !sum.project.is_empty()
        {
            let dir = PathBuf::from(&sum.project);
            entity.update(cx, |t, _| t.restore_project(dir));
        }
        self.live_threads.insert(id.to_string(), entity.downgrade());
        Some(entity)
    }

    /// The live (in-memory) thread for `id`, if its entity is still alive.
    /// Unlike [`ThreadStore::load_thread`], never restores from disk — the
    /// archive path only needs threads that could still hold a live team.
    pub fn live_thread(&self, id: &str) -> Option<Entity<Thread>> {
        self.live_threads.get(id).and_then(WeakEntity::upgrade)
    }

    /// Track a live thread (team leader/member at `set_team`) so the
    /// centralized archive-teardown can reach its team from the id alone.
    pub fn register_live_thread(&mut self, id: &str, t: WeakEntity<Thread>) {
        self.live_threads.insert(id.to_string(), t);
    }

    /// Seed an active summary row without touching disk — lets foreign test
    /// modules (team lifecycle) exercise the archive cascade against real
    /// thread ids.
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_summary_for_test(&mut self, id: &str, parent: Option<&str>) {
        self.summaries.push(crate::db::ThreadSummary {
            id: id.to_string(),
            summary: id.to_string(),
            title: None,
            title_override: None,
            model_id: String::new(),
            provider_id: None,
            approval_mode: 0,
            project: String::new(),
            depth: parent.is_some() as i32,
            parent_id: parent.map(str::to_string),
            archived: false,
            pinned: false,
            has_unread: false,
            errored: false,
            created_at: 0,
            interacted_at: 0,
            updated_at: 0,
            cumulative_total_tokens: 0,
        });
    }

    /// Archive (or unarchive) a session. The row moves between the active
    /// and archived partitions immediately; the post-write refresh in
    /// `write_meta` re-syncs both partitions from disk. Archiving cascades
    /// to every descendant along `parent_id` (team members, fork children —
    /// one hierarchy rule); unarchiving moves only the requested row.
    /// Re-asserting the current state is a no-op: no partition move, meta
    /// write, or lifecycle hook.
    pub fn archive_thread(&mut self, id: &str, archived: bool, cx: &mut Context<Self>) {
        if self.summary_by_id(id).is_some_and(|s| s.archived == archived) {
            return;
        }
        let ids = if archived {
            self.descendant_ids(id)
        } else {
            vec![id.to_string()]
        };
        if archived {
            // Archive-leader invariant, centralized: any live thread in the
            // cascade that still leads a team is torn down here (running
            // members cancelled, roster released) so no orphan keeps working
            // after its row is archived — covers every current and future
            // archive entry point. `teardown` is store-free, keeping this
            // method re-entrancy-safe; the cascade below archives the rows.
            for tid in &ids {
                if let Some(thread) = self.live_thread(tid) {
                    if let Some(team) = thread.read(cx).team().cloned() {
                        // Only a leader's archive tears the team down; a
                        // dismissed member must not cancel its siblings.
                        if team.read_with(cx, |t, _| t.is_leader(&thread)) {
                            team.update(cx, |t, cx| t.teardown(cx));
                        }
                    }
                    thread.update(cx, |t, cx| t.clear_team(cx));
                }
            }
        }
        for tid in ids {
            // A row already at the target state (e.g. archived by the
            // caller's disband earlier) skips move + meta + hook: one
            // SessionEnd per working life.
            if self.summary_by_id(&tid).is_some_and(|s| s.archived == archived) {
                continue;
            }
            if archived {
                if let Some(pos) = self.summaries.iter().position(|s| s.id == tid) {
                    let mut summary = self.summaries.swap_remove(pos);
                    summary.archived = true;
                    self.archived_summaries.push(summary);
                }
            } else if let Some(pos) = self.archived_summaries.iter().position(|s| s.id == tid) {
                let mut summary = self.archived_summaries.swap_remove(pos);
                summary.archived = false;
                self.summaries.push(summary);
            }
            self.write_meta(&tid, move |meta| meta.archived = archived, cx);
            if archived {
                // Plugin lifecycle: archiving ends the session's working life
                // (the retired harness fired on thread deletion; the pi path
                // keeps sessions and archives instead). Fail-open, detached.
                crate::plugin_hooks::fire(
                    crate::plugin_hooks::HookEvent::SessionEnd,
                    None,
                    serde_json::json!({ "thread_id": tid }),
                );
            }
        }
    }

    /// `id` plus every transitive child across both partitions, each parent
    /// before its children (the archive cascade set).
    fn descendant_ids(&self, id: &str) -> Vec<String> {
        let mut out = vec![id.to_string()];
        let mut frontier = vec![id.to_string()];
        while let Some(parent) = frontier.pop() {
            for s in self.summaries.iter().chain(self.archived_summaries.iter()) {
                if s.parent_id.as_deref() == Some(parent.as_str()) && !out.contains(&s.id) {
                    frontier.push(s.id.clone());
                    out.push(s.id.clone());
                }
            }
        }
        out
    }

    /// Toggle the pinned flag on a session (persisted in its sidecar).
    pub fn pin_thread(&mut self, id: &str, pinned: bool, cx: &mut Context<Self>) {
        if let Some(s) = self.summary_mut(id) {
            s.pinned = pinned;
        }
        self.write_meta(id, move |meta| meta.pinned = pinned, cx);
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

    /// Persist a sidecar change for a session. The caller's in-memory update
    /// is the render source of truth, so `SummariesUpdated` fires up front;
    /// the sidecar write is best-effort. The list is rescanned only after
    /// the write lands — a rescan racing the write would re-read stale
    /// sidecar flags and revert the in-memory state.
    fn write_meta(
        &self,
        id: &str,
        update: impl FnOnce(&mut pi_extensions::session_meta::SessionMeta) + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.emit(ThreadStoreEvent::SummariesUpdated);
        cx.notify();
        let Some(path) = self.session_paths.get(id).cloned() else {
            return;
        };
        let dir = self.sessions_dir.clone();
        let this = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            let saved = crate::runtime::handle()
                .spawn(async move {
                    pi_extensions::session_meta::update(&dir, &path, update)
                        .await
                        .is_ok()
                })
                .await
                .unwrap_or(false);
            if saved {
                this.update(cx, |s, cx| s.refresh(cx)).ok();
            }
        })
        .detach();
    }
}

/// Persist a `Thread` snapshot. The pi transcript persists itself — this
/// mirrors the current `ui_notes` (host-only annotation cards) into the
/// session sidecar on real user activity, and refreshes the sidebar list.
///
/// The sidecar write runs through the per-session serialized `update` path,
/// so it cannot interleave with a concurrent archive/pin/unread write (the
/// `/exit` flow spawns both back-to-back); a crash mid-write loses at most
/// that write — the same window the jsonl transcript has.
pub fn save_thread(thread: Entity<Thread>, touch: bool, cx: &mut App) {
    let store = global();
    let id = thread.read(cx).id.0.clone();
    let snapshot = serde_json::to_value(thread.read(cx).ui_notes())
        .ok()
        .filter(|v| !matches!(v, serde_json::Value::Array(a) if a.is_empty()));
    store.update(cx, |s, cx| {
        if touch {
            s.refresh(cx);
        }
        s.write_meta(&id, move |meta| meta.ui_notes = snapshot.clone(), cx);
    });
}

/// Read every session plus its sidecar into the sidebar summary shape.
async fn load_summaries(dir: &std::path::Path) -> Vec<(ThreadSummary, PathBuf)> {
    let repo = pi::session::repository::SessionRepository::new(dir);
    let Ok(list) = repo.list().await else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for info in list {
        // The sidebar renders only the current host's sessions; other hosts'
        // files stay addressable on disk but never surface here.
        if !crate::host::belongs_to_current_host(info.metadata.as_ref()) {
            continue;
        }
        let meta = match pi_extensions::session_meta::load(dir, &info.path).await {
            Ok(meta) => meta,
            Err(error) => {
                tracing::warn!(session = %info.id, error = %error, "session sidecar unreadable; rendering default flags");
                pi_extensions::session_meta::SessionMeta::default()
            }
        };
        let path = info.path.clone();
        out.push((session_info_to_summary(&info, &meta), path));
    }
    resolve_depths(&mut out);
    out
}

/// Maximum team nesting depth. One cap serves two roles: it bounds a legal
/// chain at 8 levels, and it terminates any cycle — a cycle is an infinite
/// chain, so the walk always trips the cap and degrades to top-level. There
/// is no separate visited set; the cap is both the cycle guard and the
/// legal-depth ceiling.
const MAX_TEAM_DEPTH: usize = 8;

/// Compute each summary's `depth` by walking its `parent_id` chain within the
/// loaded list. A parent missing from the list (deleted leader, foreign
/// host) leaves the row top-level; a cycle or an over-long chain likewise
/// degrades to 0 instead of looping or nesting wildly.
fn resolve_depths(list: &mut [(ThreadSummary, PathBuf)]) {
    let parents: HashMap<String, Option<String>> = list
        .iter()
        .map(|(s, _)| (s.id.clone(), s.parent_id.clone()))
        .collect();
    for (sum, _) in list.iter_mut() {
        let mut depth = 0usize;
        let mut cur = sum.parent_id.as_deref();
        while let Some(parent) = cur {
            if depth >= MAX_TEAM_DEPTH {
                depth = 0;
                break;
            }
            match parents.get(parent) {
                // A present parent is one nesting level; keep walking. A
                // parent with no parent of its own ends the chain.
                Some(Some(next)) => {
                    depth += 1;
                    cur = Some(next);
                }
                Some(None) => {
                    depth += 1;
                    break;
                }
                // Orphan: the parent is not in this host's list.
                None => {
                    depth = 0;
                    break;
                }
            }
        }
        sum.depth = depth as i32;
    }
}

/// Split a loaded session list into the id→path map (every session — an
/// archived session must stay addressable so archive/unarchive and the
/// unread/error flags can still reach its sidecar), the active list the
/// sidebar renders, and the archived list kept separately so surfaces that
/// offer an archive view can still reach it.
fn project_session_lists(
    list: Vec<(ThreadSummary, PathBuf)>,
) -> (
    HashMap<String, PathBuf>,
    Vec<ThreadSummary>,
    Vec<ThreadSummary>,
) {
    let paths = list
        .iter()
        .map(|(sum, path)| (sum.id.clone(), path.clone()))
        .collect();
    let mut active = Vec::new();
    let mut archived = Vec::new();
    for (sum, _) in list {
        if sum.archived {
            archived.push(sum);
        } else {
            active.push(sum);
        }
    }
    (paths, active, archived)
}


/// The team leader's session id from a session header's `team.parent`, when
/// present. Shared by the sidebar store and the actor's mirrored session
/// list so both resolve the affiliation identically.
pub(crate) fn team_parent_id(info: &pi::session::repository::SessionInfo) -> Option<String> {
    info.metadata
        .as_ref()
        .and_then(|m| m.get("team"))
        .and_then(|t| t.get("parent"))
        .and_then(|p| p.as_str())
        .map(str::to_string)
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
        // Team affiliation wins over a fork lineage when both are present:
        // the fork link is history, the team link is the live hierarchy.
        // A fork lineage alone also nests under its fork source when both
        // rows share a list (the tree renderer treats any parent_id as a
        // hierarchy edge).
        parent_id: team_parent_id(info).or_else(|| info.parent_session_path.clone()),
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
        archived_summaries: Vec::new(),
        session_paths: HashMap::new(),
        known_projects: Vec::new(),
        db: db.clone(),
        running: HashSet::new(),
        pending_auth: HashSet::new(),
        pending_plan: HashSet::new(),
        background_work: HashSet::new(),
        live_threads: HashMap::new(),
        sessions_dir: dir,
    });
    *TEST_OVERRIDE.lock().unwrap() = Some(entity);
}

#[cfg(any(test, feature = "test-support"))]
pub fn drop_for_test() {
    *TEST_OVERRIDE.lock().unwrap() = None;
}

/// Serializes tests that install the process-global store override
/// (`init_for_test` / `drop_for_test`); the override is a single slot, so
/// store-backed tests in different modules must not interleave.
#[cfg(any(test, feature = "test-support"))]
pub fn store_test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(std::sync::Mutex::default)
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
                archived_summaries: Vec::new(),
                session_paths: HashMap::new(),
                known_projects,
                db,
                running: HashSet::new(),
                pending_auth: HashSet::new(),
                pending_plan: HashSet::new(),
                background_work: HashSet::new(),
                live_threads: HashMap::new(),
                sessions_dir: std::env::temp_dir(),
            })
        })
    }

    /// The ui_notes sidecar write path: `write_meta` snapshots the caller's
    /// update through the detached load→modify→save pipeline, and a
    /// subsequent sidecar load sees it — the mechanism `save_thread` uses to
    /// persist `Thread::ui_notes` (Error / Notice / PlanReview cards,
    /// including `data.tool_call_id` on approval records).
    #[test]
    fn write_meta_persists_ui_notes_to_sidecar() {
        let (db, db_path) = temp_db();
        let mut cx = gpui::TestAppContext::single();
        cx.update(crate::runtime::init);
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("t1.jsonl");
        let store = store_entity(&mut cx, db.clone());
        cx.update(|cx| {
            store.update(cx, |s, _| {
                s.session_paths.insert("t1".to_string(), session.clone());
                s.sessions_dir = dir.path().to_path_buf();
            });
        });
        let notes = serde_json::json!([
            {
                "anchor_user_id": "u1",
                "kind": "notice",
                "data": { "text": "Bash allowed", "tool_call_id": "tu_1" }
            }
        ]);
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.write_meta("t1", move |meta| meta.ui_notes = Some(notes.clone()), cx);
            });
        });
        // Start the gpui-side write task (it parks awaiting the tokio inner
        // task), then poll the tokio runtime until the sidecar write lands.
        cx.executor().run_until_parked();
        let mut records: Option<Vec<crate::db::UiNoteRecord>> = None;
        for _ in 0..200 {
            if let Ok(meta) = crate::runtime::handle().block_on(
                pi_extensions::session_meta::load(dir.path(), &session),
            ) && let Some(notes) = meta.ui_notes
            {
                records = serde_json::from_value(notes).ok();
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let records = records.expect("sidecar write landed within the poll window");
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].data.get("tool_call_id").and_then(|v| v.as_str()),
            Some("tu_1"),
            "the tool anchor survives the sidecar write"
        );
        std::fs::remove_file(db_path).ok();
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

    /// The pending-auth badge marker toggles per thread id and only emits
    /// `SummariesUpdated` on an actual state change.
    #[test]
    fn mark_pending_auth_toggles_marker() {
        let (db, path) = temp_db();
        let mut cx = gpui::TestAppContext::single();
        let store = store_entity(&mut cx, db.clone());
        let events = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sub = {
            let events = std::sync::Arc::clone(&events);
            cx.update(|cx| {
                cx.subscribe(&store, move |_, _: &ThreadStoreEvent, _| {
                    events.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                })
            })
        };
        cx.update(|cx| {
            store.update(cx, |s, cx| s.mark_pending_auth("t1", true, cx));
        });
        assert!(cx.update(|cx| store.read(cx).pending_auth_contains("t1")));
        // Idempotent mark: no event, no duplicate work.
        cx.update(|cx| {
            store.update(cx, |s, cx| s.mark_pending_auth("t1", true, cx));
        });
        assert_eq!(events.load(std::sync::atomic::Ordering::SeqCst), 1);
        cx.update(|cx| {
            store.update(cx, |s, cx| s.mark_pending_auth("t1", false, cx));
        });
        assert!(!cx.update(|cx| store.read(cx).pending_auth_contains("t1")));
        assert_eq!(events.load(std::sync::atomic::Ordering::SeqCst), 2);
        drop(sub);
        std::fs::remove_file(path).ok();
    }

    /// The running-set marker (the sidebar spinner source) toggles per thread
    /// id, fires `RunningChanged` only on an actual state change, and is
    /// idempotent under repeated marks — the store contract every host
    /// subscription (foreground, parked, actor) relies on.
    #[test]
    fn mark_running_toggles_marker() {
        let (db, path) = temp_db();
        let mut cx = gpui::TestAppContext::single();
        let store = store_entity(&mut cx, db.clone());
        let events = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sub = {
            let events = std::sync::Arc::clone(&events);
            cx.update(|cx| {
                cx.subscribe(&store, move |_, _: &ThreadStoreEvent, _| {
                    events.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                })
            })
        };
        cx.update(|cx| {
            store.update(cx, |s, cx| s.mark_running("t1", cx));
        });
        assert!(cx.update(|cx| store.read(cx).is_running("t1")));
        assert_eq!(events.load(std::sync::atomic::Ordering::SeqCst), 1);
        // Idempotent mark: no event, no duplicate work.
        cx.update(|cx| {
            store.update(cx, |s, cx| s.mark_running("t1", cx));
        });
        assert_eq!(events.load(std::sync::atomic::Ordering::SeqCst), 1);
        // A second thread marks independently.
        cx.update(|cx| {
            store.update(cx, |s, cx| s.mark_running("t2", cx));
        });
        assert!(cx.update(|cx| store.read(cx).is_running("t2")));
        assert_eq!(events.load(std::sync::atomic::Ordering::SeqCst), 2);
        cx.update(|cx| {
            store.update(cx, |s, cx| s.mark_idle("t1", cx));
        });
        assert!(!cx.update(|cx| store.read(cx).is_running("t1")));
        assert!(cx.update(|cx| store.read(cx).is_running("t2")));
        assert_eq!(events.load(std::sync::atomic::Ordering::SeqCst), 3);
        drop(sub);
        std::fs::remove_file(path).ok();
    }

    /// The plan-review and background-work markers (the blue-static vs
    /// spinner distinction) toggle per thread id and are idempotent under
    /// repeated marks.
    #[test]
    fn plan_and_background_markers_toggle() {
        let (db, path) = temp_db();
        let mut cx = gpui::TestAppContext::single();
        let store = store_entity(&mut cx, db.clone());
        let events = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sub = {
            let events = std::sync::Arc::clone(&events);
            cx.update(|cx| {
                cx.subscribe(&store, move |_, _: &ThreadStoreEvent, _| {
                    events.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                })
            })
        };
        cx.update(|cx| {
            store.update(cx, |s, cx| s.mark_pending_plan("t1", true, cx));
            store.update(cx, |s, cx| s.mark_background_work("t1", true, cx));
        });
        assert!(cx.update(|cx| store.read(cx).pending_plan_contains("t1")));
        assert!(cx.update(|cx| store.read(cx).background_work_contains("t1")));
        assert_eq!(events.load(std::sync::atomic::Ordering::SeqCst), 2);
        // Idempotent marks: no duplicate events.
        cx.update(|cx| {
            store.update(cx, |s, cx| s.mark_pending_plan("t1", true, cx));
            store.update(cx, |s, cx| s.mark_background_work("t1", true, cx));
        });
        assert_eq!(events.load(std::sync::atomic::Ordering::SeqCst), 2);
        // A second thread marks independently; clearing only removes its own.
        cx.update(|cx| {
            store.update(cx, |s, cx| s.mark_pending_plan("t2", true, cx));
        });
        assert_eq!(events.load(std::sync::atomic::Ordering::SeqCst), 3);
        cx.update(|cx| {
            store.update(cx, |s, cx| s.mark_pending_plan("t1", false, cx));
            store.update(cx, |s, cx| s.mark_background_work("t1", false, cx));
        });
        assert!(!cx.update(|cx| store.read(cx).pending_plan_contains("t1")));
        assert!(!cx.update(|cx| store.read(cx).background_work_contains("t1")));
        assert!(cx.update(|cx| store.read(cx).pending_plan_contains("t2")));
        assert_eq!(events.load(std::sync::atomic::Ordering::SeqCst), 5);
        drop(sub);
        std::fs::remove_file(path).ok();
    }

    fn sample_summary(id: &str, archived: bool) -> ThreadSummary {
        ThreadSummary {
            id: id.to_string(),
            summary: String::new(),
            title: None,
            title_override: None,
            model_id: String::new(),
            provider_id: None,
            approval_mode: 0,
            project: String::new(),
            depth: 0,
            parent_id: None,
            archived,
            pinned: false,
            has_unread: false,
            errored: false,
            created_at: 0,
            interacted_at: 0,
            updated_at: 0,
            cumulative_total_tokens: 0,
        }
    }

    #[test]
    fn project_session_lists_partitions_archived_keeps_paths() {
        let (paths, active, archived) = project_session_lists(vec![
            (
                sample_summary("active", false),
                PathBuf::from("active.jsonl"),
            ),
            (
                sample_summary("archived", true),
                PathBuf::from("archived.jsonl"),
            ),
        ]);
        let ids: Vec<&str> = active.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["active"]);
        let archived_ids: Vec<&str> = archived.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(archived_ids, vec!["archived"]);
        // Both partitions stay addressable (unarchive / unread / pin still
        // reach their sidecars); only the active one feeds the sidebar list.
        assert_eq!(paths.len(), 2);
        assert!(paths.contains_key("archived"));
        assert_eq!(
            paths.get("active").unwrap().file_name().unwrap(),
            "active.jsonl"
        );
    }

    fn sample_info(
        id: &str,
        metadata: Option<serde_json::Value>,
    ) -> pi::session::repository::SessionInfo {
        let now = chrono::Utc::now();
        pi::session::repository::SessionInfo {
            path: PathBuf::from(format!("{id}.jsonl")),
            id: id.to_string(),
            cwd: "/p".to_string(),
            name: None,
            parent_session_path: None,
            created_at: now,
            modified_at: now,
            message_count: 1,
            first_message: "hi".to_string(),
            all_messages_text: "hi".to_string(),
            metadata,
        }
    }

    #[test]
    fn summary_prefers_team_parent_over_fork_lineage() {
        let mut info = sample_info(
            "member",
            Some(serde_json::json!({ "team": { "parent": "leader" } })),
        );
        info.parent_session_path = Some("fork-source".to_string());
        let summary =
            session_info_to_summary(&info, &pi_extensions::session_meta::SessionMeta::default());
        assert_eq!(summary.parent_id.as_deref(), Some("leader"));
    }

    #[test]
    fn summary_falls_back_to_fork_parent_without_team_key() {
        let mut info = sample_info("forked", Some(serde_json::json!({ "host": "manox" })));
        info.parent_session_path = Some("source".to_string());
        let summary =
            session_info_to_summary(&info, &pi_extensions::session_meta::SessionMeta::default());
        assert_eq!(summary.parent_id.as_deref(), Some("source"));
    }

    #[test]
    fn resolve_depths_nests_chains_and_degrades_orphans() {
        let mut list = vec![
            (sample_summary("a", false), PathBuf::from("a")),
            (sample_summary("b", false), PathBuf::from("b")),
            (sample_summary("c", false), PathBuf::from("c")),
            (sample_summary("orphan", false), PathBuf::from("orphan")),
        ];
        list[1].0.parent_id = Some("a".into());
        list[2].0.parent_id = Some("b".into());
        list[3].0.parent_id = Some("gone".into());
        resolve_depths(&mut list);
        let depths: Vec<(String, i32)> = list
            .iter()
            .map(|(s, _)| (s.id.clone(), s.depth))
            .collect();
        assert_eq!(
            depths,
            vec![
                ("a".into(), 0),
                ("b".into(), 1),
                ("c".into(), 2),
                ("orphan".into(), 0)
            ]
        );
    }

    #[test]
    fn resolve_depths_breaks_cycles_and_overlong_chains() {
        // a <-> b cycle: neither can resolve a stable depth.
        let mut cycle = vec![
            (sample_summary("a", false), PathBuf::from("a")),
            (sample_summary("b", false), PathBuf::from("b")),
        ];
        cycle[0].0.parent_id = Some("b".into());
        cycle[1].0.parent_id = Some("a".into());
        resolve_depths(&mut cycle);
        assert_eq!(cycle[0].0.depth, 0);
        assert_eq!(cycle[1].0.depth, 0);

        // A chain longer than the cap is malformed: rows whose own depth
        // would exceed the cap degrade to top-level, while rows at or under
        // the cap keep their valid nesting.
        let mut chain: Vec<(ThreadSummary, PathBuf)> = (0..=MAX_TEAM_DEPTH + 1)
            .map(|i| (sample_summary(&format!("n{i}"), false), PathBuf::new()))
            .collect();
        for (i, item) in chain
            .iter_mut()
            .enumerate()
            .skip(1)
            .take(MAX_TEAM_DEPTH + 1)
        {
            item.0.parent_id = Some(format!("n{}", i - 1));
        }
        resolve_depths(&mut chain);
        assert_eq!(
            chain[MAX_TEAM_DEPTH + 1].0.depth,
            0,
            "over-cap row degrades"
        );
        assert_eq!(
            chain[MAX_TEAM_DEPTH].0.depth,
            MAX_TEAM_DEPTH as i32,
            "at-cap row keeps depth"
        );
        assert_eq!(chain[0].0.depth, 0);
    }

    /// The `/exit` flow archives a session and, in the same instant,
    /// `attach_thread` snapshots the outgoing thread's ui_notes — two
    /// back-to-back sidecar writes on the same session. Serialized writes
    /// must keep `archived` on disk while the notes land (the lost-update
    /// that resurrected archived conversations).
    #[test]
    fn archive_survives_concurrent_ui_notes_write() {
        let (db, db_path) = temp_db();
        let mut cx = gpui::TestAppContext::single();
        cx.update(crate::runtime::init);
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("t1.jsonl");
        let store = store_entity(&mut cx, db.clone());
        cx.update(|cx| {
            store.update(cx, |s, _| {
                s.session_paths.insert("t1".to_string(), session.clone());
                s.sessions_dir = dir.path().to_path_buf();
            });
        });
        let notes = serde_json::json!([
            {
                "anchor_user_id": "u1",
                "kind": "notice",
                "data": { "text": "Bash allowed" }
            }
        ]);
        cx.update(|cx| {
            store.update(cx, |s, cx| {
                s.archive_thread("t1", true, cx);
                s.write_meta("t1", move |meta| meta.ui_notes = Some(notes.clone()), cx);
            });
        });
        cx.executor().run_until_parked();
        let mut settled = false;
        // Generous budget: the two sidecar writes run on the process-wide
        // tokio runtime shared with every parallel test in the binary — a
        // loaded CI runner starves them well past the 2s the writes need in
        // isolation. The assertion still catches the lost-update regression;
        // it just doesn't double as a scheduler benchmark.
        for _ in 0..1500 {
            if let Ok(meta) = crate::runtime::handle().block_on(
                pi_extensions::session_meta::load(dir.path(), &session),
            ) && meta.archived
                && meta.ui_notes.is_some()
            {
                settled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(settled, "archived flag lost to a concurrent ui_notes write");
        std::fs::remove_file(db_path).ok();
    }

    /// Minimal summary row for hierarchy tests; only id / parent / archived
    /// matter to the cascade.
    fn cascade_summary(id: &str, parent: Option<&str>) -> ThreadSummary {
        ThreadSummary {
            id: id.to_string(),
            summary: id.to_string(),
            title: None,
            title_override: None,
            model_id: String::new(),
            provider_id: None,
            approval_mode: 0,
            project: String::new(),
            depth: parent.is_some() as i32,
            parent_id: parent.map(str::to_string),
            archived: false,
            pinned: false,
            has_unread: false,
            errored: false,
            created_at: 0,
            interacted_at: 0,
            updated_at: 0,
            cumulative_total_tokens: 0,
        }
    }

    /// Archiving a leader cascades to every transitive descendant (team
    /// members and fork children share the one `parent_id` hierarchy rule).
    #[test]
    fn archive_cascades_to_descendants() {
        let (db, db_path) = temp_db();
        let mut cx = gpui::TestAppContext::single();
        let store = store_entity(&mut cx, db);
        cx.update(|cx| {
            store.update(cx, |s, _| {
                s.summaries.push(cascade_summary("lead", None));
                s.summaries.push(cascade_summary("member", Some("lead")));
                s.summaries.push(cascade_summary("grand", Some("member")));
                s.summaries.push(cascade_summary("sibling", None));
            });
        });
        cx.update(|cx| store.update(cx, |s, cx| s.archive_thread("lead", true, cx)));
        cx.update(|cx| {
            store.update(cx, |s, _| {
                // lead + member + grand archived; unrelated row untouched.
                assert_eq!(s.summaries.len(), 1);
                assert_eq!(s.summaries[0].id, "sibling");
                let archived: Vec<&str> = s
                    .archived_summaries
                    .iter()
                    .map(|s| s.id.as_str())
                    .collect();
                for id in ["lead", "member", "grand"] {
                    assert!(archived.contains(&id), "{id} not archived");
                }
                assert!(s.archived_summaries.iter().all(|s| s.archived));
            });
        });
        std::fs::remove_file(db_path).ok();
    }

    /// Unarchiving moves only the requested row; descendants stay archived.
    #[test]
    fn unarchive_does_not_cascade() {
        let (db, db_path) = temp_db();
        let mut cx = gpui::TestAppContext::single();
        let store = store_entity(&mut cx, db);
        cx.update(|cx| {
            store.update(cx, |s, _| {
                s.summaries.push(cascade_summary("lead", None));
                s.summaries.push(cascade_summary("member", Some("lead")));
            });
        });
        cx.update(|cx| store.update(cx, |s, cx| s.archive_thread("lead", true, cx)));
        cx.update(|cx| store.update(cx, |s, cx| s.archive_thread("lead", false, cx)));
        cx.update(|cx| {
            store.update(cx, |s, _| {
                assert!(s.summaries.iter().any(|s| s.id == "lead" && !s.archived));
                assert!(s
                    .archived_summaries
                    .iter()
                    .any(|s| s.id == "member" && s.archived));
            });
        });
        std::fs::remove_file(db_path).ok();
    }
}
