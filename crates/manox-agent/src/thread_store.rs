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
//!
//! A refresh is a RECONCILE, not a scan: a `read_dir` + `stat` sweep
//! fingerprints every session file (and its sidecar); files whose append-only
//! coverage still holds reuse the cached bounded facts (`session_index` hot
//! map + db table), and only the new or shrunk ones take a bounded read
//! (header + first user message — never the transcript). Latency grows with
//! CHANGES, not with history: a 40 MiB session being actively appended is
//! never re-read.
//!
//! Two invariants hold the sidebar still. The row order is the durable manual
//! account in [`crate::sidebar_order`] — never a timestamp sort — so an
//! activity-driven rescan cannot move a row. And `interacted_at` is the last
//! human prompt or steer (the sidecar's stamp), so no assistant output, tool
//! result or metadata write advances it.

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
    /// Archived rows, partitioned out of `summaries` so the sidebar list
    /// stays clean while surfaces can still render them on demand.
    archived_summaries: Vec<ThreadSummary>,
    /// Session file path per summary id, for sidecar writes and reopen.
    session_paths: HashMap<String, PathBuf>,
    known_projects: Vec<String>,
    /// The durable sidebar order: folder sequence plus one thread account per
    /// partition. Authoritative over `summaries`' order — see
    /// [`crate::sidebar_order`].
    order: crate::sidebar_order::SidebarOrder,
    /// Set when `order` diverged from its persisted form; `with_mut`'s drain
    /// writes and clears it outside the state lock.
    order_dirty: bool,
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
    /// Decisions newer than their sidecar write: id → (pinned, archived).
    /// A flag decision flips the in-memory mirror immediately but persists
    /// asynchronously (the queued write lands inside the next refresh
    /// pass) — a pass that started scanning before the write landed must
    /// not publish the pre-decision state. The overlay wins until a scan
    /// OBSERVES the decided pair on disk, then drops (write confirmed).
    decision_overlay: HashMap<String, (bool, bool)>,
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
    /// The session-scan cache (hot layer; the db `session_index` table is
    /// the cold layer). A refresh reconciles stat fingerprints against it —
    /// steady state reads NO session content, only `read_dir` + `stat`.
    index: tokio::sync::Mutex<SessionIndex>,
    /// Sidecar writes drained INSIDE the refresh pass, before the scan:
    /// a burst of flag decisions lands in dispatch order and ONE reconcile
    /// reads the settled sidecars — a scan can never publish the pre-write
    /// state and revert a live in-memory decision mid-burst.
    meta_queue: parking_lot::Mutex<Vec<MetaWrite>>,
    /// Single-flight gate for scans; see [`StoreHandle::refresh`].
    refresh_gate: tokio::sync::Mutex<()>,
    /// Set while a runner task owns the gate loop — the spawn decision.
    refresh_running: std::sync::atomic::AtomicBool,
    /// Set by a caller that asked for a scan while the runner was in the
    /// air; the runner's loop tail serves it with one more pass.
    refresh_pending: std::sync::atomic::AtomicBool,
}

