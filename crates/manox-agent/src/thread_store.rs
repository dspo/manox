//! The `ThreadStore` facade — the session-list state the sidebar renders.
//!
//! The sidebar's session list comes from the pi session repository (jsonl)
//! plus a per-session UI-metadata sidecar (`manox_harness::session_meta`).
//! The pi transcript persists itself, so `refresh_thread_list` only refreshes
//! the sidebar list; manox SQLite timeline/note records are not produced.
//! Archived sessions are excluded from the sidebar list but stay in
//! `session_paths` so their sidecar remains addressable. The retired manox
//! SQLite-backed implementation was removed; see git history (or the
//! `origin/Manox` backup branch) for it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use std::sync::Arc;

use crate::db::ThreadSummary;
use crate::thread::{PermissionMode, Thread, ThreadCore, ThreadHandle, ThreadId};

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
    /// Threads with an interaction pending a user answer (a parked
    /// thread's question card is not visible, so the sidebar badge is the
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
    live_threads: HashMap<String, std::sync::Weak<ThreadCore>>,
    sessions_dir: PathBuf,
    /// Events buffered under the state lock; [`StoreHandle::with_mut`]
    /// drains and broadcasts them once the mutation closure returns.
    pending_events: Vec<ThreadStoreEvent>,
    /// Sidecar writes queued under the state lock; [`StoreHandle::with_mut`]
    /// drains and dispatches them on the agent runtime once the mutation
    /// closure returns.
    pending_meta_writes: Vec<MetaWrite>,
}

/// One queued sidecar write, drained by [`StoreHandle::with_mut`] once the
/// state lock releases and dispatched on the agent runtime.
struct MetaWrite {
    dir: PathBuf,
    path: PathBuf,
    update: Box<dyn FnOnce(&mut manox_harness::session_meta::SessionMeta) + Send + Sync + 'static>,
}

/// The gpui-free handle to the thread store. Cheap to clone (`Arc`); state
/// lives behind a lock and events broadcast to channel subscribers. This is
/// the kernel-side unit the AgentServer and (transitionally) the frontends
/// hold.
#[derive(Clone)]
pub struct StoreHandle(Arc<StoreCore>);

pub struct StoreCore {
    state: parking_lot::RwLock<ThreadStore>,
    /// Event subscribers. Carries `Arc<ThreadStoreEvent>` for parity with
    /// the `ThreadHandle` channel shape; the event is `Clone`, so the `Arc`
    /// can come off once the consumers settle.
    subscribers: parking_lot::Mutex<Vec<async_channel::Sender<Arc<ThreadStoreEvent>>>>,
}

impl StoreHandle {
    /// Wrap a freshly built [`ThreadStore`].
    pub fn new(thread_store: ThreadStore) -> Self {
        Self(Arc::new(StoreCore {
            state: parking_lot::RwLock::new(thread_store),
            subscribers: parking_lot::Mutex::new(Vec::new()),
        }))
    }

    /// Subscribe to this store's event stream.
    pub fn subscribe(&self) -> async_channel::Receiver<Arc<ThreadStoreEvent>> {
        let (tx, rx) = async_channel::unbounded();
        self.0.subscribers.lock().push(tx);
        rx
    }

    /// Shared-read the state.
    pub fn read<R>(&self, f: impl FnOnce(&ThreadStore) -> R) -> R {
        let state = self.0.state.read();
        f(&state)
    }

    /// Mutate under the write lock, then broadcast the buffered events and
    /// dispatch the queued sidecar writes. Three-phase: lock -> mutate
    /// (collecting `pending_events` / `pending_meta_writes`) -> unlock ->
    /// emit. The closure must never await.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut ThreadStore) -> R) -> R {
        let (r, events, writes) = {
            let mut state = self.0.state.write();
            let r = f(&mut state);
            let events = std::mem::take(&mut state.pending_events);
            let writes = std::mem::take(&mut state.pending_meta_writes);
            (r, events, writes)
        };
        for write in writes {
            self.spawn_meta_write(write);
        }
        self.broadcast(events);
        r
    }

    fn broadcast(&self, events: Vec<ThreadStoreEvent>) {
        if events.is_empty() {
            return;
        }
        let mut subs = self.0.subscribers.lock();
        // Drop subscribers whose receiver is gone (view unmount); otherwise
        // the list grows without bound on a long-lived store.
        subs.retain(|tx| !tx.is_closed());
        if subs.is_empty() {
            return;
        }
        for ev in events {
            let ev = Arc::new(ev);
            for tx in subs.iter() {
                let _ = tx.try_send(ev.clone());
            }
        }
    }

    /// Re-read the session directory and refresh the summary list. Runs on
    /// the agent runtime so a large session folder cannot stall the caller;
    /// `SummariesUpdated` broadcasts when the scan lands.
    pub fn refresh(&self) {
        let dir = self.read(|s| s.sessions_dir.clone());
        let this = self.clone();
        crate::runtime::handle().spawn(async move {
            let rows = load_summaries(&dir).await;
            let registry = crate::thread_registry::load().await;
            this.with_mut(|s| {
                let (session_paths, mut summaries, archived) = group_by_thread(rows, &registry);
                resolve_depths(&mut summaries);
                s.session_paths = session_paths;
                s.summaries = summaries;
                s.archived_summaries = archived;
                s.pending_events.push(ThreadStoreEvent::SummariesUpdated);
            });
        });
    }

    /// Persist one queued sidecar write on the agent runtime. The rescan
    /// follows the write — a rescan racing the write would re-read stale
    /// sidecar flags and revert the in-memory state.
    fn spawn_meta_write(&self, write: MetaWrite) {
        let this = self.clone();
        crate::runtime::handle().spawn(async move {
            let saved = manox_harness::session_meta::update(&write.dir, &write.path, write.update)
                .await
                .is_ok();
            if saved {
                this.refresh();
            }
        });
    }
}