impl StoreHandle {
    /// Wrap a freshly built [`ThreadStore`].
    pub fn new(thread_store: ThreadStore) -> Self {
        Self(Arc::new(StoreCore {
            state: parking_lot::RwLock::new(thread_store),
            subscribers: parking_lot::Mutex::new(Vec::new()),
            index: tokio::sync::Mutex::new(SessionIndex::default()),
            meta_queue: parking_lot::Mutex::new(Vec::new()),
            refresh_gate: tokio::sync::Mutex::new(()),
            refresh_running: std::sync::atomic::AtomicBool::new(false),
            refresh_pending: std::sync::atomic::AtomicBool::new(false),
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
    /// dispatch the queued sidecar writes and (when the account moved) the
    /// sidebar order. Three-phase: lock -> mutate (collecting `pending_events`
    /// / `pending_meta_writes` / the dirty order snapshot) -> unlock -> emit.
    /// The closure must never await.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut ThreadStore) -> R) -> R {
        let (r, events, writes, order_write) = {
            let mut state = self.0.state.write();
            let r = f(&mut state);
            let events = std::mem::take(&mut state.pending_events);
            let writes = std::mem::take(&mut state.pending_meta_writes);
            let order_write = if state.order_dirty {
                state.order_dirty = false;
                Some(state.order.clone())
            } else {
                None
            };
            (r, events, writes, order_write)
        };
        for write in writes {
            self.queue_meta_write(write);
        }
        if let Some(order) = order_write {
            // No runtime (a bare unit test, or teardown) drops the write rather
            // than panicking: the account is best-effort UI truth, and the
            // in-memory order stays correct for the running process.
            if let Some(handle) = crate::runtime::try_handle() {
                handle.spawn(async move {
                    if let Err(error) = crate::sidebar_order::save(&order).await {
                        tracing::warn!(
                            error = %error,
                            "failed to persist the sidebar order account"
                        );
                    }
                });
            }
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
    /// `SummariesUpdated` broadcasts when the scan lands. Single-flight
    /// with trailing-edge coalescing: while a scan is in the air, later
    /// callers only mark it pending — one reconcile serves the whole burst
    /// (a sidecar-write storm asks once, not once per write).
    pub fn refresh(&self) {
        // Single-flight spawn with a Dekker handshake: write the ask
        // (pending) BEFORE reading the gate (running). The runner's exit
        // publishes quiescence in the mirrored order (store running=false,
        // then re-read pending), so under SeqCst at least one side always
        // sees the other's store — a caller that reads running=true is
        // guaranteed the runner's trailing re-check sees its pending. The
        // reverse order (read gate, then write ask) left a window where
        // both sides walked away and the ask was lost.
        self.0
            .refresh_pending
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if !self
            .0
            .refresh_running
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            // Claimed the gate: become the runner. The pass clears pending
            // at its head, so our own ask rides this run.
            let this = self.clone();
            crate::runtime::handle().spawn(async move {
                this.refresh_coalesced().await;
            });
        }
    }

    /// The gate discipline: each pass lands every queued sidecar write (in
    /// dispatch order) BEFORE the reconcile, so the scan always reads
    /// settled sidecars; one more pass runs iff someone asked meanwhile;
    /// stop when quiet. The trailing re-check closes the lost-wakeup window
    /// between the loop exit and the quiescence publish.
    async fn refresh_coalesced(&self) {
        let _guard = self.0.refresh_gate.lock().await;
        loop {
            self.0
                .refresh_pending
                .store(false, std::sync::atomic::Ordering::SeqCst);
            let writes: Vec<MetaWrite> = std::mem::take(&mut *self.0.meta_queue.lock());
            for write in writes {
                let dir = write.dir.clone();
                let path = write.path.clone();
                if let Err(error) =
                    manox_harness::session_meta::update(&dir, &path, write.update).await
                {
                    tracing::warn!(
                        session = %path.display(),
                        %error,
                        "sidecar write failed"
                    );
                }
            }
            self.refresh_now().await;
            if !self
                .0
                .refresh_pending
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                break;
            }
        }
        self.0
            .refresh_running
            .store(false, std::sync::atomic::Ordering::SeqCst);
        if self
            .0
            .refresh_pending
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.refresh();
        }
    }

    /// The awaiting form of [`Self::refresh`]: one reconcile lands before
    /// the return. Freshness is fingerprint-driven — a stat sweep decides
    /// which (if any) files need a bounded read — so the steady-state cost
    /// is O(files), never O(bytes); a cold cache (first boot, wiped table)
    /// degrades to bounded per-file scans, never a full parse.
    ///
    /// The durable order account is read outside the mutation closure (it is
    /// awaited work; the closure never awaits), and the account's display
    /// order replaces the scanned order before the rows are published.
    pub async fn refresh_now(&self) {
        let dir = self.read(|s| s.sessions_dir.clone());
        let db = self.read(|s| std::sync::Arc::clone(&s.db));
        let registry = crate::thread_registry::load().await;
        let order = crate::sidebar_order::load().await;
        let rows = {
            let mut index = self.0.index.lock().await;
            reconcile_summaries(&dir, Some(&db), &mut index).await
        };
        self.with_mut(|s| {
            let (session_paths, summaries, archived) = group_by_thread(rows, &registry);
            let mut summaries = summaries;
            resolve_depths(&mut summaries);
            s.session_paths = session_paths;
            s.summaries = summaries;
            s.archived_summaries = archived;
            // The persisted account is the authority over the row order; the
            // scan's timestamp order is only ever an input to first-sight.
            s.order = order;
            s.rerank();
            s.apply_decision_overlay();
            s.pending_events.push(ThreadStoreEvent::SummariesUpdated);
        });
    }

    /// Route a decision-point row into the thread's journal (the K3
    /// mechanism, generalized): the live engine actor serializes it against
    /// every other writer of the session; a thread without an engine
    /// cold-appends on the session file. Callers without a store (foreign
    /// test fixtures) skip the route via `try_global`.
    pub fn route_journal_row(&self, id: &str, kind: &str, payload: serde_json::Value) {
        let path = self.read(|s| s.session_paths.get(id).cloned());
        crate::engine::dispatch_store_journal_row(id.to_string(), path, kind.to_string(), payload);
    }

    /// Queue one sidecar write for the next refresh pass: it lands (in
    /// dispatch order, alongside every other queued write) and the pass's
    /// single reconcile reads the settled result. A queued write asks for
    /// the pass itself.
    fn queue_meta_write(&self, write: MetaWrite) {
        self.0.meta_queue.lock().push(write);
        self.refresh();
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
        order: crate::sidebar_order::SidebarOrder::default(),
        order_dirty: false,
        running: HashSet::new(),
        pending_auth: HashSet::new(),
        pending_plan: HashSet::new(),
        background_work: HashSet::new(),
        live_threads: HashMap::new(),
        sessions_dir: dir,
        db,
        decision_overlay: HashMap::new(),
        pending_events: Vec::new(),
        pending_meta_writes: Vec::new(),
    });
    let purged = handle.with_mut(|s| s.purge_superseded_predecessors());
    if purged > 0 {
        tracing::info!(purged, "purged superseded predecessor sidecars");
    }
    handle.refresh();
    *GLOBAL.lock().unwrap() = Some(handle);
}

/// Whether the process-global store is a test override (`init_for_test`).
/// The gateway's ListThreads rescan self-hold skips the scan under an
/// override: the gpui test scheduler flags the millisecond answer latency
/// as foreign-thread activity (its determinism window is microscopic), and
/// the gpui suites never exercise the cross-process freshness the scan
/// serves — the session-core suite (production `init`, no override) pins
/// the real self-hold.
pub fn test_override_active() -> bool {
    #[cfg(any(test, feature = "test-support"))]
    {
        TEST_OVERRIDE.lock().unwrap().is_some()
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        false
    }
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
    /// The summary mirror row for one session id (sidecar-merged truth).
    pub fn summary_by_id(&self, id: &str) -> Option<&ThreadSummary> {
        self.summaries
            .iter()
            .find(|s| s.id == id)
            .or_else(|| self.archived_summaries.iter().find(|s| s.id == id))
    }

    /// A row's effective (pinned, archived): the decision overlay wins
    /// over the summary mirror while its sidecar write is still in flight
    /// — the mirror is what a racing scan last published, which can predate
    /// the decision.
    fn decided_flags(&self, id: &str) -> Option<(bool, bool)> {
        if let Some(flags) = self.decision_overlay.get(id) {
            return Some(*flags);
        }
        self.summary_by_id(id).map(|s| (s.pinned, s.archived))
    }

    /// The sidebar partition a row belongs to: its registered project, or the
    /// loose Conversations account. An unregistered path (a removed folder, or
    /// a cwd never bound as a project) is loose — the same partition rule the
    /// client renders on, so both sides agree by construction.
    fn partition_of<'a>(known_projects: &[String], project: &'a str) -> &'a str {
        if project.is_empty() || !known_projects.iter().any(|p| p == project) {
            crate::sidebar_order::LOOSE
        } else {
            project
        }
    }

    /// Re-emit both partitions from the durable order account: reconcile every
    /// account against live membership (first-seen ids prepend, dead ids and
    /// emptied partitions drop), then lay the rows out as folder sequence →
    /// pinned band → account rank.
    ///
    /// This is the one place the sidebar order is decided: a timestamp never
    /// re-sorts a live row. `order_dirty` rises only when the account itself
    /// moved — the emitted sequence is a pure function of the account, live
    /// membership and the pinned flag (sidecar truth), so re-emitting it after a
    /// rescan or a flag write persists nothing.
    fn rerank(&mut self) {
        let Self {
            summaries,
            archived_summaries,
            known_projects,
            order,
            order_dirty,
            ..
        } = self;
        let scanned = std::mem::take(summaries);
        let groups = order.reconcile_groups(known_projects);
        let rows: Vec<crate::sidebar_order::Row<'_>> = scanned
            .iter()
            .map(|s| crate::sidebar_order::Row {
                id: s.id.as_str(),
                partition: Self::partition_of(known_projects, &s.project),
                pinned: s.pinned,
                interacted_at: s.interacted_at,
            })
            .collect();
        let before = order.clone();
        let ordered = crate::sidebar_order::reconcile(order, &rows);
        let index: HashMap<&str, usize> = scanned
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.as_str(), i))
            .collect();
        let mut ranked: Vec<ThreadSummary> = Vec::with_capacity(scanned.len());
        for partition in groups
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(crate::sidebar_order::LOOSE))
        {
            let Some(ids) = ordered.get(partition) else {
                continue;
            };
            ranked.extend(
                ids.iter()
                    .filter_map(|id| index.get(*id).map(|&i| scanned[i].clone())),
            );
        }
        if ranked.len() != scanned.len() {
            // Every live row is claimed by exactly one partition, so a mismatch
            // means the account lost a row: keep the scanned order rather than
            // dropping it from the list.
            tracing::warn!(
                ranked = ranked.len(),
                live = scanned.len(),
                "sidebar order left rows unclaimed; keeping the scanned order"
            );
            ranked = scanned;
        }
        *summaries = ranked;
        // The registry list is the folder account's projection, so the wire
        // `Projects` mirror every client reads carries the committed sequence.
        *known_projects = groups;
        // The archived partition owns no account; a deterministic
        // interaction-then-id sort keeps its list stable across refreshes.
        archived_summaries.sort_by(|a, b| {
            b.interacted_at
                .cmp(&a.interacted_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        *order_dirty |= *order != before;
    }

    /// Move one thread inside its partition's durable account (DOM
    /// `insertBefore`; `before = None` appends to the tail). A rejected move —
    /// an unaccounted row or anchor — changes nothing at all: no mutation, no
    /// event, no write.
    pub fn insert_thread_before(&mut self, thread_id: &str, before: Option<&str>) {
        let Some(row) = self.summary_by_id(thread_id) else {
            tracing::warn!(thread = thread_id, "sidebar move rejected: unknown thread");
            return;
        };
        let partition = Self::partition_of(&self.known_projects, &row.project).to_string();
        match self.order.move_thread(&partition, thread_id, before) {
            Ok(false) => {}
            Ok(true) => {
                // The account is already mutated here, so the dirty flag must be
                // raised before the rerank (which diffs from this point).
                self.order_dirty = true;
                self.rerank();
                self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
            }
            Err(error) => {
                tracing::warn!(error = %error, thread = thread_id, "sidebar move rejected");
            }
        }
    }

    /// Move one project folder inside the Projects section order. Same
    /// rejection and no-op contract as [`Self::insert_thread_before`].
    pub fn insert_group_before(&mut self, path: &str, before: Option<&str>) {
        match self.order.move_group(path, before) {
            Ok(false) => {}
            Ok(true) => {
                // The account is already mutated here, so the dirty flag must be
                // raised before the rerank (which diffs from this point).
                self.order_dirty = true;
                self.rerank();
                self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
            }
            Err(error) => {
                tracing::warn!(error = %error, project = path, "sidebar folder move rejected");
            }
        }
    }

    /// All registered project paths in committed folder order. The sidebar
    /// renders a folder for every path here, and the wire registry mirror
    /// broadcasts this exact sequence, so folder order is shared state.
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
        // The folder account claims a committed tail position for the new
        // folder, and the registry list is re-projected from it.
        self.rerank();
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
        // The folder leaves the account; its rows re-partition as loose.
        self.rerank();
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

    /// Rename a session: the user's title, persisted in the sidecar.
    ///
    /// The sidecar is the title's durable authority (the transcript-derived
    /// name is not), so a rename is a meta write like [`Self::set_unread`].
    /// The in-memory row is updated too, and its `title_override` slot is what
    /// [`ThreadSummary::display_title`] reads first — so a rename outranks a
    /// title the model generated and survives a rescan.
    ///
    /// An empty or whitespace-only title is not a rename; it is refused so a
    /// client cannot blank out a session's name by sending nothing.
    pub fn rename_thread(&mut self, id: &str, title: &str) -> bool {
        let title = title.trim();
        if title.is_empty() {
            return false;
        }
        let Some(summary) = self.summary_mut(id) else {
            return false;
        };
        if summary.title_override.as_deref() == Some(title) {
            return true;
        }
        summary.title_override = Some(title.to_string());
        let owned = title.to_string();
        self.write_meta(id, move |meta| meta.title = Some(owned));
        true
    }

    /// Persist the session's granted extra working directories (multi-
    /// root) so a cold restore re-widens the fence (multi-working-dirs).
    pub fn set_working_directories(&mut self, id: &str, dirs: Vec<String>) {
        self.write_meta(id, move |meta| meta.working_directories = dirs);
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
    /// `Err` when another process holds the session's write lease — driving
    /// a session is exclusive across processes, never queued.
    pub fn load_thread(
        &mut self,
        id: &str,
    ) -> Result<Option<ThreadHandle>, crate::session_lease::LeaseError> {
        if let Some(weak) = self.live_threads.get(id)
            && let Some(handle) = ThreadHandle::upgrade(weak)
        {
            return Ok(Some(handle));
        }
        // Bind hand-off: a superseded id loads its successor — the
        // predecessor's identity is a redirect, never a second log.
        if let Some(successor) = self.superseded_by(id) {
            return self.load_thread(&successor);
        }
        let Some(path) = self.session_paths.get(id).cloned() else {
            return Ok(None);
        };
        let lease = crate::session_lease::acquire(&path)?;
        let cwd = self
            .summary_by_id(id)
            .map(|s| PathBuf::from(s.project.clone()))
            .unwrap_or_else(|| PathBuf::from("."));
        let handle = Thread::open_existing(ThreadId(id.to_string()), cwd, path.clone(), lease);
        // Re-surface the bound project from the sidecar so the chip shows it.
        if let Some(sum) = self.summary_by_id(id)
            && !sum.project.is_empty()
        {
            let dir = PathBuf::from(&sum.project);
            handle.with_mut(|t| t.restore_project(dir));
        }
        // Multi-root restore (multi-working-dirs): the sidecar's granted
        // directories re-widen the restored engine's fence — `open`
        // spawned it cwd-only.
        for dir in self.persisted_working_directories(&path) {
            handle.with_mut(|t| t.grant_working_directory(dir));
        }
        self.live_threads.insert(id.to_string(), handle.downgrade());
        Ok(Some(handle))
    }

    /// The sidecar's granted working directories for a session file, read
    /// synchronously (multi-root restore). The scan cache is not
    /// consulted: a cold id seeded through `note_session_path` has no
    /// indexed row yet, and one bounded read beats a refresh round-trip.
    fn persisted_working_directories(&self, path: &std::path::Path) -> Vec<PathBuf> {
        let meta = manox_harness::session_meta::meta_path(&self.sessions_dir, path);
        let Ok(json) = std::fs::read_to_string(&meta) else {
            return Vec::new();
        };
        serde_json::from_str::<manox_harness::session_meta::SessionMeta>(&json)
            .map(|m| {
                m.working_directories
                    .into_iter()
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Seed one session path from an authoritative on-disk probe: a cold
    /// `CreateSession`/`OpenSession` must restore an id no list refresh has
    /// indexed yet (a bare server never scans). A scanned mapping wins —
    /// this never overwrites what a refresh grouped (thread-keyed leaf
    /// pointers).
    pub fn note_session_path(&mut self, id: &str, path: &std::path::Path) {
        self.session_paths
            .entry(id.to_string())
            .or_insert_with(|| path.to_path_buf());
    }

    /// Seed the bound project on a test summary row (the sidecar truth the
    /// workspace domain's header validation reads).
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_project_for_test(&mut self, id: &str, project: &str) {
        if let Some(sum) = self.summaries.iter_mut().find(|s| s.id == id) {
            sum.project = project.to_string();
        }
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
            superseded_by: None,
            has_unread: false,
            errored: false,
            created_at: 0,
            interacted_at: 0,
            updated_at: 0,
            cumulative_total_tokens: 0,
        });
    }

    /// Like `insert_summary_for_test`, with explicit recency columns —
    /// interacted_at (advanced by real activity only) and updated_at
    /// (advanced by every metadata save) diverge in production, and
    /// consumers pin the wire mapping between them.
    // Explicit per-column seeding: every parameter is a wire-mapped column
    // the list regression pins; a builder struct would obscure the mapping.
    #[allow(clippy::too_many_arguments)]
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_summary_with_times_for_test(
        &mut self,
        id: &str,
        parent: Option<&str>,
        interacted_at: i64,
        updated_at: i64,
        project: &str,
        tag: Option<&str>,
        approval_mode: i64,
    ) {
        self.insert_summary_for_test(id, parent);
        let summary = self.summaries.last_mut().expect("just inserted");
        summary.interacted_at = interacted_at;
        summary.updated_at = updated_at;
        summary.project = project.to_string();
        summary.tag = tag.map(str::to_string);
        summary.approval_mode = approval_mode;
    }

    /// Fold the decision overlay into the freshly scanned partitions: a
    /// row whose decision is still in flight keeps the DECIDED flags and
    /// partition — never the pre-write scan state a racing pass read. A
    /// scan that already shows the decided pair confirms the write landed;
    /// the entry retires. A row absent from the scan keeps its entry until
    /// it reappears.
    fn apply_decision_overlay(&mut self) {
        let decided: Vec<(String, (bool, bool))> = self.decision_overlay.drain().collect();
        for (id, (pinned, archived)) in decided {
            let observed = self.summary_by_id(&id).map(|s| (s.pinned, s.archived));
            if observed == Some((pinned, archived)) {
                continue; // the write landed and was observed: retire.
            }
            let moved = if let Some(pos) = self.summaries.iter().position(|s| s.id == id) {
                let mut summary = self.summaries.remove(pos);
                summary.pinned = pinned;
                summary.archived = archived;
                if archived {
                    self.archived_summaries.push(summary);
                } else {
                    self.summaries.push(summary);
                }
                true
            } else if let Some(pos) = self.archived_summaries.iter().position(|s| s.id == id) {
                let mut summary = self.archived_summaries.remove(pos);
                summary.pinned = pinned;
                summary.archived = archived;
                if archived {
                    self.archived_summaries.push(summary);
                } else {
                    self.summaries.push(summary);
                }
                true
            } else {
                false
            };
            if !moved {
                // The row is not in this scan (a vanished file mid-decision):
                // keep the decision effective against its return.
                self.decision_overlay.insert(id, (pinned, archived));
            }
        }
    }

    /// Archive (or unarchive) a session. The row moves between the active
    /// and archived partitions immediately; the post-write refresh in
    /// `write_meta` re-syncs both partitions from disk. Archiving cascades
    /// to every descendant along `parent_id` (team members, fork children —
    /// one hierarchy rule); unarchiving moves only the requested row.
    /// Re-asserting the current state is a no-op: no partition move, meta
    /// write, or lifecycle hook.
    pub fn archive_thread(&mut self, id: &str, archived: bool) {
        if self.decided_flags(id).is_some_and(|(_, a)| a == archived) {
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
            // SessionEnd per working life. The overlay counts: a decision
            // in flight IS the row's state.
            if self.decided_flags(&tid).is_some_and(|(_, a)| a == archived) {
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
            // K3 (L3): every cascaded row's flag decision journals its own
            // `pinned_archived` entry (the entry is the authority, K2); a
            // skipped row already carries the target state and stays
            // silent, matching the no-op discipline of this method.
            let pinned = self.summary_by_id(&tid).is_some_and(|s| s.pinned);
            // The decision is effective NOW (the mirror flip below) even
            // though its sidecar write lands in the next refresh pass.
            self.decision_overlay
                .insert(tid.clone(), (pinned, archived));
            self.journal_pinned_archived(&tid, pinned, archived);
            self.write_meta(&tid, move |meta| meta.archived = archived);
            if archived {
                // Plugin lifecycle: archiving ends the session's working life
                // (the retired harness fired on thread deletion; the manox harness
                // keeps sessions and archives instead). Fail-open, detached.
                crate::plugin_hooks::fire(
                    crate::plugin_hooks::HookEvent::SessionEnd,
                    None,
                    serde_json::json!({ "thread_id": tid }),
                );
            }
        }
        // Membership moved between the two partitions, so the accounts gain or
        // lose exactly the rows that just crossed.
        self.rerank();
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

    /// Set the pinned flag on a session (persisted in its sidecar). A pin is an
    /// explicit order action — like a drag — so the row floats to the head of
    /// its partition and leads the pinned band; unpinning leaves the account
    /// alone and the row drops into the unpinned band at its stored rank.
    pub fn pin_thread(&mut self, id: &str, pinned: bool) {
        let archived = self.summary_by_id(id).is_some_and(|s| s.archived);
        if let Some(s) = self.summary_mut(id) {
            s.pinned = pinned;
        }
        self.decision_overlay
            .insert(id.to_string(), (pinned, archived));
        if pinned {
            let project = self
                .summary_by_id(id)
                .map(|s| s.project.clone())
                .unwrap_or_default();
            let known = self.known_projects.clone();
            let partition = Self::partition_of(&known, &project).to_string();
            if self.order.float_to_head(&partition, id) {
                self.order_dirty = true;
            }
        }
        // The band membership moved, so the emitted order is re-derived.
        self.rerank();
        // K3 (L3): the flag decision journals a `pinned_archived` entry —
        // the entry is the authority (K2) and the sidecar write below is
        // the derived fast-list cache. The entry carries BOTH flags, so
        // one entry fully re-establishes the pair on rebuild.
        self.journal_pinned_archived(id, pinned, archived);
        self.write_meta(id, move |meta| meta.pinned = pinned);
    }

    /// Route a `pinned_archived` decision into the thread's journal (K3):
    /// the live engine actor serializes it against every other writer of
    /// the session; a thread without an engine cold-appends on the session
    /// file. The flag pair is the post-decision full state, sourced from
    /// the summary mirror (a thread whose summary never loaded carries the
    /// decided flag alone — every later decision re-carries the pair).
    fn journal_pinned_archived(&self, id: &str, pinned: bool, archived: bool) {
        crate::engine::dispatch_store_journal_row(
            id.to_string(),
            self.session_paths.get(id).cloned(),
            "pinned_archived".into(),
            serde_json::json!({ "pinned": pinned, "archived": archived }),
        );
    }

    /// Durable supersede marker for a bind hand-off: the summary mirror
    /// flips up front (the redirect and the list exclusion act on it
    /// immediately) and the sidecar write follows on the refresh pass.
    pub fn mark_superseded(&mut self, id: &str, successor: &str) {
        if self
            .summary_by_id(id)
            .and_then(|s| s.superseded_by.clone())
            .as_deref()
            == Some(successor)
        {
            return;
        }
        if let Some(sum) = self.summaries.iter_mut().find(|s| s.id == id) {
            sum.superseded_by = Some(successor.to_string());
        }
        self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
        // A not-yet-materialized predecessor has no scan-indexed path (the
        // scan iterates journal files); synthesize the canonical one so the
        // marker still lands on its sidecar.
        if !self.session_paths.contains_key(id) {
            let path = self.sessions_dir.join(format!("{id}.jsonl"));
            self.session_paths.insert(id.to_string(), path);
        }
        let successor = successor.to_string();
        self.write_meta(id, move |meta| meta.superseded_by = Some(successor.clone()));
    }

    /// The bound project straight from the session sidecar (sync read).
    /// Unlike the summary mirror, the sidecar survives reconciles for
    /// sessions whose journal never materialized — the header-validation
    /// source the workspace domain relies on (review #805 follow-up).
    pub fn sidecar_project(&self, id: &str) -> Option<String> {
        // The canonical sidecar path is derived from the id alone — a
        // synthetic `session_paths` entry (bind-time predecessor) is not
        // required, and a reconcile pass cannot invalidate the lookup.
        let meta_path = self.sessions_dir.join(format!("{id}.meta.json"));
        let raw = std::fs::read_to_string(meta_path).ok()?;
        let meta: manox_harness::session_meta::SessionMeta = serde_json::from_str(&raw).ok()?;
        meta.project
    }

    /// Prune the durable rows of superseded predecessors that never
    /// materialized (plan §3.1 B-iv): a fresh process has no live entry for
    /// them, and any rows a create-time upsert left behind are dead weight.
    /// The SIDECAR MARKER STAYS — the redirect must keep resolving an id a
    /// client (or another host) still holds across a restart (review #809
    /// [sugg] 7); a materialized predecessor keeps its journal as well.
    pub fn purge_superseded_predecessors(&mut self) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.sessions_dir) else {
            return 0;
        };
        let mut purged = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let Some(id) = name.strip_suffix(".meta.json") else {
                continue;
            };
            if self.sessions_dir.join(format!("{id}.jsonl")).exists() {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(meta) = serde_json::from_str::<manox_harness::session_meta::SessionMeta>(&raw)
            else {
                continue;
            };
            if meta.superseded_by.is_none() {
                continue;
            }
            self.session_paths.remove(id);
            match self.db.delete_thread(id) {
                Ok(()) => purged += 1,
                Err(error) => {
                    tracing::warn!(%error, session = %id, "superseded db row purge failed");
                }
            }
        }
        purged
    }

    /// The successor a session was superseded by (summary mirror of the
    /// sidecar marker — the restart-surviving half of the redirect map).
    pub fn superseded_by(&self, id: &str) -> Option<String> {
        self.summary_by_id(id).and_then(|s| s.superseded_by.clone())
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

/// The hot layer of the session-scan cache (the db `session_index` table is
/// the cold layer). Facts are immutable in an append-only journal, so they
/// are cached forever until a file SHRINKS; activity (mtime) refreshes from
/// stat every scan. A never-seen file costs one bounded read.
#[derive(Default)]
struct SessionIndex {
    /// `false` until the first seed from the db.
    seeded: bool,
    /// Anything moved since the last persist — a new or rescanned file, a
    /// refreshed activity fingerprint, a reloaded sidecar. A pure-stat pass
    /// over an idle store writes nothing.
    dirty: bool,
    rows: HashMap<PathBuf, IndexedSession>,
}

/// One cached session file's bounded list facts.
#[derive(Clone)]
struct IndexedSession {
    /// File size when the facts were read — the FACT-layer fingerprint:
    /// growth keeps the facts (appends never touch them), a shrink rescans.
    size: u64,
    /// The activity layer — refreshed from stat every scan, never a fact
    /// invalidator.
    mtime_ns: i64,
    /// The last bounded scan FAILED while the file wore this fingerprint
    /// (a headerless zombie from an old bug, a torn file). An unchanged
    /// fingerprint skips the file silently — no re-read, no warn spam;
    /// any change re-attempts once and re-warns.
    failed: bool,
    id: String,
    cwd: String,
    created_at: chrono::DateTime<chrono::Utc>,
    parent_session_path: Option<String>,
    metadata: Option<serde_json::Value>,
    first_user_text: String,
    has_messages: bool,
    sidecar_size: u64,
    sidecar_mtime_ns: i64,
    sidecar: Option<manox_harness::session_meta::SessionMeta>,
}

/// One stat sweep result: the session file's fingerprints plus its
/// sidecar's. The steady-state refresh touches NOTHING else.
struct Stat {
    path: PathBuf,
    size: u64,
    mtime_ns: i64,
    sidecar_size: u64,
    sidecar_mtime_ns: i64,
}

/// The refresh's reconcile: stat every session file, reuse cached facts
/// whose (append-only) coverage still holds, bounded-read only the new or
/// shrunk files, refresh sidecars whose fingerprint moved, and persist the
/// index wholesale (best-effort). Returns the rows the sidebar renders —
/// identical CONTENT to a full scan, at O(files) steady-state cost.
async fn reconcile_summaries(
    dir: &std::path::Path,
    db: Option<&std::sync::Arc<crate::db::ThreadsDatabase>>,
    index: &mut SessionIndex,
) -> Vec<SessionRow> {
    if !index.seeded {
        if let Some(db) = db {
            match db.load_session_index() {
                Ok(rows) => {
                    index.rows = rows
                        .into_iter()
                        .filter_map(IndexedSession::from_db)
                        .collect();
                }
                Err(error) => {
                    // A missing/corrupt table degrades to bounded scans —
                    // the list's content never depends on the cache.
                    tracing::warn!(%error, "session index unreadable; cold-scanning instead");
                }
            }
        }
        index.seeded = true;
    }
    let stats = stat_session_files(dir).await;
    let live: std::collections::HashSet<PathBuf> = stats.iter().map(|s| s.path.clone()).collect();
    let mut rows = Vec::with_capacity(stats.len());
    for stat in stats {
        // Negative-cache hit: the file failed its scan wearing exactly this
        // fingerprint — skip silently until it changes.
        if index.rows.get(&stat.path).is_some_and(|entry| {
            entry.failed && entry.size == stat.size && entry.mtime_ns == stat.mtime_ns
        }) {
            continue;
        }
        // Fact layer: an unknown or SHRUNK file rescans (bounded); growth or
        // an unchanged size reuses the cached facts untouched. A FAILED
        // entry reaching here (its fingerprint changed — the skip arm above
        // handled the unchanged case) has no facts to reuse and rescans
        // whatever the size says.
        let mut torn_tail = false;
        if index
            .rows
            .get(&stat.path)
            .is_none_or(|entry| entry.failed || stat.size < entry.size)
        {
            match manox_harness::session::repository::scan_session(&stat.path).await {
                Ok(scanned) => {
                    // A possibly-torn tail (facts read off an unterminated
                    // last line — an append mid-write) renders this pass but
                    // never caches: the fact layer would pin a half-written
                    // first message until the file shrank, which an
                    // append-only writer never does.
                    torn_tail = scanned.torn_tail;
                    let info = scanned.info;
                    let entry = index
                        .rows
                        .entry(stat.path.clone())
                        .or_insert(IndexedSession {
                            size: 0,
                            mtime_ns: 0,
                            failed: false,
                            id: String::new(),
                            cwd: String::new(),
                            created_at: chrono::Utc::now(),
                            parent_session_path: None,
                            metadata: None,
                            first_user_text: String::new(),
                            has_messages: false,
                            sidecar_size: 0,
                            sidecar_mtime_ns: 0,
                            sidecar: None,
                        });
                    // A successful rescan clears any remembered failure —
                    // the negative cache must never swallow a recovered
                    // file — and the fresh facts are worth persisting.
                    entry.failed = false;
                    index.dirty = true;
                    entry.id = info.id;
                    entry.cwd = info.cwd;
                    entry.created_at = info.created_at;
                    entry.parent_session_path = info.parent_session_path;
                    entry.metadata = info.metadata;
                    entry.first_user_text = info.first_message;
                    entry.has_messages = info.has_messages;
                }
                Err(error) => {
                    tracing::warn!(
                        path = %stat.path.display(),
                        %error,
                        "session file skipped by the sidebar reconcile"
                    );
                    // Negative cache: the failure is fingerprinted, so an
                    // unchanged file never re-reads (or re-warns) — a store
                    // carrying legacy headerless zombies pays for them once.
                    index.dirty = true;
                    index.rows.insert(
                        stat.path.clone(),
                        IndexedSession {
                            size: stat.size,
                            mtime_ns: stat.mtime_ns,
                            failed: true,
                            id: String::new(),
                            cwd: String::new(),
                            created_at: chrono::Utc::now(),
                            parent_session_path: None,
                            metadata: None,
                            first_user_text: String::new(),
                            has_messages: false,
                            sidecar_size: 0,
                            sidecar_mtime_ns: 0,
                            sidecar: None,
                        },
                    );
                    continue;
                }
            }
        }
        let Some(entry) = index.rows.get_mut(&stat.path) else {
            continue;
        };
        // Activity layer: size/mtime follow the stat sweep; a change
        // (a fresh append) marks the index for persistence.
        if entry.size != stat.size || entry.mtime_ns != stat.mtime_ns {
            entry.size = stat.size;
            entry.mtime_ns = stat.mtime_ns;
            index.dirty = true;
        }
        // Sidecar: fingerprint match skips the read entirely.
        if stat.sidecar_size == 0 {
            if entry.sidecar.is_some() {
                index.dirty = true;
            }
            entry.sidecar = None;
            entry.sidecar_size = 0;
            entry.sidecar_mtime_ns = 0;
        } else if entry.sidecar_size != stat.sidecar_size
            || entry.sidecar_mtime_ns != stat.sidecar_mtime_ns
            || entry.sidecar.is_none()
        {
            let loaded = manox_harness::session_meta::load(dir, &stat.path).await;
            index.dirty = true;
            entry.sidecar = Some(loaded.unwrap_or_else(|error| {
                tracing::warn!(
                    session = %stat.path.display(),
                    %error,
                    "session sidecar unreadable; rendering default flags"
                );
                manox_harness::session_meta::SessionMeta::default()
            }));
            entry.sidecar_size = stat.sidecar_size;
            entry.sidecar_mtime_ns = stat.sidecar_mtime_ns;
        }
        // Borrowed sidecar — no per-row SessionMeta clone (a fat sidecar's
        // plan snapshot would otherwise be deep-copied for every file on
        // every refresh).
        let default_meta = DEFAULT_SESSION_META.get_or_init(Default::default);
        let meta = entry.sidecar.as_ref().unwrap_or(default_meta);
        if meta.interacted_at.is_none() {
            seed_interaction_stamp(dir, &stat.path, &entry.id, stat.mtime_ns / 1_000_000_000);
        }
        // The sidebar renders only the current host's sessions; subagent
        // transcripts persist for usage accounting but never surface.
        if !crate::host::belongs_to_current_host(entry.metadata.as_ref())
            || entry
                .metadata
                .as_ref()
                .is_some_and(|m| m.get("subagent").is_some())
        {
            continue;
        }
        let modified_at = nanos_to_datetime(entry.mtime_ns).unwrap_or(entry.created_at);
        let thread_key = entry
            .metadata
            .as_ref()
            .and_then(|m| m.get("thread"))
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        rows.push(SessionRow {
            summary: summary_from_cached(entry, modified_at, meta),
            path: stat.path.clone(),
            thread_key,
        });
        if torn_tail {
            index.rows.remove(&stat.path);
            index.dirty = true;
        }
    }
    // Vanished files leave the cache; the persist below drops their rows
    // from the cold layer with them.
    let pruned = index.rows.len() != live.len();
    index.rows.retain(|path, _| live.contains(path));
    // Persist only when something moved (a new or rescanned file, a
    // refreshed fingerprint, a reloaded sidecar, a prune): a pure-stat pass
    // over an idle store writes nothing. Failed verdicts persist too — a
    // restart must not re-attempt every legacy zombie the store carries.
    if (pruned || index.dirty)
        && let Some(db) = db
    {
        let persisted: Vec<crate::db::SessionIndexRow> = index
            .rows
            .iter()
            .map(|(path, entry)| entry.to_db(path))
            .collect();
        if let Err(error) = db.replace_session_index(&persisted) {
            tracing::warn!(%error, "failed to persist the session index");
        }
    }
    index.dirty = false;
    rows
}

/// The shared default sidecar for sessions without one (borrow target so
/// cache-hit rows never clone a `SessionMeta`).
static DEFAULT_SESSION_META: std::sync::OnceLock<manox_harness::session_meta::SessionMeta> =
    std::sync::OnceLock::new();

/// The cache-hit row's summary, built by borrowing: no `SessionInfo`
/// roundtrip (that would deep-clone the header metadata JSON) and no
/// full-`SessionMeta` clone — only the small fields the summary carries.
/// Mirrors [`session_info_to_summary`] field for field.
fn summary_from_cached(
    entry: &IndexedSession,
    modified_at: chrono::DateTime<chrono::Utc>,
    meta: &manox_harness::session_meta::SessionMeta,
) -> ThreadSummary {
    let summary = if entry.first_user_text.trim().is_empty() {
        "(no messages)".to_string()
    } else {
        entry.first_user_text.clone()
    };
    // Team affiliation over fork lineage (`team_parent_id`'s precedence).
    let parent_id = entry
        .metadata
        .as_ref()
        .and_then(|m| m.get("team"))
        .and_then(|t| t.get("parent"))
        .and_then(|p| p.as_str())
        .map(str::to_string)
        .or_else(|| entry.parent_session_path.clone());
    ThreadSummary {
        id: entry.id.clone(),
        summary,
        title: meta.title.clone(),
        title_override: None,
        model_id: String::new(),
        provider_id: None,
        approval_mode: PermissionMode::default().as_i64(),
        // The bound project (sidecar) wins over the header cwd.
        project: meta
            .project
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| entry.cwd.clone()),
        depth: 0,
        parent_id,
        archived: meta.archived,
        pinned: meta.pinned,
        tag: meta.tag.clone(),
        superseded_by: meta.superseded_by.clone(),
        has_unread: meta.unread,
        errored: meta.errored,
        created_at: entry.created_at.timestamp(),
        interacted_at: meta.interacted_at.unwrap_or(modified_at.timestamp()),
        updated_at: modified_at.timestamp(),
        cumulative_total_tokens: 0,
    }
}

/// The stat sweep: every session file's (size, mtime) plus its sidecar's.
/// No content is read — this is the entire steady-state cost of a refresh.
/// One blocking task with synchronous syscalls: a per-file async `stat`
/// costs a thread-pool hop each, which dwarfs the stat itself on a
/// thousands-file store.
async fn stat_session_files(dir: &std::path::Path) -> Vec<Stat> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let Ok(file_meta) = std::fs::metadata(&path) else {
                continue;
            };
            if !file_meta.is_file() {
                continue;
            }
            let sidecar =
                std::fs::metadata(manox_harness::session_meta::meta_path(&dir, &path)).ok();
            out.push(Stat {
                path,
                size: file_meta.len(),
                mtime_ns: metadata_mtime_ns(&file_meta),
                sidecar_size: sidecar.as_ref().map_or(0, |m| m.len()),
                sidecar_mtime_ns: sidecar.as_ref().map_or(0, metadata_mtime_ns),
            });
        }
        out
    })
    .await
    .unwrap_or_default()
}

/// Epoch nanoseconds of a file's mtime (0 when unknowable).
fn metadata_mtime_ns(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i64)
        .unwrap_or(0)
}

fn nanos_to_datetime(nanos: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp(nanos / 1_000_000_000, (nanos % 1_000_000_000) as u32)
}

impl IndexedSession {
    fn from_db(row: crate::db::SessionIndexRow) -> Option<(PathBuf, IndexedSession)> {
        let metadata = row.metadata_json.as_deref().and_then(|json| {
            serde_json::from_str::<serde_json::Value>(json)
                .map_err(|error| {
                    tracing::warn!(
                        path = %row.path.display(),
                        %error,
                        "cached session metadata corrupt; rescanning the file"
                    );
                    error
                })
                .ok()
        });
        // A FAILED row carries no facts by design — its fingerprint IS
        // the content. A fact row whose identity columns cannot be trusted
        // cannot rebuild a summary without the file; drop it and let the
        // bounded scan re-establish the facts.
        if row.failed {
            return Some((
                row.path,
                IndexedSession {
                    size: row.size,
                    mtime_ns: row.mtime_ns,
                    failed: true,
                    id: String::new(),
                    cwd: String::new(),
                    created_at: chrono::DateTime::parse_from_rfc3339("1970-01-01T00:00:00Z")
                        .unwrap()
                        .with_timezone(&chrono::Utc),
                    parent_session_path: None,
                    metadata: None,
                    first_user_text: String::new(),
                    has_messages: false,
                    sidecar_size: 0,
                    sidecar_mtime_ns: 0,
                    sidecar: None,
                },
            ));
        }
        if row.session_id.is_empty() || row.cwd.is_empty() {
            return None;
        }
        let sidecar = row.sidecar_json.as_deref().and_then(|json| {
            serde_json::from_str::<manox_harness::session_meta::SessionMeta>(json)
                .map_err(|error| {
                    tracing::warn!(
                        path = %row.path.display(),
                        %error,
                        "cached sidecar corrupt; reloading it"
                    );
                    error
                })
                .ok()
        });
        Some((
            row.path,
            IndexedSession {
                size: row.size,
                mtime_ns: row.mtime_ns,
                failed: false,
                id: row.session_id,
                cwd: row.cwd,
                created_at: nanos_to_datetime(row.created_at_ns)?,
                parent_session_path: row.parent_session,
                metadata,
                first_user_text: row.first_user_text,
                has_messages: row.has_messages,
                sidecar_size: row.sidecar_size,
                sidecar_mtime_ns: row.sidecar_mtime_ns,
                sidecar,
            },
        ))
    }