/// `Mutex<Option<_>>` (not a `OnceLock`) so test-support can reset the
/// global between tests.
static GLOBAL: std::sync::Mutex<Option<StoreHandle>> = std::sync::Mutex::new(None);

#[cfg(any(test, feature = "test-support"))]
static TEST_OVERRIDE: std::sync::Mutex<Option<StoreHandle>> = std::sync::Mutex::new(None);

/// Resolve the pi session directory under the manox config dir.
pub(crate) fn sessions_dir() -> PathBuf {
    crate::paths::manox_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("sessions")
}

/// Open the session directory, seed the summary list, and register the
/// process-global handle. Call at App startup.
pub fn init() {
    let dir = sessions_dir();
    let db_path = crate::db::default_db_path().expect("Failed to resolve threads.db path");
    let db = Arc::new(
        crate::db::ThreadsDatabase::open(&db_path)
            .unwrap_or_else(|e| panic!("Failed to open threads db ({}): {e}", db_path.display())),
    );
    let known_projects = db.list_projects().unwrap_or_default();
    let handle = StoreHandle::new(ThreadStore {
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
        pending_events: Vec::new(),
        pending_meta_writes: Vec::new(),
    });
    handle.refresh();
    *GLOBAL.lock().unwrap() = Some(handle);
}

/// Returns the global [`StoreHandle`]. Panics if `init` was not called.
pub fn global() -> StoreHandle {
    try_global().expect("ThreadStore not initialized; call manox_agent::init first")
}

/// The global store when initialized (`manox_agent::init`, or `init_for_test`);
/// `None` before init so teardown paths (team disband) can skip archival
/// instead of panicking in store-less environments.
pub fn try_global() -> Option<StoreHandle> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(handle) = TEST_OVERRIDE.lock().unwrap().clone() {
        return Some(handle);
    }
    GLOBAL.lock().unwrap().clone()
}

/// Drop the global store handle — test-support only, so a test can tear down
/// without the process-global slot leaking into other tests.
#[cfg(any(test, feature = "test-support"))]
pub fn drop_global_for_test() {
    *GLOBAL.lock().unwrap() = None;
}

impl ThreadStore {
    pub fn summaries(&self) -> &[ThreadSummary] {
        &self.summaries
    }

    /// The shared threads.db handle, for UI-layer per-thread state that
    /// piggybacks on the store's single connection (right-pane snapshots).
    pub fn db(&self) -> &std::sync::Arc<crate::db::ThreadsDatabase> {
        &self.db
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
    pub fn register_project(&mut self, path: String) {
        if path.is_empty() || self.known_projects.contains(&path) {
            return;
        }
        self.known_projects.push(path.clone());
        if let Err(e) = self.db.register_project(&path) {
            tracing::warn!(error = %e, "failed to persist project registration");
        }
        self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
    }

    /// Unregister a project path: the sidebar folder disappears and threads
    /// bound to the path fall back to the loose Conversations list. The
    /// conversation history itself is never touched. No-op for an unknown
    /// path.
    pub fn remove_project(&mut self, path: &str) {
        if !self.known_projects.iter().any(|p| p == path) {
            return;
        }
        self.known_projects.retain(|p| p != path);
        if let Err(e) = self.db.remove_project(path) {
            tracing::warn!(error = %e, "failed to persist project removal");
        }
        self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
    }

    /// Whether the given thread id is currently running a turn.
    pub fn is_running(&self, id: &str) -> bool {
        self.running.contains(id)
    }

    /// Mark a thread as running (turn started).
    pub fn mark_running(&mut self, id: &str) {
        if self.running.insert(id.to_string()) {
            self.pending_events.push(ThreadStoreEvent::RunningChanged);
        }
    }

    /// Mark a thread as idle (turn ended).
    pub fn mark_idle(&mut self, id: &str) {
        if self.running.remove(id) {
            self.pending_events.push(ThreadStoreEvent::RunningChanged);
        }
    }

    /// Set the unread flag on a session (persisted in its sidecar).
    pub fn set_unread(&mut self, id: &str, unread: bool) {
        if let Some(s) = self.summary_mut(id)
            && s.has_unread == unread
        {
            return;
        }
        if let Some(s) = self.summary_mut(id) {
            s.has_unread = unread;
        }
        self.write_meta(id, move |meta| meta.unread = unread);
    }

    /// Whether a thread has a tool authorization pending a user verdict.
    pub fn pending_auth_contains(&self, id: &str) -> bool {
        self.pending_auth.contains(id)
    }

    /// Mark/unmark a thread as awaiting a tool-authorization verdict. Fires
    /// `SummariesUpdated` so the sidebar badge appears without waiting for a
    /// rescan. In-memory only — the badge is a live-state signal, never
    /// persisted.
    pub fn mark_pending_auth(&mut self, id: &str, pending: bool) {
        let changed = if pending {
            self.pending_auth.insert(id.to_string())
        } else {
            self.pending_auth.remove(id)
        };
        if changed {
            self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
        }
    }

    /// Whether a thread's turn is parked on a plan-review verdict.
    pub fn pending_plan_contains(&self, id: &str) -> bool {
        self.pending_plan.contains(id)
    }

    /// Mark/unmark a thread as awaiting a plan-review verdict. Same lifecycle
    /// and event as `mark_pending_auth`: the sidebar's blue static icon (not
    /// the spinner) signals the wait until the user decides.
    pub fn mark_pending_plan(&mut self, id: &str, pending: bool) {
        let changed = if pending {
            self.pending_plan.insert(id.to_string())
        } else {
            self.pending_plan.remove(id)
        };
        if changed {
            self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
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
    pub fn mark_background_work(&mut self, id: &str, active: bool) {
        let changed = if active {
            self.background_work.insert(id.to_string())
        } else {
            self.background_work.remove(id)
        };
        if changed {
            self.pending_events.push(ThreadStoreEvent::RunningChanged);
        }
    }

    /// Set the errored flag on a session (persisted in its sidecar).
    pub fn set_errored(&mut self, id: &str, errored: bool) {
        if let Some(s) = self.summary_mut(id)
            && s.errored == errored
        {
            return;
        }
        if let Some(s) = self.summary_mut(id) {
            s.errored = errored;
        }
        self.write_meta(id, move |meta| meta.errored = errored);
    }

    /// Load and restore a `Thread` by id (model resolved from the registry).
    pub fn load_thread(&mut self, id: &str) -> Option<ThreadHandle> {
        if let Some(weak) = self.live_threads.get(id)
            && let Some(handle) = ThreadHandle::upgrade(weak)
        {
            return Some(handle);
        }
        let path = self.session_paths.get(id)?.clone();
        let cwd = self
            .summary_by_id(id)
            .map(|s| PathBuf::from(s.project.clone()))
            .unwrap_or_else(|| PathBuf::from("."));
        let handle = Thread::open_existing(ThreadId(id.to_string()), cwd, path);
        // Re-surface the bound project from the sidecar so the chip shows it.
        if let Some(sum) = self.summary_by_id(id)
            && !sum.project.is_empty()
        {
            let dir = PathBuf::from(&sum.project);
            handle.with_mut(|t| t.restore_project(dir));
        }
        self.live_threads.insert(id.to_string(), handle.downgrade());
        Some(handle)
    }

    /// The live (in-memory) thread for `id`, if it is still alive. Unlike
    /// [`ThreadStore::load_thread`], never restores from disk.
    pub fn live_thread(&self, id: &str) -> Option<ThreadHandle> {
        self.live_threads.get(id).and_then(ThreadHandle::upgrade)
    }

    /// Track a live thread so the facade can address it by id alone. Stores
    /// only a weak reference; the caller keeps the thread alive.
    pub fn register_live_thread(&mut self, id: &str, t: &ThreadHandle) {
        self.live_threads.insert(id.to_string(), t.downgrade());
    }

    /// Seed an active summary row without touching disk — lets foreign test
    /// modules exercise the archive cascade against real thread ids.
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_summary_for_test(&mut self, id: &str, parent: Option<&str>) {
        self.summaries.push(crate::db::ThreadSummary {
            id: id.to_string(),
            summary: id.to_string(),
            title: None,
            title_override: None,
            model_id: String::new(),
            provider_id: None,
            approval_mode: PermissionMode::default().as_i64(),
            project: String::new(),
            depth: parent.is_some() as i32,
            parent_id: parent.map(str::to_string),
            archived: false,
            pinned: false,
            tag: None,
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
    pub fn archive_thread(&mut self, id: &str, archived: bool) {
        if self
            .summary_by_id(id)
            .is_some_and(|s| s.archived == archived)
        {
            return;
        }
        let ids = if archived {
            self.descendant_ids(id)
        } else {
            vec![id.to_string()]
        };
        for tid in ids {
            // A row already at the target state (e.g. archived by the
            // caller's disband earlier) skips move + meta + hook: one
            // SessionEnd per working life.
            if self
                .summary_by_id(&tid)
                .is_some_and(|s| s.archived == archived)
            {
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
            self.write_meta(&tid, move |meta| meta.archived = archived);
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
    pub fn pin_thread(&mut self, id: &str, pinned: bool) {
        if let Some(s) = self.summary_mut(id) {
            s.pinned = pinned;
        }
        self.write_meta(id, move |meta| meta.pinned = pinned);
    }

    /// Set the user tag on a session (persisted in its sidecar); `None`
    /// removes it. Re-asserting the current value is a no-op — no sidecar
    /// write, no rescan.
    pub fn set_thread_tag(&mut self, id: &str, tag: Option<String>) {
        if let Some(s) = self.summary_mut(id)
            && s.tag == tag
        {
            return;
        }
        if let Some(s) = self.summary_mut(id) {
            s.tag = tag.clone();
        }
        self.write_meta(id, move |meta| meta.tag = tag);
    }

    /// Append a `model_change` event. The pi transcript records model changes
    /// itself; nothing to do here.
    pub fn record_model_change(&self, _thread_id: &str, _from: Option<&str>, _to: &str) {}

    /// Append a typed event to the thread's timeline. The manox SQLite
    /// timeline is not produced by the pi backend.
    pub fn record_event(
        &self,
        _thread_id: &str,
        _event_type: crate::db::ThreadEventType,
        _data: &serde_json::Value,
    ) {
    }

    /// Queue a sidecar change for a session. The caller's in-memory update
    /// is the render source of truth, so `SummariesUpdated` fires up front;
    /// the write itself is queued for [`StoreHandle::with_mut`] to dispatch
    /// on the agent runtime once the state lock releases (best-effort).
    fn write_meta(
        &mut self,
        id: &str,
        update: impl FnOnce(&mut manox_harness::session_meta::SessionMeta) + Send + Sync + 'static,
    ) {
        self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
        let Some(path) = self.session_paths.get(id).cloned() else {
            return;
        };
        self.pending_meta_writes.push(MetaWrite {
            dir: self.sessions_dir.clone(),
            path,
            update: Box::new(update),
        });
    }
}

/// Refresh the sidebar summary list from the store (new threads surface at
/// send time, not at turn end). The transcript and its UI annotation entries
/// persist themselves; nothing else rides this path.
pub fn refresh_thread_list() {
    global().refresh();
}

/// One loaded session: the sidebar summary plus the grouping input — the
/// header's owning-thread stamp.
#[derive(Clone)]
struct SessionRow {
    summary: ThreadSummary,
    path: PathBuf,
    thread_key: Option<String>,
}

/// Read every session plus its sidecar into raw rows; grouping into
/// thread-level rows happens in [`group_by_thread`] and team depths in
/// [`resolve_depths`] afterwards.
async fn load_summaries(dir: &std::path::Path) -> Vec<SessionRow> {
    let repo = manox_harness::session::repository::SessionRepository::new(dir);
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
        // Subagent transcripts persist for usage accounting but never
        // surface as threads.
        if info
            .metadata
            .as_ref()
            .is_some_and(|m| m.get("subagent").is_some())
        {
            continue;
        }
        let meta = match manox_harness::session_meta::load(dir, &info.path).await {
            Ok(meta) => meta,
            Err(error) => {
                tracing::warn!(session = %info.id, error = %error, "session sidecar unreadable; rendering default flags");
                manox_harness::session_meta::SessionMeta::default()
            }
        };
        // The owning thread's id rides the header metadata (stamped at
        // creation, inherited by forks); absent on legacy files.
        let thread_key = info
            .metadata
            .as_ref()
            .and_then(|m| m.get("thread"))
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        out.push(SessionRow {
            summary: session_info_to_summary(&info, &meta),
            path: info.path.clone(),
            thread_key,
        });
    }
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
fn resolve_depths(list: &mut [ThreadSummary]) {
    let parents: HashMap<String, Option<String>> = list
        .iter()
        .map(|s| (s.id.clone(), s.parent_id.clone()))
        .collect();
    for sum in list.iter_mut() {
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

/// Collapse each thread's sessions into the single row the user sees: a
/// thread IS the sidebar unit, its sessions (base + historical
/// worktree-era forks) internal
/// storage. The surfaced session is the registry's active pointer when it
/// hits, else the newest; the row carries the THREAD's id (stable across
/// swaps and restarts) and the active session's fields. Sessions without a
/// thread stamp (legacy files) pass through as singleton rows keyed by
/// their own id. Returns the id→active-session-path map (every surfaced row
/// stays addressable for `load_thread` and sidecar flag writes), the active
/// rows, and the archived rows.
fn group_by_thread(
    rows: Vec<SessionRow>,
    registry: &HashMap<String, crate::thread_registry::ThreadRegistryEntry>,
) -> (
    HashMap<String, PathBuf>,
    Vec<ThreadSummary>,
    Vec<ThreadSummary>,
) {
    // Session id (the `<id>.jsonl` stem) → row index, and the session→thread
    // map used to remap team edges from session ids to thread keys.
    let mut by_session: HashMap<String, usize> = HashMap::new();
    let mut session_to_thread: HashMap<String, String> = HashMap::new();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, row) in rows.iter().enumerate() {
        let session_id = row
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        by_session.insert(session_id, i);
        if let Some(key) = &row.thread_key {
            session_to_thread.insert(row.summary.id.clone(), key.clone());
            groups.entry(key.clone()).or_default().push(i);
        }
    }
    let mut session_paths: HashMap<String, PathBuf> = HashMap::new();
    let mut active = Vec::new();
    let mut archived = Vec::new();
    let mut consumed: HashSet<usize> = HashSet::new();
    for (thread_key, members) in groups {
        let pointed = registry
            .get(&thread_key)
            .and_then(|entry| by_session.get(&entry.active_session))
            .copied()
            .filter(|i| members.contains(i));
        let chosen = pointed.unwrap_or_else(|| {
            members
                .iter()
                .max_by_key(|i| rows[**i].summary.interacted_at)
                .copied()
                .expect("a group is never empty")
        });
        let chosen_row = &rows[chosen];
        let mut sum = chosen_row.summary.clone();
        let path = chosen_row.path.clone();
        consumed.extend(members.iter().copied());
        // The row IS the thread: stable id across session swaps, team edge
        // remapped from the leader's SESSION id to the leader's THREAD key
        // (unresolvable legacy edges keep their raw value).
        sum.id = thread_key.clone();
        if let Some(parent_session) = sum.parent_id.clone()
            && let Some(leader_thread) = session_to_thread.get(&parent_session)
        {
            sum.parent_id = Some(leader_thread.clone());
        }
        // Grouping re-keys the row by thread id, so any prior depth is
        // stale; `resolve_depths` re-derives it from the remapped edges.
        sum.depth = 0;
        session_paths.insert(thread_key, path.clone());
        if sum.archived {
            archived.push(sum);
        } else {
            active.push(sum);
        }
    }
    for (i, row) in rows.into_iter().enumerate() {
        if consumed.contains(&i) {
            continue;
        }
        session_paths.insert(row.summary.id.clone(), row.path);
        if row.summary.archived {
            archived.push(row.summary);
        } else {
            active.push(row.summary);
        }
    }
    (session_paths, active, archived)
}

/// The team leader's session id from a session header's `team.parent`, when
/// present. Shared by the sidebar store and the actor's mirrored session
/// list so both resolve the affiliation identically.
pub(crate) fn team_parent_id(
    info: &manox_harness::session::repository::SessionInfo,
) -> Option<String> {
    info.metadata
        .as_ref()
        .and_then(|m| m.get("team"))
        .and_then(|t| t.get("parent"))
        .and_then(|p| p.as_str())
        .map(str::to_string)
}
/// Map a pi session info + sidecar onto the sidebar summary shape.
fn session_info_to_summary(
    info: &manox_harness::session::repository::SessionInfo,
    meta: &manox_harness::session_meta::SessionMeta,
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
        approval_mode: PermissionMode::default().as_i64(),
        // The bound project (sidecar) wins over the header cwd: a fork's
        // header cwd may be another directory, but the thread stays under
        // its source project; a `/` header cwd (GUI-launched bound session)
        // classifies the same way.
        project: meta
            .project
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| info.cwd.clone()),
        depth: 0,
        // Team affiliation is the only rendered hierarchy edge: historical fork
        // forks are a thread's internal sessions (`group_by_thread`
        // collapses them), so their `parentSession` lineage stays raw
        // metadata and never nests.
        parent_id: team_parent_id(info).or_else(|| info.parent_session_path.clone()),
        archived: meta.archived,
        pinned: meta.pinned,
        tag: meta.tag.clone(),
        has_unread: meta.unread,
        errored: meta.errored,
        created_at: info.created_at.timestamp(),
        interacted_at: info.modified_at.timestamp(),
        updated_at: info.modified_at.timestamp(),
        cumulative_total_tokens: 0,
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn init_for_test(db: Arc<crate::db::ThreadsDatabase>) {
    let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    let handle = StoreHandle::new(ThreadStore {
        summaries: Vec::new(),
        archived_summaries: Vec::new(),
        session_paths: HashMap::new(),
        known_projects: Vec::new(),
        db,
        running: HashSet::new(),
        pending_auth: HashSet::new(),
        pending_plan: HashSet::new(),
        background_work: HashSet::new(),
        live_threads: HashMap::new(),
        sessions_dir: dir,
        pending_events: Vec::new(),
        pending_meta_writes: Vec::new(),
    });
    *TEST_OVERRIDE.lock().unwrap() = Some(handle);
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

    fn temp_db() -> (
        std::sync::Arc<crate::db::ThreadsDatabase>,
        std::path::PathBuf,
    ) {
        let path = std::env::temp_dir().join(format!("pi-store-test-{}.db", uuid::Uuid::new_v4()));
        let db = std::sync::Arc::new(
            crate::db::ThreadsDatabase::open(&path).expect("open temp threads db"),
        );
        (db, path)
    }

    fn store_handle(db: Arc<crate::db::ThreadsDatabase>) -> StoreHandle {
        let known_projects = db.list_projects().unwrap_or_default();
        StoreHandle::new(ThreadStore {
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
            pending_events: Vec::new(),
            pending_meta_writes: Vec::new(),
        })
    }

    #[test]
    fn register_project_persists_and_survives_reopen() {
        let (db, path) = temp_db();
        let store = store_handle(db.clone());
        store.with_mut(|s| s.register_project("/p/a".into()));
        // Persisted to the db...
        assert!(db.list_projects().unwrap().contains(&"/p/a".to_string()));
        // ...and a freshly initialized store (simulated restart) sees it.
        let reopened = store_handle(db.clone());
        let known = reopened.read(|s| s.known_projects().to_vec());
        assert_eq!(known, vec!["/p/a".to_string()]);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn register_project_dedupes() {
        let (db, path) = temp_db();
        let store = store_handle(db.clone());
        store.with_mut(|s| {
            s.register_project("/p/a".into());
            s.register_project("/p/a".into());
            s.register_project(String::new());
        });
        let known = store.read(|s| s.known_projects().to_vec());
        assert_eq!(known, vec!["/p/a".to_string()]);
        assert_eq!(db.list_projects().unwrap().len(), 1);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn remove_project_persists_and_survives_reopen() {
        let (db, path) = temp_db();
        let store = store_handle(db.clone());
        store.with_mut(|s| {
            s.register_project("/p/a".into());
            s.register_project("/p/b".into());
            s.remove_project("/p/a");
            // Removing an unknown path is a no-op.
            s.remove_project("/p/missing");
        });
        // Persisted to the db...
        assert_eq!(db.list_projects().unwrap(), vec!["/p/b".to_string()]);
        let known = store.read(|s| s.known_projects().to_vec());
        assert_eq!(known, vec!["/p/b".to_string()]);
        // ...and a freshly initialized store (simulated restart) sees it.
        let reopened = store_handle(db.clone());
        let known = reopened.read(|s| s.known_projects().to_vec());
        assert_eq!(known, vec!["/p/b".to_string()]);
        std::fs::remove_file(path).ok();
    }

    /// The pending-auth badge marker toggles per thread id and only emits
    /// `SummariesUpdated` on an actual state change.
    #[test]
    fn mark_pending_auth_toggles_marker() {
        let (db, path) = temp_db();
        let store = store_handle(db.clone());
        let events = store.subscribe();
        store.with_mut(|s| s.mark_pending_auth("t1", true));
        assert!(store.read(|s| s.pending_auth_contains("t1")));
        assert_eq!(events.len(), 1);
        // Idempotent mark: no event, no duplicate work.
        store.with_mut(|s| s.mark_pending_auth("t1", true));
        assert_eq!(events.len(), 1);
        store.with_mut(|s| s.mark_pending_auth("t1", false));
        assert!(!store.read(|s| s.pending_auth_contains("t1")));
        assert_eq!(events.len(), 2);
        std::fs::remove_file(path).ok();
    }

    /// The running-set marker (the sidebar spinner source) toggles per thread
    /// id, fires `RunningChanged` only on an actual state change, and is
    /// idempotent under repeated marks — the store contract every host
    /// subscription (foreground, parked, actor) relies on.
    #[test]
    fn mark_running_toggles_marker() {
        let (db, path) = temp_db();
        let store = store_handle(db.clone());
        let events = store.subscribe();
        store.with_mut(|s| s.mark_running("t1"));
        assert!(store.read(|s| s.is_running("t1")));
        assert_eq!(events.len(), 1);
        // Idempotent mark: no event, no duplicate work.
        store.with_mut(|s| s.mark_running("t1"));
        assert_eq!(events.len(), 1);
        // A second thread marks independently.
        store.with_mut(|s| s.mark_running("t2"));
        assert!(store.read(|s| s.is_running("t2")));
        assert_eq!(events.len(), 2);
        store.with_mut(|s| s.mark_idle("t1"));
        assert!(!store.read(|s| s.is_running("t1")));
        assert!(store.read(|s| s.is_running("t2")));
        assert_eq!(events.len(), 3);
        std::fs::remove_file(path).ok();
    }

    /// The plan-review and background-work markers (the blue-static vs
    /// spinner distinction) toggle per thread id and are idempotent under
    /// repeated marks.
    #[test]
    fn plan_and_background_markers_toggle() {
        let (db, path) = temp_db();
        let store = store_handle(db.clone());
        let events = store.subscribe();
        store.with_mut(|s| {
            s.mark_pending_plan("t1", true);
            s.mark_background_work("t1", true);
        });
        assert!(store.read(|s| s.pending_plan_contains("t1")));
        assert!(store.read(|s| s.background_work_contains("t1")));
        assert_eq!(events.len(), 2);
        // Idempotent marks: no duplicate events.
        store.with_mut(|s| {
            s.mark_pending_plan("t1", true);
            s.mark_background_work("t1", true);
        });
        assert_eq!(events.len(), 2);
        // A second thread marks independently; clearing only removes its own.
        store.with_mut(|s| s.mark_pending_plan("t2", true));
        assert_eq!(events.len(), 3);
        store.with_mut(|s| {
            s.mark_pending_plan("t1", false);
            s.mark_background_work("t1", false);
        });
        assert!(!store.read(|s| s.pending_plan_contains("t1")));
        assert!(!store.read(|s| s.background_work_contains("t1")));
        assert!(store.read(|s| s.pending_plan_contains("t2")));
        assert_eq!(events.len(), 5);
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
            approval_mode: PermissionMode::default().as_i64(),
            project: String::new(),
            depth: 0,
            parent_id: None,
            archived,
            pinned: false,
            tag: None,
            has_unread: false,
            errored: false,
            created_at: 0,
            interacted_at: 0,
            updated_at: 0,
            cumulative_total_tokens: 0,
        }
    }

    fn sample_row(
        id: &str,
        thread_key: Option<&str>,
        interacted_at: i64,
        archived: bool,
    ) -> SessionRow {
        let mut summary = sample_summary(id, archived);
        summary.interacted_at = interacted_at;
        SessionRow {
            summary,
            path: PathBuf::from(format!("{id}.jsonl")),
            thread_key: thread_key.map(str::to_string),
        }
    }

    fn pointer(active_session: &str) -> crate::thread_registry::ThreadRegistryEntry {
        crate::thread_registry::ThreadRegistryEntry {
            active_session: active_session.to_string(),
        }
    }

    #[test]
    fn group_collapses_to_registry_active() {
        let rows = vec![
            sample_row("base", Some("t"), 10, false),
            sample_row("fork", Some("t"), 20, false),
        ];
        let registry = HashMap::from([("t".to_string(), pointer("base"))]);
        let (paths, active, archived) = group_by_thread(rows, &registry);
        assert!(archived.is_empty());
        assert_eq!(active.len(), 1, "one thread = one row");
        assert_eq!(active[0].id, "t", "row keyed by thread id");
        assert_eq!(
            active[0].interacted_at, 10,
            "fields from the ACTIVE session"
        );
        assert_eq!(paths.get("t").cloned(), Some(PathBuf::from("base.jsonl")));
    }

    #[test]
    fn group_falls_back_to_newest_without_pointer() {
        let rows = vec![
            sample_row("base", Some("t"), 10, false),
            sample_row("fork", Some("t"), 20, false),
        ];
        // No registry entry → the newest session surfaces.
        let (paths, active, _) = group_by_thread(rows.clone(), &HashMap::new());
        assert_eq!(active.len(), 1);
        assert_eq!(paths.get("t").cloned(), Some(PathBuf::from("fork.jsonl")));
        // A stale pointer to a foreign session id degrades the same way.
        let stale = HashMap::from([("t".to_string(), pointer("gone"))]);
        let (_, active, _) = group_by_thread(rows, &stale);
        assert_eq!(active.len(), 1);
    }

    #[test]
    fn group_remaps_team_edge_to_thread_id() {
        let leader = sample_row("leader-sess", Some("TL"), 10, false);
        let mut member = sample_row("member-sess", Some("TM"), 10, false);
        member.summary.parent_id = Some("leader-sess".to_string());
        let (paths, active, _) = group_by_thread(vec![leader, member], &HashMap::new());
        assert_eq!(active.len(), 2);
        let member_row = active.iter().find(|s| s.id == "TM").expect("member row");
        assert_eq!(
            member_row.parent_id.as_deref(),
            Some("TL"),
            "team edge remapped session id → thread key"
        );
        assert!(paths.contains_key("TL") && paths.contains_key("TM"));
    }

    #[test]
    fn group_passes_through_unstamped_rows() {
        let rows = vec![
            sample_row("legacy", None, 10, false),
            sample_row("threaded", Some("t"), 20, false),
        ];
        let (paths, active, _) = group_by_thread(rows, &HashMap::new());
        assert_eq!(active.len(), 2);
        assert!(active.iter().any(|s| s.id == "legacy"));
        assert!(active.iter().any(|s| s.id == "t"));
        assert_eq!(
            paths.get("legacy").cloned(),
            Some(PathBuf::from("legacy.jsonl"))
        );
    }

    #[test]
    fn group_partitions_by_active_session_archived_flag() {
        let rows = vec![
            sample_row("base", Some("t"), 10, false),
            sample_row("fork", Some("t"), 20, true),
        ];
        // Pointer on the archived fork → the thread row retires with it.
        let registry = HashMap::from([("t".to_string(), pointer("fork"))]);
        let (_, active, archived) = group_by_thread(rows, &registry);
        assert!(active.is_empty());
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].id, "t");
    }

    /// A thread's real session files (base + worktree fork, both stamped
    /// with the same header `thread` key as the retired worktree fork left them)
    /// collapse to ONE sidebar row keyed by the thread id, following the
    /// registry's active pointer in both directions.
    #[tokio::test]
    async fn grouping_end_to_end_over_real_session_files() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path();
        let (base_id, fork_id, thread_key) = ("base-session", "fork-session", "thread-1");
        let header = |id: &str, cwd: &str| {
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"{cwd}\",\"metadata\":{{\"host\":\"manox\",\"thread\":\"{thread_key}\"}}}}\n"
            )
        };
        tokio::fs::write(
            sessions.join(format!("{base_id}.jsonl")),
            header(base_id, "/proj/a"),
        )
        .await
        .unwrap();
        tokio::fs::write(
            sessions.join(format!("{fork_id}.jsonl")),
            header(fork_id, "/tmp/wt"),
        )
        .await
        .unwrap();
        // Sidecars: the fork wears the source's title/project.
        let base_path = sessions.join(format!("{base_id}.jsonl"));
        let fork_path = sessions.join(format!("{fork_id}.jsonl"));
        manox_harness::session_meta::update(sessions, &base_path, |m| {
            m.title = Some("the title".into());
            m.project = Some("/proj/a".into());
        })
        .await
        .unwrap();
        manox_harness::session_meta::update(sessions, &fork_path, |m| {
            m.title = Some("the title".into());
            m.project = Some("/proj/a".into());
        })
        .await
        .unwrap();

        let rows = load_summaries(sessions).await;
        assert_eq!(rows.len(), 2);

        // Pointer on the fork (inside the worktree): one row, thread-keyed,
        // project stays the source's.
        let registry = HashMap::from([(thread_key.to_string(), pointer(fork_id))]);
        let (paths, active, archived) = group_by_thread(rows.clone(), &registry);
        assert!(archived.is_empty());
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, thread_key);
        assert_eq!(active[0].title.as_deref(), Some("the title"));
        assert_eq!(active[0].project, "/proj/a");
        assert_eq!(paths.get(thread_key).cloned(), Some(fork_path.clone()));

        // Pointer back on the base: same single row,
        // now addressable at the base session.
        let registry = HashMap::from([(thread_key.to_string(), pointer(base_id))]);
        let (paths, active, _) = group_by_thread(rows, &registry);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, thread_key);
        assert_eq!(paths.get(thread_key).cloned(), Some(base_path));
    }

    fn sample_info(
        id: &str,
        metadata: Option<serde_json::Value>,
    ) -> manox_harness::session::repository::SessionInfo {
        let now = chrono::Utc::now();
        manox_harness::session::repository::SessionInfo {
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
            session_info_to_summary(&info, &manox_harness::session_meta::SessionMeta::default());
        assert_eq!(summary.parent_id.as_deref(), Some("leader"));
    }

    #[test]
    fn summary_falls_back_to_fork_parent_without_team_key() {
        let mut info = sample_info("forked", Some(serde_json::json!({ "host": "manox" })));
        info.parent_session_path = Some("source".to_string());
        let summary =
            session_info_to_summary(&info, &manox_harness::session_meta::SessionMeta::default());
        assert_eq!(summary.parent_id.as_deref(), Some("source"));
    }

    #[test]
    fn summary_project_prefers_sidecar_over_cwd() {
        let info = sample_info("s", None);
        let meta = manox_harness::session_meta::SessionMeta {
            project: Some("/proj/a".into()),
            ..Default::default()
        };
        let summary = session_info_to_summary(&info, &meta);
        assert_eq!(summary.project, "/proj/a");
        // Without a bound project the header cwd classifies the row.
        let default = manox_harness::session_meta::SessionMeta::default();
        let summary = session_info_to_summary(&info, &default);
        assert_eq!(summary.project, "/p");
    }

    #[test]
    fn resolve_depths_nests_chains_and_degrades_orphans() {
        let mut list = vec![
            sample_summary("a", false),
            sample_summary("b", false),
            sample_summary("c", false),
            sample_summary("orphan", false),
        ];
        list[1].parent_id = Some("a".into());
        list[2].parent_id = Some("b".into());
        list[3].parent_id = Some("gone".into());
        resolve_depths(&mut list);
        let depths: Vec<(String, i32)> = list.iter().map(|s| (s.id.clone(), s.depth)).collect();
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
        let mut cycle = vec![sample_summary("a", false), sample_summary("b", false)];
        cycle[0].parent_id = Some("b".into());
        cycle[1].parent_id = Some("a".into());
        resolve_depths(&mut cycle);
        assert_eq!(cycle[0].depth, 0);
        assert_eq!(cycle[1].depth, 0);

        // A chain longer than the cap is malformed: rows whose own depth
        // would exceed the cap degrade to top-level, while rows at or under
        // the cap keep their valid nesting.
        let mut chain: Vec<ThreadSummary> = (0..=MAX_TEAM_DEPTH + 1)
            .map(|i| sample_summary(&format!("n{i}"), false))
            .collect();
        for (i, item) in chain
            .iter_mut()
            .enumerate()
            .skip(1)
            .take(MAX_TEAM_DEPTH + 1)
        {
            item.parent_id = Some(format!("n{}", i - 1));
        }
        resolve_depths(&mut chain);
        assert_eq!(chain[MAX_TEAM_DEPTH + 1].depth, 0, "over-cap row degrades");
        assert_eq!(
            chain[MAX_TEAM_DEPTH].depth, MAX_TEAM_DEPTH as i32,
            "at-cap row keeps depth"
        );
        assert_eq!(chain[0].depth, 0);
    }

    /// The `/exit` flow archives a session and, in the same instant, a
    /// thread attach writes another sidecar field — two back-to-back sidecar
    /// writes on the same session. Serialized writes must keep `archived` on
    /// disk while the second write lands (the lost-update that resurrected
    /// archived conversations).
    #[test]
    fn archive_survives_concurrent_pinned_write() {
        let (db, db_path) = temp_db();
        crate::runtime::init();
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("t1.jsonl");
        let store = store_handle(db.clone());
        store.with_mut(|s| {
            s.session_paths.insert("t1".to_string(), session.clone());
            s.sessions_dir = dir.path().to_path_buf();
        });
        store.with_mut(|s| {
            s.archive_thread("t1", true);
            s.write_meta("t1", |meta| meta.pinned = true);
        });
        let mut settled = false;
        // Generous budget: the two sidecar writes run on the process-wide
        // tokio runtime shared with every parallel test in the binary — a
        // loaded CI runner starves them well past the 2s the writes need in
        // isolation. The assertion still catches the lost-update regression;
        // it just doesn't double as a scheduler benchmark.
        for _ in 0..1500 {
            if let Ok(meta) = crate::runtime::handle()
                .block_on(manox_harness::session_meta::load(dir.path(), &session))
                && meta.archived
                && meta.pinned
            {
                settled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(settled, "archived flag lost to a concurrent pinned write");
        std::fs::remove_file(db_path).ok();
    }

    /// The user tag round-trips through the sidecar: the in-memory summary
    /// flips immediately (the render source of truth) and the persisted
    /// sidecar follows; clearing lands `None`.
    #[test]
    fn set_thread_tag_persists_to_sidecar() {
        let (db, db_path) = temp_db();
        crate::runtime::init();
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("t1.jsonl");
        // A real session file so the post-write rescan keeps the row (and
        // its `session_paths` entry) addressable for follow-up writes.
        std::fs::write(
            &session,
            "{\"type\":\"session\",\"version\":3,\"id\":\"t1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/p\",\"metadata\":{\"host\":\"manox\"}}\n",
        )
        .unwrap();
        let store = store_handle(db.clone());
        store.with_mut(|s| {
            s.session_paths.insert("t1".to_string(), session.clone());
            s.sessions_dir = dir.path().to_path_buf();
            s.insert_summary_for_test("t1", None);
        });
        store.with_mut(|s| s.set_thread_tag("t1", Some("urgent".into())));
        // In-memory flip is immediate.
        store.read(|s| {
            assert_eq!(
                s.summary_by_id("t1").and_then(|s| s.tag.clone()),
                Some("urgent".into())
            );
        });
        let wait_for = |expected: Option<&str>| {
            for _ in 0..1500 {
                if let Ok(meta) = crate::runtime::handle()
                    .block_on(manox_harness::session_meta::load(dir.path(), &session))
                    && meta.tag.as_deref() == expected
                {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            false
        };
        assert!(wait_for(Some("urgent")), "tag never reached the sidecar");
        store.with_mut(|s| s.set_thread_tag("t1", None));
        assert!(wait_for(None), "cleared tag never reached the sidecar");
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
            approval_mode: PermissionMode::default().as_i64(),
            project: String::new(),
            depth: parent.is_some() as i32,
            parent_id: parent.map(str::to_string),
            archived: false,
            pinned: false,
            tag: None,
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
        let store = store_handle(db);
        store.with_mut(|s| {
            s.summaries.push(cascade_summary("lead", None));
            s.summaries.push(cascade_summary("member", Some("lead")));
            s.summaries.push(cascade_summary("grand", Some("member")));
            s.summaries.push(cascade_summary("sibling", None));
        });
        store.with_mut(|s| s.archive_thread("lead", true));
        store.read(|s| {
            // lead + member + grand archived; unrelated row untouched.
            assert_eq!(s.summaries.len(), 1);
            assert_eq!(s.summaries[0].id, "sibling");
            let archived: Vec<&str> = s.archived_summaries.iter().map(|s| s.id.as_str()).collect();
            for id in ["lead", "member", "grand"] {
                assert!(archived.contains(&id), "{id} not archived");
            }
            assert!(s.archived_summaries.iter().all(|s| s.archived));
        });
        std::fs::remove_file(db_path).ok();
    }

    /// Unarchiving moves only the requested row; descendants stay archived.
    #[test]
    fn unarchive_does_not_cascade() {
        let (db, db_path) = temp_db();
        let store = store_handle(db);
        store.with_mut(|s| {
            s.summaries.push(cascade_summary("lead", None));
            s.summaries.push(cascade_summary("member", Some("lead")));
        });
        store.with_mut(|s| s.archive_thread("lead", true));
        store.with_mut(|s| s.archive_thread("lead", false));
        store.read(|s| {
            assert!(s.summaries.iter().any(|s| s.id == "lead" && !s.archived));
            assert!(
                s.archived_summaries
                    .iter()
                    .any(|s| s.id == "member" && s.archived)
            );
        });
        std::fs::remove_file(db_path).ok();
    }
}