    fn to_db(&self, path: &std::path::Path) -> crate::db::SessionIndexRow {
        if self.failed {
            return crate::db::SessionIndexRow {
                path: path.to_path_buf(),
                size: self.size,
                mtime_ns: self.mtime_ns,
                session_id: String::new(),
                cwd: String::new(),
                created_at_ns: 0,
                parent_session: None,
                metadata_json: None,
                first_user_text: String::new(),
                has_messages: false,
                failed: true,
                sidecar_size: 0,
                sidecar_mtime_ns: 0,
                sidecar_json: None,
            };
        }
        crate::db::SessionIndexRow {
            path: path.to_path_buf(),
            size: self.size,
            mtime_ns: self.mtime_ns,
            session_id: self.id.clone(),
            cwd: self.cwd.clone(),
            created_at_ns: self.created_at.timestamp_nanos_opt().unwrap_or_default(),
            parent_session: self.parent_session_path.clone(),
            metadata_json: self.metadata.as_ref().map(|value| value.to_string()),
            first_user_text: self.first_user_text.clone(),
            has_messages: self.has_messages,
            failed: false,
            sidecar_size: self.sidecar_size,
            sidecar_mtime_ns: self.sidecar_mtime_ns,
            sidecar_json: self
                .sidecar
                .as_ref()
                .map(|meta| serde_json::to_string(meta).unwrap_or_default()),
        }
    }
}

/// Materialize a missing interaction stamp. Fire-and-forget and rescan-free
/// (this write must never trigger another scan — it runs inside one), and
/// idempotent: a concurrent human prompt that stamped the sidecar first wins,
/// because the update refuses to overwrite a present value.
fn seed_interaction_stamp(dir: &std::path::Path, path: &std::path::Path, id: &str, at: i64) {
    let Some(handle) = crate::runtime::try_handle() else {
        return;
    };
    let dir = dir.to_path_buf();
    let path = path.to_path_buf();
    let id = id.to_string();
    handle.spawn(async move {
        let result = manox_harness::session_meta::update(&dir, &path, |meta| {
            if meta.interacted_at.is_none() {
                meta.interacted_at = Some(at);
            }
        })
        .await;
        if let Err(error) = result {
            tracing::warn!(session = %id, %error, "failed to seed the interaction stamp");
        }
    });
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

#[cfg(any(test, feature = "test-support"))]
pub fn init_for_test(db: Arc<crate::db::ThreadsDatabase>) {
    let handle = standalone_for_test(db);
    *TEST_OVERRIDE.lock().unwrap() = Some(handle);
}

/// A standalone store handle over `db` (test-support): NO process global
/// involved. Tests that inject a store directly (the goal bridge's journal
/// routing) use this to stay immune to cross-test TEST_OVERRIDE churn —
/// the agent suite runs its store tests in parallel.
#[cfg(any(test, feature = "test-support"))]
pub fn standalone_for_test(db: Arc<crate::db::ThreadsDatabase>) -> StoreHandle {
    let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    StoreHandle::new(ThreadStore {
        summaries: Vec::new(),
        archived_summaries: Vec::new(),
        session_paths: HashMap::new(),
        known_projects: Vec::new(),
        order: crate::sidebar_order::SidebarOrder::default(),
        order_dirty: false,
        db,
        running: HashSet::new(),
        pending_auth: HashSet::new(),
        pending_plan: HashSet::new(),
        background_work: HashSet::new(),
        live_threads: HashMap::new(),
        sessions_dir: dir,
        decision_overlay: HashMap::new(),
        pending_events: Vec::new(),
        pending_meta_writes: Vec::new(),
    })
}

#[cfg(any(test, feature = "test-support"))]
pub fn drop_for_test() {
    *TEST_OVERRIDE.lock().unwrap() = None;
}

/// The directory the global store scans for session transcripts.
/// The store is the sessions-dir single authority (review round 3, P0-1):
/// the gateway's cold read resolves through this seam in production, so it
/// is not test-gated. The desktop end-to-end test seeds transcripts here,
/// calls [`StoreHandle::refresh`], and switches threads to assert a cold
/// restore loads the persisted transcript (the #765 regression lock).
pub fn global_sessions_dir() -> PathBuf {
    global().read(|s| s.sessions_dir.clone())
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

    /// A rename is a user title: it takes display precedence over a
    /// model-generated one (the `title_override` slot `display_title` reads
    /// first), and a blank title is refused rather than stored — a blank would
    /// read as "renamed to nothing" while the session kept its old name.
    #[test]
    fn rename_thread_sets_the_override_and_refuses_blank() {
        let (db, _path) = temp_db();
        let store = store_handle(db);
        store.with_mut(|s| s.insert_summary_for_test("t-rename", None));
        store.with_mut(|s| {
            let row = s.summary_mut("t-rename").expect("seeded");
            row.title = Some("model said this".to_string());
        });

        store.with_mut(|s| assert!(s.rename_thread("t-rename", "user said this")));
        let row = store
            .read(|s| s.summary_by_id("t-rename").cloned())
            .unwrap();
        assert_eq!(row.title_override.as_deref(), Some("user said this"));
        assert_eq!(
            row.display_title(),
            "user said this",
            "a rename outranks the model's title"
        );

        // Whitespace is not a title.
        store.with_mut(|s| assert!(!s.rename_thread("t-rename", "   ")));
        store.with_mut(|s| assert!(!s.rename_thread("t-rename", "")));
        let row = store
            .read(|s| s.summary_by_id("t-rename").cloned())
            .unwrap();
        assert_eq!(
            row.title_override.as_deref(),
            Some("user said this"),
            "a refused rename leaves the previous title intact"
        );

        // An unknown session is refused, not silently dropped.
        store.with_mut(|s| assert!(!s.rename_thread("no-such-session", "x")));
    }

    fn store_handle(db: Arc<crate::db::ThreadsDatabase>) -> StoreHandle {
        let known_projects = db.list_projects().unwrap_or_default();
        StoreHandle::new(ThreadStore {
            summaries: Vec::new(),
            archived_summaries: Vec::new(),
            session_paths: HashMap::new(),
            known_projects,
            order: crate::sidebar_order::SidebarOrder::default(),
            order_dirty: false,
            db,
            running: HashSet::new(),
            pending_auth: HashSet::new(),
            pending_plan: HashSet::new(),
            background_work: HashSet::new(),
            live_threads: HashMap::new(),
            sessions_dir: std::env::temp_dir(),
            decision_overlay: HashMap::new(),
            pending_events: Vec::new(),
            pending_meta_writes: Vec::new(),
        })
    }

    /// A v4 session fixture: a host+thread-stamped header plus one user
    /// message whose text length the test controls (the size lever for the
    /// growth/shrink rules).
    fn write_session_fixture(
        dir: &std::path::Path,
        id: &str,
        first_user: &str,
    ) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        let content = format!(
            "{{\"type\":\"session\",\"version\":4,\"id\":\"{id}\",\"timestamp\":\"2026-09-13T00:00:00Z\",\"cwd\":\"/proj\",\"metadata\":{{\"host\":\"manox\",\"thread\":\"{id}\"}}}}\n{{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-09-13T00:00:01Z\",\"seq\":0,\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"{first_user}\"}}],\"timestamp\":1770000000000}}}}\n",
        );
        std::fs::write(&path, content).unwrap();
        path
    }

    /// Plan §3.1 B-iv: the startup prune drops the durable rows of a
    /// superseded predecessor that never materialized, and keeps its
    /// sidecar marker (the redirect must survive a restart) as well as a
    /// materialized predecessor's journal.
    #[test]
    fn purge_superseded_predecessors_removes_only_unmaterialized_sidecars() {
        let (db, db_path) = temp_db();
        let dir = std::env::temp_dir().join(format!("pi-store-purge-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let handle = store_handle(db);
        handle.with_mut(|s| s.sessions_dir = dir.clone());
        std::fs::write(dir.join("pred.meta.json"), "{\"superseded_by\":\"succ\"}").unwrap();
        std::fs::write(dir.join("kept.meta.json"), "{\"superseded_by\":\"succ2\"}").unwrap();
        let kept_journal = write_session_fixture(&dir, "kept", "hello");
        std::fs::write(dir.join("plain.meta.json"), "{\"project\":\"/proj\"}").unwrap();

        let purged = handle.with_mut(|s| s.purge_superseded_predecessors());
        assert_eq!(purged, 1, "only the unmaterialized predecessor prunes");
        assert!(
            dir.join("pred.meta.json").exists(),
            "the redirect marker SURVIVES: an id a client still holds must keep              resolving after a restart"
        );
        assert!(
            dir.join("kept.meta.json").exists() && kept_journal.exists(),
            "a materialized predecessor keeps its sidecar and journal"
        );
        assert!(
            dir.join("plain.meta.json").exists(),
            "a sidecar without the marker is never touched"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&db_path);
    }

    fn first_summary_of(store: &StoreHandle, id: &str) -> String {
        store.read(|s| {
            s.summaries()
                .iter()
                .find(|row| row.id == id)
                .map(|row| row.summary.clone())
                .expect("the row is listed")
        })
    }

    /// The fact-layer rule: append-only GROWTH never rescans a session's
    /// bounded facts (the row keeps the cached first message even when the
    /// prefix on disk moved underneath — an impossibility under real
    /// appends, pinned here precisely because the cache must not care);
    /// a SHRINK always rescans.
    #[tokio::test]
    async fn reconcile_reuses_facts_on_growth_and_rescans_on_shrink() {
        let (db, _db_path) = temp_db();
        let store = standalone_for_test(db.clone());
        let dir = store.read(|s| s.sessions_dir.clone());
        write_session_fixture(&dir, "rule", "original");

        store.refresh_now().await;
        assert_eq!(first_summary_of(&store, "rule"), "original");

        // Grow the file with a different prefix: the facts stay cached.
        write_session_fixture(&dir, "rule", &format!("rewritten{}", " ".repeat(4096)));
        store.refresh_now().await;
        assert_eq!(
            first_summary_of(&store, "rule"),
            "original",
            "growth keeps the cached facts — an active session is never re-read"
        );

        // Shrink below the covered size: the facts rescan.
        write_session_fixture(&dir, "rule", "shrunk");
        store.refresh_now().await;
        assert_eq!(first_summary_of(&store, "rule"), "shrunk");
    }

    /// While a runner owns the gate loop, `refresh()` marks pending
    /// instead of spawning — the burst is served by the runner's loop tail.
    #[tokio::test]
    async fn refresh_marks_pending_while_a_runner_is_in_flight() {
        let (db, _db_path) = temp_db();
        let store = standalone_for_test(db);
        store
            .0
            .refresh_running
            .store(true, std::sync::atomic::Ordering::SeqCst);
        for _ in 0..5 {
            store.refresh();
        }
        assert!(
            store
                .0
                .refresh_pending
                .load(std::sync::atomic::Ordering::SeqCst),
            "the burst must mark pending, not spawn five runners"
        );
    }

    /// Facts read off a torn (unterminated) first-user line are never
    /// cached — the completing append may change the very text the row
    /// shows, and the growth-keeps-facts rule would otherwise pin the
    /// mid-write wording forever.
    #[tokio::test]
    async fn reconcile_does_not_cache_torn_tail_facts() {
        let (db, _db_path) = temp_db();
        let store = standalone_for_test(db.clone());
        let dir = store.read(|s| s.sessions_dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        let header = r#"{"type":"session","version":4,"id":"torn","timestamp":"2026-09-13T00:00:00Z","cwd":"/proj","metadata":{"host":"manox","thread":"torn"}}"#;
        let torn_line = r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-09-13T00:00:01Z","seq":0,"message":{"role":"user","content":[{"type":"text","text":"mid-write"}],"timestamp":1770000000000}}"#;
        std::fs::write(dir.join("torn.jsonl"), format!("{header}\n{torn_line}")).unwrap();

        store.refresh_now().await;
        assert_eq!(first_summary_of(&store, "torn"), "mid-write");

        // The line completes with DIFFERENT text and padding (the file only
        // grew — the case the fact layer would treat as append-only).
        let completed = r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-09-13T00:00:01Z","seq":0,"message":{"role":"user","content":[{"type":"text","text":"completed wording, settled"}],"timestamp":1770000000000}}"#;
        std::fs::write(dir.join("torn.jsonl"), format!("{header}\n{completed}  \n")).unwrap();
        store.refresh_now().await;
        assert_eq!(
            first_summary_of(&store, "torn"),
            "completed wording, settled",
            "a torn read must not have been pinned by the fact layer"
        );
    }

    /// The negative cache: a file that fails its bounded scan (a
    /// headerless zombie) is fingerprinted and skipped silently until it
    /// changes — and a file that HEALS (a valid rewrite wearing a different
    /// fingerprint) comes back. The failure never swallows the recovery.
    #[tokio::test]
    async fn reconcile_negative_cache_skips_zombies_until_they_heal() {
        let (db, _db_path) = temp_db();
        let store = standalone_for_test(db.clone());
        let dir = store.read(|s| s.sessions_dir.clone());
        std::fs::create_dir_all(&dir).unwrap();
        // A headerless zombie (the shape a pre-fix bug left on real stores).
        std::fs::write(dir.join("zombie.jsonl"), "not a session header\n").unwrap();
        write_session_fixture(&dir, "alive", "hello");

        store.refresh_now().await;
        assert!(
            !store.read(|s| s.summaries().iter().any(|r| r.id == "zombie")),
            "the zombie yields no row"
        );
        assert_eq!(first_summary_of(&store, "alive"), "hello");

        // The zombie heals into a valid session (different fingerprint): the
        // next reconcile must scan it and surface the row.
        write_session_fixture(&dir, "zombie", "recovered");
        store.refresh_now().await;
        assert_eq!(
            first_summary_of(&store, "zombie"),
            "recovered",
            "a healed file must not stay swallowed by the negative cache"
        );
    }

    /// The cold layer: a refresh persists the index to the db, and a fresh
    /// store over the same directory + db rebuilds identical rows from the
    /// seeded cache — a restart costs the stat sweep, not the scans.
    #[tokio::test]
    async fn reconcile_persists_the_index_and_seeds_a_restart() {
        let (db, _db_path) = temp_db();
        let store = standalone_for_test(db.clone());
        let dir = store.read(|s| s.sessions_dir.clone());
        write_session_fixture(&dir, "persist", "cold start prompt");

        store.refresh_now().await;
        let rows = db.load_session_index().unwrap();
        assert!(
            rows.iter().any(|row| row.session_id == "persist"),
            "the index row landed in the db: {rows:?}"
        );

        // A "restarted" store: fresh handle, same sessions dir + db.
        let restarted = StoreHandle::new(ThreadStore {
            summaries: Vec::new(),
            archived_summaries: Vec::new(),
            session_paths: HashMap::new(),
            known_projects: db.list_projects().unwrap_or_default(),
            order: crate::sidebar_order::SidebarOrder::default(),
            order_dirty: false,
            db,
            running: HashSet::new(),
            pending_auth: HashSet::new(),
            pending_plan: HashSet::new(),
            background_work: HashSet::new(),
            live_threads: HashMap::new(),
            sessions_dir: dir,
            decision_overlay: HashMap::new(),
            pending_events: Vec::new(),
            pending_meta_writes: Vec::new(),
        });
        restarted.refresh_now().await;
        assert_eq!(first_summary_of(&restarted, "persist"), "cold start prompt");
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
            superseded_by: None,
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

        let mut index = SessionIndex::default();
        let rows = reconcile_summaries(sessions, None, &mut index).await;
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

    fn sample_entry(id: &str, metadata: Option<serde_json::Value>) -> IndexedSession {
        IndexedSession {
            size: 10,
            mtime_ns: 1,
            failed: false,
            id: id.to_string(),
            cwd: "/p".to_string(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata,
            first_user_text: "hi".to_string(),
            has_messages: true,
            sidecar_size: 0,
            sidecar_mtime_ns: 0,
            sidecar: None,
        }
    }

    fn cached_summary(
        entry: &IndexedSession,
        meta: &manox_harness::session_meta::SessionMeta,
    ) -> ThreadSummary {
        summary_from_cached(entry, entry.created_at, meta)
    }

    #[test]
    fn summary_prefers_team_parent_over_fork_lineage() {
        let mut entry = sample_entry(
            "member",
            Some(serde_json::json!({ "team": { "parent": "leader" } })),
        );
        entry.parent_session_path = Some("fork-source".to_string());
        let summary = cached_summary(&entry, &manox_harness::session_meta::SessionMeta::default());
        assert_eq!(summary.parent_id.as_deref(), Some("leader"));
    }

    #[test]
    fn summary_falls_back_to_fork_parent_without_team_key() {
        let mut entry = sample_entry("forked", Some(serde_json::json!({ "host": "manox" })));
        entry.parent_session_path = Some("source".to_string());
        let summary = cached_summary(&entry, &manox_harness::session_meta::SessionMeta::default());
        assert_eq!(summary.parent_id.as_deref(), Some("source"));
    }

    #[test]
    fn summary_project_prefers_sidecar_over_cwd() {
        let entry = sample_entry("s", None);
        let meta = manox_harness::session_meta::SessionMeta {
            project: Some("/proj/a".into()),
            ..Default::default()
        };
        let summary = cached_summary(&entry, &meta);
        assert_eq!(summary.project, "/proj/a");
        // Without a bound project the header cwd classifies the row.
        let default = manox_harness::session_meta::SessionMeta::default();
        let summary = cached_summary(&entry, &default);
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
        crate::runtime::init_hermetic_for_test();
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
        crate::runtime::init_hermetic_for_test();
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
            superseded_by: None,
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

    /// K3 (L3) regression: a pin / archive decision on a thread with no
    /// live engine journals its `pinned_archived` entry through the cold
    /// storage append — the decision-point entry lands on the session's
    /// chain (authority for the K2 rebuild), not only in the sidecar
    /// cache. Covers the cascade too: archiving a lead journals an entry
    /// for every descendant row that actually moves.
    #[test]
    fn pin_and_archive_decisions_cold_append_pinned_archived_entries() {
        let (db, db_path) = temp_db();
        crate::runtime::init_hermetic_for_test();
        let dir = tempfile::tempdir().unwrap();
        // Real session files (v3 headers: the cold append rides the lazy
        // v3→v4 migration like any other writer). The child carries its
        // team edge in the header so the post-write rescans (which rebuild
        // the summaries from disk) keep the cascade link.
        let write_session = |id: &str, metadata: &str| {
            let path = dir.path().join(format!("{id}.jsonl"));
            std::fs::write(
                &path,
                format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/p\",\"metadata\":{metadata}}}\n"),
            )
            .unwrap();
            path
        };
        let lead = write_session("k3-lead", "{\"host\":\"manox\"}");
        let child = write_session(
            "k3-child",
            "{\"host\":\"manox\",\"team\":{\"parent\":\"k3-lead\"}}",
        );
        let store = store_handle(db.clone());
        store.with_mut(|s| {
            s.sessions_dir = dir.path().to_path_buf();
            s.session_paths.insert("k3-lead".to_string(), lead.clone());
            s.session_paths
                .insert("k3-child".to_string(), child.clone());
            s.insert_summary_for_test("k3-lead", None);
            s.insert_summary_for_test("k3-child", Some("k3-lead"));
        });

        // The pinned_archived entries each decision must land, in order.
        let entries_for = |path: &std::path::Path| -> Vec<(bool, bool)> {
            crate::runtime::handle()
                .block_on(async {
                    let storage = manox_harness::session::jsonl::JsonlSessionStorage::open(path)
                        .await
                        .unwrap();
                    storage.journal_range(0, u64::MAX).await.unwrap()
                })
                .into_iter()
                .filter_map(|record| match record.entry {
                    manox_harness::session::SessionTreeEntry::PinnedArchived {
                        pinned,
                        archived,
                        ..
                    } => Some((pinned, archived)),
                    _ => None,
                })
                .collect()
        };
        let wait_for_entries = |path: &std::path::Path, count: usize| {
            for _ in 0..1500 {
                let entries = entries_for(path);
                if entries.len() >= count {
                    return entries;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!(
                "the decision-point entry never landed in {}: {:?}",
                path.display(),
                entries_for(path)
            );
        };

        // Pin: one entry carrying BOTH flags (the pair fully re-establishes
        // the state on rebuild), appended while the sidecar cache follows.
        store.with_mut(|s| s.pin_thread("k3-lead", true));
        let pinned = wait_for_entries(&lead, 1);
        assert_eq!(
            pinned,
            vec![(true, false)],
            "pin journals {{pinned:true, archived:false}}"
        );

        // Archive cascades: the lead AND the child each journal their own
        // entry, carrying the pinned flag the summary mirror holds.
        store.with_mut(|s| s.archive_thread("k3-lead", true));
        let lead_entries = wait_for_entries(&lead, 2);
        assert_eq!(
            lead_entries[1],
            (true, true),
            "the cascade entry carries the full post-decision flag pair"
        );
        let child_entries = wait_for_entries(&child, 1);
        assert_eq!(
            child_entries[0],
            (false, true),
            "the child journals its own entry"
        );

        // Re-asserting the current state is a no-op: no duplicate entry.
        store.with_mut(|s| s.archive_thread("k3-lead", true));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(
            entries_for(&lead).len(),
            2,
            "a no-op decision must not journal"
        );
        std::fs::remove_file(db_path).ok();
    }

    /// B3/P0-4 (review round 3): two store decisions on one engine-less
    /// thread each spawn their own cold append — fresh storage instances
    /// whose instance-scoped append locks never see each other. Raced on a
    /// v3 file (where both appends also race the lazy v3→v4 migration,
    /// which used to share one temp name), they must still land a LINEAR
    /// chain: the chain-walking journal_range reads BOTH `pinned_archived`
    /// rows. Pre-fix the pair forked — both rows parented to the same
    /// leaf, each seq self-stamped, the load validator accepts siblings,
    /// and the cursor kept only the file-last branch, so the range never
    /// reached 2 and the earlier decision silently left the chain. Three
    /// rounds: the race window is narrow; the serialization (the engine's
    /// per-path cold-append lock + the rewrite's unique temp name) is what
    /// makes every round converge.
    #[test]
    fn concurrent_cold_appends_land_a_linear_chain() {
        let (db, db_path) = temp_db();
        crate::runtime::init_hermetic_for_test();
        let dir = tempfile::tempdir().unwrap();
        let entries_for = |path: &std::path::Path| -> Vec<(bool, bool)> {
            crate::runtime::handle()
                .block_on(async {
                    let storage = manox_harness::session::jsonl::JsonlSessionStorage::open(path)
                        .await
                        .unwrap();
                    storage.journal_range(0, u64::MAX).await.unwrap()
                })
                .into_iter()
                .filter_map(|record| match record.entry {
                    manox_harness::session::SessionTreeEntry::PinnedArchived {
                        pinned,
                        archived,
                        ..
                    } => Some((pinned, archived)),
                    _ => None,
                })
                .collect()
        };
        let wait_for_entries = |path: &std::path::Path, count: usize| {
            for _ in 0..1500 {
                let entries = entries_for(path);
                if entries.len() >= count {
                    return entries;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!(
                "the racing cold appends never landed a linear chain in {}: {:?}",
                path.display(),
                entries_for(path)
            );
        };
        let store = store_handle(db.clone());
        for round in 0..3u32 {
            let id = format!("k3-race-{round}");
            let path = dir.path().join(format!("{id}.jsonl"));
            std::fs::write(
                &path,
                format!("{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/p\",\"metadata\":{{\"host\":\"manox\"}}}}\n"),
            )
            .unwrap();
            store.with_mut(|s| {
                s.session_paths.insert(id.clone(), path.clone());
                s.insert_summary_for_test(&id, None);
            });
            // Fire BOTH decisions back-to-back: their cold-append tasks
            // race for the same file. The lock serializes but does not
            // order them, so assert the chain, not the sequence: both
            // rows readable through the walk, and the archive's
            // full-state row among them.
            store.with_mut(|s| s.pin_thread(&id, true));
            store.with_mut(|s| s.archive_thread(&id, true));
            let entries = wait_for_entries(&path, 2);
            assert_eq!(
                entries.len(),
                2,
                "round {round}: both decisions readable through the chain (a fork strands one)"
            );
            assert!(
                entries.contains(&(true, true)),
                "round {round}: the archive full-state row landed: {entries:?}"
            );
        }
        std::fs::remove_file(db_path).ok();
    }

    // ── Durable order account ──────────────────────────────────────────────

    /// A store seeded with rows: `(id, project, interacted_at, pinned)`. Every
    /// project a row names is registered first, so partition assignment follows
    /// the same rule production uses.
    ///
    /// Assertions read the in-memory account and the dirty flag inside the
    /// mutation closure: without a runtime the drain cannot spawn a save, so no
    /// test ever touches the real `~/.manox/sidebar.order.json`.
    fn ordered_store(rows: &[(&str, &str, i64, bool)]) -> (StoreHandle, std::path::PathBuf) {
        let (db, db_path) = temp_db();
        let store = store_handle(db);
        store.with_mut(|s| {
            for (id, project, at, pinned) in rows {
                s.insert_summary_with_times_for_test(
                    id,
                    None,
                    *at,
                    *at,
                    project,
                    None,
                    PermissionMode::default().as_i64(),
                );
                if !project.is_empty() && !s.known_projects.iter().any(|p| p == project) {
                    s.known_projects.push(project.to_string());
                }
                if let Some(sum) = s.summary_mut(id) {
                    sum.pinned = *pinned;
                }
            }
            s.rerank();
            s.order_dirty = false;
        });
        (store, db_path)
    }

    fn ids(store: &StoreHandle) -> Vec<String> {
        store.read(ranked_ids)
    }

    /// The same projection against a raw store handle (inside a mutation
    /// closure, where the dirty-flag assertion belongs).
    fn ranked_ids(store: &ThreadStore) -> Vec<String> {
        store.summaries.iter().map(|x| x.id.clone()).collect()
    }

    #[test]
    fn a_later_rescan_with_moved_timestamps_cannot_reorder_the_list() {
        let (store, path) = ordered_store(&[
            ("t1", "/p/a", 100, false),
            ("t2", "/p/a", 200, false),
            ("t3", "", 300, false),
        ]);
        let seeded = store.read(|s| s.order.clone());
        // A background turn lands on every row and the next scan hands the rows
        // over in the opposite arrival order: neither may move a row.
        store.with_mut(|s| {
            s.summaries.reverse();
            for sum in s.summaries.iter_mut() {
                sum.interacted_at += 10_000;
                sum.updated_at += 10_000;
            }
            s.rerank();
            assert!(
                !s.order_dirty,
                "a membership-preserving rescan persists nothing"
            );
        });
        assert_eq!(ids(&store), vec!["t2", "t1", "t3"], "activity moved a row");
        assert_eq!(
            store.read(|s| s.order.clone()),
            seeded,
            "the account churned"
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_thread_move_reorders_the_list_and_dirties_the_account() {
        let (store, path) = ordered_store(&[
            ("t1", "/p/a", 100, false),
            ("t2", "/p/a", 200, false),
            ("t3", "/p/a", 300, false),
        ]);
        assert_eq!(ids(&store), vec!["t3", "t2", "t1"]);
        store.with_mut(|s| {
            s.insert_thread_before("t1", Some("t3"));
            assert!(s.order_dirty, "a real move must persist");
        });
        assert_eq!(ids(&store), vec!["t1", "t3", "t2"]);
        assert_eq!(
            store.read(|s| s.order.accounts["/p/a"].clone()),
            vec!["t1".to_string(), "t3".to_string(), "t2".to_string()],
            "the account must record the move"
        );
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn a_move_is_rejected_when_the_anchor_lives_elsewhere() {
        let (store, path) =
            ordered_store(&[("t1", "/p/a", 100, false), ("t2", "/p/b", 200, false)]);
        let before = ids(&store);
        store.with_mut(|s| {
            s.insert_thread_before("t1", Some("t2"));
            assert_eq!(
                ranked_ids(s),
                before,
                "a cross-partition move must not apply"
            );
            assert!(!s.order_dirty, "a rejected move persists nothing");
        });
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pinning_floats_the_row_to_its_partition_head() {
        // A pin journals its decision and persists the account, both of which
        // need the runtime — so the account file goes to a scratch path.
        crate::runtime::init_hermetic_for_test();
        let scratch = std::env::temp_dir().join(format!("pi-order-{}.json", uuid::Uuid::new_v4()));
        crate::sidebar_order::set_order_path_for_test(Some(scratch.clone()));
        let (store, path) = ordered_store(&[
            ("t1", "/p/a", 300, false),
            ("t2", "/p/a", 200, false),
            ("t3", "/p/a", 100, true),
        ]);
        // A pinned row leads the partition even though it was the oldest row.
        assert_eq!(ids(&store), vec!["t3", "t1", "t2"]);
        store.with_mut(|s| {
            s.pin_thread("t2", true);
            assert!(s.order_dirty, "a pin is an explicit order action");
        });
        // The most recently pinned row leads: a pin floats, it does not merely
        // re-band.
        assert_eq!(ids(&store), vec!["t2", "t3", "t1"]);
        // Unpinning drops the row back to its stored rank.
        store.with_mut(|s| s.pin_thread("t2", false));
        assert_eq!(ids(&store), vec!["t3", "t2", "t1"]);
        std::fs::remove_file(path).ok();
        std::fs::remove_file(&scratch).ok();
        crate::sidebar_order::set_order_path_for_test(None);
    }

    #[test]
    fn a_new_row_surfaces_at_the_head_of_its_partition() {
        let (store, path) = ordered_store(&[("t1", "/p/a", 100, false)]);
        store.with_mut(|s| {
            s.insert_summary_with_times_for_test(
                "t9",
                None,
                900,
                900,
                "/p/a",
                None,
                PermissionMode::default().as_i64(),
            );
            s.rerank();
        });
        assert_eq!(ids(&store), vec!["t9", "t1"]);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn folders_follow_the_committed_order_and_the_registry_projects_it() {
        let (store, path) = ordered_store(&[
            ("t1", "/p/a", 100, false),
            ("t2", "/p/b", 200, false),
            ("t3", "", 300, false),
        ]);
        assert_eq!(
            store.read(|s| s.known_projects().to_vec()),
            vec!["/p/a".to_string(), "/p/b".to_string()]
        );
        store.with_mut(|s| s.insert_group_before("/p/b", Some("/p/a")));
        assert_eq!(
            store.read(|s| s.known_projects().to_vec()),
            vec!["/p/b".to_string(), "/p/a".to_string()]
        );
        // Loose rows stay last regardless of folder order.
        assert_eq!(ids(&store), vec!["t2", "t1", "t3"]);
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn archiving_leaves_the_account_and_unarchiving_surfaces_at_the_head() {
        // Archiving journals its decision and persists the account, both of
        // which need the runtime — so the account file goes to a scratch path.
        crate::runtime::init_hermetic_for_test();
        let scratch = std::env::temp_dir().join(format!("pi-order-{}.json", uuid::Uuid::new_v4()));
        crate::sidebar_order::set_order_path_for_test(Some(scratch.clone()));
        let (store, path) =
            ordered_store(&[("t1", "/p/a", 100, false), ("t2", "/p/a", 200, false)]);
        store.with_mut(|s| s.archive_thread("t1", true));
        assert_eq!(ids(&store), vec!["t2"]);
        assert!(
            !store.read(|s| s.order.accounts["/p/a"].contains(&"t1".to_string())),
            "an archived row leaves the account"
        );
        store.with_mut(|s| s.archive_thread("t1", false));
        // A row that returns has no stored rank, so it surfaces at the head.
        assert_eq!(ids(&store), vec!["t1", "t2"]);
        std::fs::remove_file(path).ok();
        std::fs::remove_file(&scratch).ok();
        crate::sidebar_order::set_order_path_for_test(None);
    }
}
