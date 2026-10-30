//! The `ThreadStore` facade — the session-list state the sidebar renders.
//!
//! The sidebar's session list comes from the session repository (jsonl)
//! plus a per-session UI-metadata sidecar (`manox_harness::session_meta`).
//! The transcript persists itself, so `refresh_thread_list` only refreshes
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
use crate::thread::{AccessMode, Thread, ThreadCore, ThreadHandle, ThreadId};

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
    /// Delegated child sessions (the run rows: header `metadata.subagent`),
    /// partitioned out of the sidebar the same way archived rows are. They
    /// never rank in the sidebar order account — the catalogue walk
    /// (`listSessions`) is their consumer, and it annotates them with the
    /// container edges (parent + kind) clients fold into container trees.
    child_summaries: Vec<ThreadSummary>,
    /// Session file path per summary id, for sidecar writes and reopen.
    session_paths: HashMap<String, PathBuf>,
    known_projects: Vec<String>,
    /// The alias half of each project object, keyed by path
    /// (`known_projects` holds the ordered set; this holds the display
    /// name).
    project_aliases: HashMap<String, String>,
    /// The durable sidebar order: folder sequence plus one thread account per
    /// partition. Authoritative over `summaries`' order — see
    /// [`crate::sidebar_order`].
    order: crate::sidebar_order::SidebarOrder,
    /// Set when `order` diverged from its persisted form; `with_mut`'s drain
    /// writes and clears it outside the state lock.
    order_dirty: bool,
    /// Host db handle persisting `known_projects` (shared threads.db,
    /// `projects` table only — thread rows remain the manox store's domain).
    /// The project-registry file under the state root (the db table's
    /// successor): one project path per line, rewritten on change.
    projects_path: PathBuf,
    running: HashSet<String>,
    /// Threads with an interaction pending a user answer (a parked
    /// thread's question card is not visible, so the sidebar badge is the
    /// only signal until the user switches back). In-memory only: cleared on
    /// attach, on terminal events, and when the run resumes past the call.
    pending_auth: HashSet<String>,
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
    /// Working-directory grants held in memory until the session's journal
    /// materializes: writing a sidecar for a session with no journal orphans
    /// a meta with no log (the v1 black-hole class). Flushed the moment the
    /// journal file appears.
    pending_working_dirs: HashMap<String, Vec<String>>,
    /// View stamps observed before the session's sidecar path materialized
    /// (a brand-new session the AHP face focused before its first scan):
    /// id → stamp. Written to the sidecar the moment the path appears, and
    /// merged into the derived bit until a scan OBSERVES the stamp on disk
    /// (the `decision_overlay` discipline — a scan that read a stale
    /// sidecar must never publish over a fresher in-memory stamp).
    pending_viewed: HashMap<String, i64>,
    /// Lowerings awaiting their durable write (`mark_unread`): id → the view
    /// stamp the disk held when the lowering was made. The sentinel
    /// `last_viewed_at = Some(0)` is forced over the scan merge while the
    /// disk still shows that prior value; a max-merge cannot hold a stamp
    /// BELOW the scanned one (that is why `pending_viewed` cannot serve this
    /// leg), so the overlay is applied AFTER the merge. It retires as soon as
    /// the disk moves — to the sentinel (this write) or to anything else (a
    /// peer's later view, which wins) — so it can never pin a row unread.
    pending_unread: HashMap<String, Option<i64>>,
    /// Set once the first reconcile has landed. The broadcast gate keys on
    /// a real row-set change, but the very first pass must announce — the
    /// subscribers start with empty partitions.
    scanned_once: bool,
}

/// One queued sidecar write, drained by [`StoreHandle::with_mut`] once the
/// state lock releases and dispatched on the agent runtime.
struct MetaWrite {
    dir: PathBuf,
    path: PathBuf,
    update: Box<dyn FnOnce(&mut manox_harness::session_meta::SessionMeta) + Send + Sync + 'static>,
}

/// The weak half of [`StoreHandle`], for long-lived pollers: the peer
/// sweep holds this and retires when the core it points at drops (a
/// replaced global, a test teardown) instead of pinning every stale core
/// forever — `init` runs more than once in a process's lifetime.
#[derive(Clone)]
pub struct WeakStoreHandle(std::sync::Weak<StoreCore>);

impl WeakStoreHandle {
    /// Upgrade; `None` once the store core is gone — the poller exits then.
    pub fn upgrade(&self) -> Option<StoreHandle> {
        self.0.upgrade().map(StoreHandle)
    }
}

/// The gpui-free handle to the thread store. Cheap to clone (`Arc`); state
/// lives behind a lock and events broadcast to channel subscribers. This is
/// the kernel-side unit the AgentServer and (transitionally) the frontends
/// hold.
#[derive(Clone)]
pub struct StoreHandle(Arc<StoreCore>);

pub struct StoreCore {
    state: parking_lot::RwLock<ThreadStore>,
    /// The sessions dir this store scans, frozen at construction — read
    /// WITHOUT the state lock (`core_root` resolves through this and is
    /// called from inside `with_mut` closures; a locked read would
    /// re-enter on the same thread).
    dir: PathBuf,
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
        let dir = thread_store.sessions_dir.clone();
        Self(Arc::new(StoreCore {
            state: parking_lot::RwLock::new(thread_store),
            dir,
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

    /// The weak view for long-lived pollers (the catalogue sweep). A strong
    /// handle there would pin every stale core: `init` runs more than once
    /// in a process's lifetime, and a replaced global must let the old
    /// loop die.
    pub fn downgrade(&self) -> WeakStoreHandle {
        WeakStoreHandle(Arc::downgrade(&self.0))
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
        let registry = crate::thread_registry::load().await;
        let order = crate::sidebar_order::load().await;
        let (rows, child_rows) = {
            let mut index = self.0.index.lock().await;
            reconcile_summaries(&dir, &mut index).await
        };
        self.with_mut(|s| {
            let (session_paths, summaries, mut archived, mut children) =
                group_by_thread(rows, child_rows, &registry);
            let mut summaries = summaries;
            resolve_depths(&mut summaries);
            // The scan read the sidecars as of the pass's head; stamps this
            // process holds beyond that (a queued view write not yet
            // observed, a settle stamped in memory ahead of its disk write)
            // must not be published over. Merge the old in-memory rows'
            // monotone inputs into the fresh rows, then the parked views on
            // top; a park whose stamp the disk now carries retires.
            let mut held: HashMap<String, (Option<i64>, Option<i64>)> = HashMap::new();
            for row in s
                .summaries
                .iter()
                .chain(s.archived_summaries.iter())
                .chain(s.child_summaries.iter())
            {
                held.insert(row.id.clone(), (row.last_settled_at, row.last_viewed_at));
            }
            // Archived rows join the merge: a view parked before an
            // archive must still retire against the disk, or it lingers
            // (and re-fires its sidecar write) forever. Child rows join
            // too: their run may settle (a disk stamp) while no sidebar
            // surface ever focused them.
            for summary in summaries
                .iter_mut()
                .chain(archived.iter_mut())
                .chain(children.iter_mut())
            {
                // The scan's own value, before the held/park merges: the
                // lowering overlay below retires only against THIS (the disk),
                // never against an in-memory stamp.
                let scanned_viewed = summary.last_viewed_at;
                if let Some((settled, viewed)) = held.get(&summary.id) {
                    summary.merge_stamps(*settled, *viewed);
                }
                // An explicit mark-unread is a LOWERING: force the sentinel
                // over the max merge until the disk carries it, or a pass
                // that read the sidecar before the queued write landed would
                // republish the old view.
                if let Some(prior) = s.pending_unread.get(&summary.id).copied() {
                    if scanned_viewed == prior {
                        summary.last_viewed_at = Some(0);
                        summary.has_unread =
                            crate::db::derive_unread(summary.last_settled_at, Some(0));
                    } else {
                        // The disk moved (this sentinel's write landed, or a
                        // peer wrote a later view): the overlay has done its
                        // job and must never outlive the race it covers.
                        s.pending_unread.remove(&summary.id);
                    }
                }
                let Some(parked) = s.pending_viewed.get(&summary.id).copied() else {
                    continue;
                };
                if summary.last_viewed_at.is_some_and(|at| at >= parked) {
                    s.pending_viewed.remove(&summary.id);
                } else {
                    summary.merge_stamps(None, Some(parked));
                    // The sidecar path materialized since the view was
                    // parked: the stamp can finally land on disk.
                    if session_paths.contains_key(&summary.id)
                        && !s.session_paths.contains_key(&summary.id)
                    {
                        s.persist_view_stamp(&summary.id, parked);
                    }
                }
            }
            // Broadcast only on a real change: a quiet pass (a sweep tick
            // over an idle store, a fingerprint-identical rescan) must not
            // re-rank sidebars or re-announce the catalogue. The comparison
            // is order-INSENSITIVE on the row partitions — the fresh rows
            // walk a HashMap while the held rows came out of the last
            // pass's rerank, and reading different orders of one set as
            // "changed" would turn every sweep tick into a re-announce.
            // The first pass announces unconditionally — the subscribers
            // start empty.
            let changed = s.scanned_once
                && (!rows_match_ignoring_order(&s.summaries, &summaries)
                    || !rows_match_ignoring_order(&s.archived_summaries, &archived)
                    || !rows_match_ignoring_order(&s.child_summaries, &children)
                    || s.session_paths != session_paths);
            s.session_paths = session_paths;
            s.flush_materialized_working_dirs();
            s.summaries = summaries;
            s.archived_summaries = archived;
            s.child_summaries = children;
            // The persisted account is the authority over the row order; the
            // scan's timestamp order is only ever an input to first-sight.
            s.order = order;
            s.rerank();
            s.apply_decision_overlay();
            if changed || !s.scanned_once {
                s.pending_events.push(ThreadStoreEvent::SummariesUpdated);
            }
            s.scanned_once = true;
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

/// Resolve the session directory under the manox config dir.
/// One durable project object: the directory plus the user's display
/// alias. The list IS the object — a project with every conversation
/// archived stays right here.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProjectEntry {
    pub path: String,
    /// User-facing display name; `None` renders the directory segment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

/// The projects file under the state root: a JSON array of
/// [`ProjectEntry`], written in committed order (newest first; manual
/// moves rewrite the order). The order is meaningful and preserved — the
/// successor of the sqlite `projects` table and the sorted registry lines.
fn read_projects_file(path: &std::path::Path) -> Vec<ProjectEntry> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_projects_file(path: &std::path::Path, projects: &[ProjectEntry]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(projects)?)
}

pub(crate) fn sessions_dir() -> PathBuf {
    // The v1.0.0 core root: per-session directories under project buckets.
    // Computed DIRECTLY (never through `paths::core_root`, which resolves
    // through this store — re-entering the global during store construction
    // deadlocks).
    crate::paths::manox_config_dir()
        .map(|dir| dir.join("core"))
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Open the session directory, seed the summary list, and register the
/// process-global handle. Call at App startup.
pub fn init() {
    let dir = sessions_dir();
    let projects_path = dir.join("projects.json");
    let projects = read_projects_file(&projects_path);
    let known_projects = projects.iter().map(|p| p.path.clone()).collect();
    let project_aliases = projects
        .into_iter()
        .filter_map(|p| p.alias.map(|alias| (p.path, alias)))
        .collect();
    let handle = StoreHandle::new(ThreadStore {
        summaries: Vec::new(),
        archived_summaries: Vec::new(),
        child_summaries: Vec::new(),
        session_paths: HashMap::new(),
        known_projects,
        project_aliases,
        order: crate::sidebar_order::SidebarOrder::default(),
        order_dirty: false,
        running: HashSet::new(),
        pending_auth: HashSet::new(),
        live_threads: HashMap::new(),
        sessions_dir: dir,
        projects_path,
        decision_overlay: HashMap::new(),
        pending_events: Vec::new(),
        pending_meta_writes: Vec::new(),
        pending_working_dirs: HashMap::new(),
        pending_viewed: HashMap::new(),
        pending_unread: HashMap::new(),
        scanned_once: false,
    });
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

/// What a rename did.
///
/// Three outcomes, not a bool: a caller has to tell "the title was rejected"
/// from "the title could not be made durable", because only the first is the
/// client's fault and only the second is recoverable by retrying later. Folding
/// them into `false` made an AHP refusal report the blank-title message for a
/// perfectly valid title.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameOutcome {
    /// The title is stored and both durable legs accepted it.
    Renamed,
    /// Nothing to rename: the title was empty or whitespace-only.
    Blank,
    /// No such session in the store.
    UnknownSession,
    /// The row was updated in memory, but the journal row had nowhere to land
    /// (no live engine and no session file) — the rename is not durable, so the
    /// caller must not report success.
    NotPersisted,
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

    /// Delegated child sessions (the run rows), partitioned out of the
    /// sidebar the same way archived rows are. The catalogue is their
    /// consumer: each row carries the container edges (`parent_id` names
    /// the owning thread row, `subagent` the kind) clients fold into
    /// container trees.
    pub fn child_summaries(&self) -> &[ThreadSummary] {
        &self.child_summaries
    }

    /// Mutable lookup across every partition by id.
    fn summary_mut(&mut self, id: &str) -> Option<&mut ThreadSummary> {
        self.summaries
            .iter_mut()
            .find(|s| s.id == id)
            .or_else(|| self.archived_summaries.iter_mut().find(|s| s.id == id))
            .or_else(|| self.child_summaries.iter_mut().find(|s| s.id == id))
    }

    /// Immutable lookup across every partition by id.
    /// The summary mirror row for one session id (sidecar-merged truth).
    pub fn summary_by_id(&self, id: &str) -> Option<&ThreadSummary> {
        self.summaries
            .iter()
            .find(|s| s.id == id)
            .or_else(|| self.archived_summaries.iter().find(|s| s.id == id))
            .or_else(|| self.child_summaries.iter().find(|s| s.id == id))
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
    /// account against live membership (dead ids and emptied partitions
    /// drop), then lay the rows out as folder sequence → pinned band →
    /// last-human-interaction rank (the account owns membership and the
    /// folder order; the interaction stamp ranks rows inside a folder).
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
            child_summaries,
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
        // The child partition owns none either and gets the same rule —
        // the catalogue walks it row-wise, order is presentation only.
        archived_summaries.sort_by(|a, b| {
            b.interacted_at
                .cmp(&a.interacted_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        child_summaries.sort_by(|a, b| {
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

    /// A project's display alias, when the user set one.
    pub fn project_alias(&self, path: &str) -> Option<&str> {
        self.project_aliases.get(path).map(String::as_str)
    }

    /// Set (or clear, on `None`) a project's display alias. The project
    /// object is durable whether or not it holds live conversations; an
    /// unknown path is refused — aliasing an unregistered directory would
    /// mint an object nobody registered.
    pub fn set_project_alias(&mut self, path: &str, alias: Option<String>) -> bool {
        if !self.known_projects.iter().any(|p| p == path) {
            return false;
        }
        match alias {
            Some(alias) if !alias.trim().is_empty() => {
                self.project_aliases.insert(path.to_string(), alias);
            }
            _ => {
                self.project_aliases.remove(path);
            }
        }
        self.persist_projects();
        self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
        true
    }

    /// Persist the project objects — the ordered path list plus the alias
    /// map — as one array write.
    fn persist_projects(&self) {
        let projects: Vec<ProjectEntry> = self
            .known_projects
            .iter()
            .map(|path| ProjectEntry {
                path: path.clone(),
                alias: self.project_aliases.get(path).cloned(),
            })
            .collect();
        if let Err(e) = write_projects_file(&self.projects_path, &projects) {
            tracing::warn!(error = %e, "failed to persist the projects file");
        }
    }

    /// The committed sidebar order: folder order plus each partition's ordered
    /// thread ids.
    ///
    /// The account the user's manual moves write. A client that only *displays*
    /// the sidebar needs this to render what the moves produced; the file's
    /// other consumers go through `reconcile`, which additionally intersects
    /// with the rows the store actually holds.
    pub fn sidebar_order(&self) -> (&[String], &std::collections::HashMap<String, Vec<String>>) {
        (&self.order.groups, &self.order.accounts)
    }
    /// Register a project path: in-memory list + persisted to the db
    /// `projects` table so sidebar folders survive restarts even when all
    /// their threads are archived.
    pub fn register_project(&mut self, path: String) {
        if path.is_empty() || self.known_projects.contains(&path) {
            return;
        }
        // Newest project first: an uncommitted folder order renders the
        // most recently added directory at the top of the section.
        self.known_projects.insert(0, path.clone());
        self.persist_projects();
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
        self.project_aliases.remove(path);
        self.persist_projects();
        // The folder leaves the account; its rows re-partition as loose.
        self.rerank();
        self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
    }

    /// Whether the given thread id is currently running a turn.
    pub fn is_running(&self, id: &str) -> bool {
        self.running.contains(id)
    }

    /// Mark a thread as running (turn started).
    ///
    /// The running set feeds the status bits the catalogue serves
    /// (`row_status_bits`), so a change also announces the summaries — the
    /// `RunningChanged` event alone would leave the list's `InProgress` bit
    /// stale until some unrelated write asked for a pass.
    pub fn mark_running(&mut self, id: &str) {
        if self.running.insert(id.to_string()) {
            self.pending_events.push(ThreadStoreEvent::RunningChanged);
            self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
        }
    }

    /// Mark a thread as idle (turn ended). See [`Self::mark_running`] for why
    /// the summaries ride along.
    pub fn mark_idle(&mut self, id: &str) {
        if self.running.remove(id) {
            self.pending_events.push(ThreadStoreEvent::RunningChanged);
            self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
        }
    }

    /// A turn settled: record the stamp and re-derive the attention bit.
    /// Monotone (max); returns whether the bit flipped — the sidebar
    /// re-renders then. A row this store has not scanned yet no-ops here:
    /// the host's own sidecar write (independent of this store) lands the
    /// stamp on disk, and the next scan derives from it.
    pub fn note_settled(&mut self, id: &str, at: i64) -> bool {
        let Some(summary) = self.summary_mut(id) else {
            return false;
        };
        if summary.merge_stamps(Some(at), None) {
            self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
            true
        } else {
            false
        }
    }

    /// A view observation from the AHP face (`isReadChanged(true)`):
    /// monotone, idempotent, never lowers the stamp. Rows whose sidecar
    /// path has not materialized yet (a brand-new session focused before
    /// its first scan) park the stamp in memory — [`Self::refresh_now`]
    /// lands it on disk the moment the path appears. Returns whether the
    /// attention bit flipped.
    pub fn mark_viewed(&mut self, id: &str, at: i64) -> bool {
        // A view supersedes an explicit mark-unread: the viewer is looking
        // now, and the queued lowering must not force the sentinel back over
        // this stamp on the next scan.
        self.pending_unread.remove(id);
        let flipped = match self.summary_mut(id) {
            Some(summary) => {
                let flipped = summary.merge_stamps(None, Some(at));
                if self.session_paths.contains_key(id) {
                    self.persist_view_stamp(id, at);
                }
                flipped
            }
            None => false,
        };
        let parked = self.pending_viewed.entry(id.to_string()).or_insert(0);
        *parked = (*parked).max(at);
        if flipped {
            self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
        }
        flipped
    }

    /// Queue the view stamp's sidecar write: max against disk, never a
    /// lowering (a peer's fresher stamp survives a stale dispatch).
    fn persist_view_stamp(&mut self, id: &str, at: i64) {
        self.write_meta(id, move |meta| {
            meta.last_viewed_at = Some(meta.last_viewed_at.map_or(at, |prev| prev.max(at)));
        });
    }

    /// The explicit "mark unread" leg (`isReadChanged(false)`): lower the
    /// view stamp to the sentinel so every recorded settle reads as
    /// un-viewed. The PERSIST is the point — the reducer already folded the
    /// action for every subscriber, and a fold the disk will not confirm is
    /// exactly the never-happened state the fail-closed rule forbids. A
    /// session with no settled turn reads clear afterwards (nothing to
    /// mark): the derivation says so, not a second stored bit.
    pub fn mark_unread(&mut self, id: &str) -> bool {
        // A lowering is not monotone: hold the sentinel over the next scan
        // merge until the disk moves (see `pending_unread`). The prior stamp
        // is what tells a later pass whether the disk still holds the value
        // this lowering replaced.
        let prior = self
            .summary_mut(id)
            .and_then(|summary| summary.last_viewed_at);
        self.pending_unread.insert(id.to_string(), prior);
        // An explicit unread supersedes a view this process has parked but
        // not yet landed; keeping the park would re-raise the stamp below.
        self.pending_viewed.remove(id);
        let flipped = match self.summary_mut(id) {
            Some(summary) => {
                summary.last_viewed_at = Some(0);
                let was = summary.has_unread;
                summary.has_unread =
                    crate::db::derive_unread(summary.last_settled_at, summary.last_viewed_at);
                let flipped = summary.has_unread != was;
                if self.session_paths.contains_key(id) {
                    self.write_meta(id, |meta| meta.last_viewed_at = Some(0));
                }
                flipped
            }
            None => false,
        };
        // No park for the lowering: the park's merge is a monotone max and
        // retires as soon as `last_viewed_at >= parked`, so it cannot hold a
        // stamp BELOW the scanned one. A pass that read the sidecar before
        // this queued write lands may republish the old view for one pass;
        // the write's next scan flips it back. The durable write is the
        // authority either way — a second in-memory overlay would be a
        // second read truth for a leg with no caller today.
        if flipped {
            self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
        }
        flipped
    }

    /// Rename a session: the user's title, durable in the journal and the
    /// sidecar.
    ///
    /// Three legs, and a rename needs all three:
    ///
    /// - **the journal** ([`Self::journal_title`]) carries `SessionTreeEntry::Title`,
    ///   which the v2 title projection folds (`projections.rs`) and the AHP
    ///   translator reads (`translate/actions.rs`). It is the only route
    ///   either takes: the live `ThreadEvent::TitleChanged` is on
    ///   `translate.rs`'s `Skip` list precisely because the journal row is its
    ///   durable form.
    /// - **the in-memory summary** updates the `title_override` slot that
    ///   [`ThreadSummary::display_title`] reads first, so a rename outranks a
    ///   model-generated title immediately.
    /// - **the sidecar** makes that survive a restart without a rescan.
    ///
    /// An empty or whitespace-only title is not a rename; it is refused so a
    /// client cannot blank out a session's name by sending nothing.
    ///
    /// # Why this journals explicitly
    ///
    /// A store-level decision runs *outside* the engine's journal tap: the tap
    /// sits between an engine emission site and the facade, and consuming a
    /// notice at the facade does not persist it. So any store-side change that
    /// clients must see has to append its own row — the same reason
    /// [`Self::journal_pinned_archived`] exists. (The tap's
    /// "no future emission site can forget to persist" guarantee holds for
    /// *emission* sites only, not for injection points like this one.)
    pub fn rename_thread(&mut self, id: &str, title: &str) -> RenameOutcome {
        let title = title.trim();
        if title.is_empty() {
            return RenameOutcome::Blank;
        }
        // A session the store knows only by path is still known: a
        // freshly-created session gets its id→path note immediately but no
        // summary row until a list refresh scans the new file. Renaming it must
        // work — refusing here answered "unknown session" for the session the
        // client had just created. The durable legs go through the path; only
        // the sidebar mirror needs the row, and it catches up on the refresh.
        let known = self.summary_mut(id).is_some() || self.session_paths.contains_key(id);
        if !known {
            return RenameOutcome::UnknownSession;
        }
        if let Some(summary) = self.summary_mut(id) {
            if summary.title_override.as_deref() == Some(title) {
                return RenameOutcome::Renamed;
            }
            summary.title_override = Some(title.to_string());
        }
        // The durable legs. Both are best-effort against a session another
        // process may be driving (the write lease is per session), so the
        // outcome reports whether they actually landed rather than assuming
        // they did — a caller that answered "accepted" on a rename that was
        // silently dropped would be lying to every subscriber.
        let journaled = self.journal_title(id, title);
        let owned = title.to_string();
        self.write_meta(id, move |meta| meta.title = Some(owned));
        if journaled {
            RenameOutcome::Renamed
        } else {
            RenameOutcome::NotPersisted
        }
    }

    /// Append the durable `title` row for a rename.
    ///
    /// The kind and payload match what the engine's own `TitleChanged`
    /// emission produces (`durable_journal_payload`), so a user rename and a
    /// model-generated title land as the *same* row shape — a reader cannot
    /// tell them apart, which is correct: both are the session's title.
    ///
    /// Returns whether a row could be queued at all. A session with no file, or
    /// one whose write lease another process holds, cannot take the row: the
    /// append is skipped (loudly, at the seam) and the caller is told, so it can
    /// refuse the rename instead of reporting success.
    fn journal_title(&self, id: &str, title: &str) -> bool {
        // A session whose journal has not materialized yet has nowhere for the
        // row to land. A live engine route is not enough: a queued
        // `AppendJournal` to an engine that never opened a file is dropped
        // without a trace, so the row would be reported as placed and never
        // exist. The file's presence is the honest precondition.
        if !self.session_paths.get(id).is_some_and(|path| path.exists()) {
            return false;
        }
        crate::engine::dispatch_store_journal_row(
            id.to_string(),
            self.session_paths.get(id).cloned(),
            "title".into(),
            serde_json::json!({ "title": title }),
        )
    }

    /// Upsert one durable user annotation (the AHP `Annotation` JSON rides the
    /// row verbatim). The same durable legs as a rename: the known-check, the
    /// journal row, and the prompt-notes cell so the next request's context
    /// carries it. Answers whether the row could land at all.
    pub fn set_annotation(&mut self, id: &str, annotation: &serde_json::Value) -> bool {
        self.annotation_row(
            id,
            "annotation_set",
            serde_json::json!({ "annotation": annotation }),
        )
    }

    /// Remove one durable user annotation by id (same legs as the upsert).
    pub fn remove_annotation(&mut self, id: &str, annotation_id: &str) -> bool {
        self.annotation_row(
            id,
            "annotation_removed",
            serde_json::json!({ "annotationId": annotation_id }),
        )
    }

    /// The shared durable legs of an annotation row: a session this store
    /// knows (by row or by path) with a materialized journal file takes the
    /// row through the store route, and the prompt-notes cell follows the
    /// durable state. Returns whether the row landed.
    fn annotation_row(&self, id: &str, kind: &str, payload: serde_json::Value) -> bool {
        let known =
            self.summaries.iter().any(|row| row.id == id) || self.session_paths.contains_key(id);
        if !known {
            return false;
        }
        let Some(path) = self.session_paths.get(id).filter(|p| p.exists()) else {
            return false;
        };
        crate::engine::dispatch_store_journal_row(
            id.to_string(),
            Some(path.clone()),
            kind.to_string(),
            payload,
        );
        crate::engine::refresh_notes_cell(id, Some(path));
        true
    }

    /// Persist the session's granted extra working directories (multi-
    /// root) so a cold restore re-widens the fence (multi-working-dirs).
    /// Drop every in-memory trace of one session (summary row, path map,
    /// live handle) — the true deletion for a session that never
    /// materialized. Durable artifacts are the caller's business: this
    /// store only ever held the row.
    pub fn drop_session_row(&mut self, id: &str) {
        self.summaries.retain(|row| row.id != id);
        self.session_paths.remove(id);
        self.live_threads.remove(id);
        self.pending_working_dirs.remove(id);
        // A parked view for a dropped row would resurrect the stamp (and
        // its sidecar write) against whatever reuses the id next.
        self.pending_viewed.remove(id);
        self.pending_unread.remove(id);
        self.pending_events.push(ThreadStoreEvent::SummariesUpdated);
    }

    /// The journal file path the store has for a session, when it has one.
    pub fn journal_path(&self, id: &str) -> Option<std::path::PathBuf> {
        self.session_paths.get(id).cloned()
    }

    /// The journal file for one session id as it exists ON DISK: the
    /// registered path first (a scan or a create-time note), then a bounded
    /// walk of the core root's buckets. The registered map is populated by
    /// scans and creates, so a session whose journal materialized since the
    /// last scan — a turn just run in a fresh session — would otherwise
    /// answer `None` to a caller that must append a durable row NOW.
    ///
    /// The walk admits only the minters' id charset (ASCII alphanumeric,
    /// `-`, `_`): the id is a session id, never a path, and a separator or
    /// dot must never reach the join.
    pub fn journal_path_on_disk(&self, id: &str) -> Option<std::path::PathBuf> {
        if let Some(path) = self.session_paths.get(id) {
            return Some(path.clone());
        }
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return None;
        }
        let buckets = std::fs::read_dir(&self.sessions_dir).ok()?;
        for bucket in buckets.flatten() {
            if !bucket.file_type().ok().is_some_and(|kind| kind.is_dir()) {
                continue;
            }
            let candidate = bucket
                .path()
                .join(id)
                .join(manox_harness::session::identity::JOURNAL_FILE);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    pub fn set_working_directories(&mut self, id: &str, dirs: Vec<String>) {
        // The grants wait for the journal: a session that never materializes
        // leaves no disk trace at all, and one that does takes the sidecar
        // the moment the file exists (refresh and load both re-check).
        self.pending_working_dirs.insert(id.to_string(), dirs);
        self.flush_working_dirs_if_materialized(id);
    }

    /// The session's create-time grants while its journal is still deferred.
    /// `None` once they flushed into the sidecar (or when the create never
    /// named any): the disk read is the durable authority by then, and this
    /// in-memory set is all a deferred session has.
    pub fn working_directories(&self, id: &str) -> Option<Vec<String>> {
        self.pending_working_dirs.get(id).cloned()
    }

    /// Flush the pending grants for one session when its journal exists.
    fn flush_working_dirs_if_materialized(&mut self, id: &str) {
        if self.session_paths.get(id).is_some_and(|path| path.exists())
            && let Some(dirs) = self.pending_working_dirs.remove(id)
        {
            self.write_meta(id, move |meta| meta.working_directories = dirs);
        }
    }

    /// Flush every pending grant whose journal has appeared since the last
    /// pass (the scan refresh and session loads call this).
    fn flush_materialized_working_dirs(&mut self) {
        let ready: Vec<String> = self
            .pending_working_dirs
            .keys()
            .filter(|id| self.session_paths.get(*id).is_some_and(|p| p.exists()))
            .cloned()
            .collect();
        for id in ready {
            self.flush_working_dirs_if_materialized(&id);
        }
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
        self.flush_working_dirs_if_materialized(id);
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
            access_mode: AccessMode::default().as_i64(),
            project: String::new(),
            depth: parent.is_some() as i32,
            parent_id: parent.map(str::to_string),
            subagent: false,
            archived: false,
            pinned: false,
            tag: None,
            last_settled_at: None,
            last_viewed_at: None,
            has_unread: false,
            errored: false,
            created_at: 0,
            interacted_at: 0,
            updated_at: 0,
            cumulative_total_tokens: 0,
        });
    }

    /// Seed a child-partition row without touching disk: the container-edge
    /// shape the catalogue annotates (a delegated subagent run whose parent
    /// names the owning thread row).
    #[cfg(any(test, feature = "test-support"))]
    pub fn insert_child_summary_for_test(&mut self, id: &str, parent: &str) {
        self.child_summaries.push(crate::db::ThreadSummary {
            subagent: true,
            parent_id: Some(parent.to_string()),
            ..crate::db::ThreadSummary::default()
        });
        let row = self.child_summaries.last_mut().expect("just inserted");
        row.id = id.to_string();
        row.summary = id.to_string();
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
        access_mode: i64,
    ) {
        self.insert_summary_for_test(id, parent);
        let summary = self.summaries.last_mut().expect("just inserted");
        summary.interacted_at = interacted_at;
        summary.updated_at = updated_at;
        summary.project = project.to_string();
        summary.tag = tag.map(str::to_string);
        summary.access_mode = access_mode;
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
    /// and archived partitions immediately (a delegated child flips its flag
    /// in the child partition — the walk filters the flag); the post-write
    /// refresh in `write_meta` re-syncs both partitions from disk. Archiving
    /// cascades to every descendant along `parent_id` (team members, fork
    /// children — one hierarchy rule; the child partition is not walked, a
    /// container teardown resolves those rows itself); unarchiving moves
    /// only the requested row. Re-asserting the current state is a no-op:
    /// no partition move, meta write, or lifecycle hook.
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
                } else if let Some(child) = self.child_summaries.iter_mut().find(|s| s.id == tid) {
                    // A delegated child stays in the child partition (there
                    // is no archived child partition — the catalogue walk
                    // filters the flag); the decision must still be visible
                    // in memory now, not only after the next refresh pass
                    // re-reads the sidecar.
                    child.archived = true;
                }
            } else if let Some(pos) = self.archived_summaries.iter().position(|s| s.id == tid) {
                let mut summary = self.archived_summaries.swap_remove(pos);
                summary.archived = false;
                self.summaries.push(summary);
            } else if let Some(child) = self.child_summaries.iter_mut().find(|s| s.id == tid) {
                child.archived = false;
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
        let _ = crate::engine::dispatch_store_journal_row(
            id.to_string(),
            self.session_paths.get(id).cloned(),
            "pinned_archived".into(),
            serde_json::json!({ "pinned": pinned, "archived": archived }),
        );
    }

    /// The bound project straight from the session sidecar (sync read).
    /// Unlike the summary mirror, the sidecar survives reconciles for
    /// sessions whose journal never materialized — the header-validation
    /// source the workspace domain relies on (review #805 follow-up).
    pub fn sidecar_project(&self, id: &str) -> Option<String> {
        // Resolve the journal first (the core layout nests it under the
        // session's own directory); the sidecar sits beside it. A
        // registered path wins — a reconcile pass cannot invalidate it —
        // and an unknown id falls back to the per-session directory guess.
        let journal = self.session_paths.get(id).cloned().unwrap_or_else(|| {
            self.sessions_dir
                .join(id)
                .join(manox_harness::session::identity::JOURNAL_FILE)
        });
        let meta_path = manox_harness::session_meta::meta_path(&self.sessions_dir, &journal);
        let raw = std::fs::read_to_string(meta_path).ok()?;
        let meta: manox_harness::session_meta::SessionMeta = serde_json::from_str(&raw).ok()?;
        meta.project
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

    /// Append a `model_change` event. The transcript records model changes
    /// itself; nothing to do here.
    pub fn record_model_change(&self, _thread_id: &str, _from: Option<&str>, _to: &str) {}

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
    index: &mut SessionIndex,
) -> (Vec<SessionRow>, Vec<SessionRow>) {
    // The cold-cache layer retired with the db: the in-memory index still
    // accelerates a live process's repeat scans, but every restart starts
    // from a bounded header scan — the list's content never depended on the
    // cache anyway.
    index.seeded = true;
    let stats = stat_session_files(dir).await;
    let live: std::collections::HashSet<PathBuf> = stats.iter().map(|s| s.path.clone()).collect();
    let mut rows = Vec::with_capacity(stats.len());
    let mut child_rows = Vec::new();
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
        // The sidebar renders only the current host's sessions. A session
        // stamped `metadata.subagent` is a delegated child (run id ==
        // session id): it persists for usage accounting and the container
        // tree, but never ranks as a sidebar row — it lands in the child
        // partition the catalogue annotates instead.
        let is_child = entry
            .metadata
            .as_ref()
            .is_some_and(|m| m.get("subagent").is_some());
        if !crate::host::belongs_to_current_host(entry.metadata.as_ref()) {
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
        let row = SessionRow {
            summary: summary_from_cached(entry, modified_at, meta),
            path: stat.path.clone(),
            thread_key,
        };
        if is_child {
            child_rows.push(row);
        } else {
            rows.push(row);
        }
        if torn_tail {
            index.rows.remove(&stat.path);
            index.dirty = true;
        }
    }
    // Vanished files leave the cache; the persist below drops their rows
    // from the cold layer with them.
    index.rows.retain(|path, _| live.contains(path));
    index.dirty = false;
    (rows, child_rows)
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
    // The container edges. A child row hangs off `metadata.subagent.parent`
    // (the spawning session — the reverse edge the child chat's ChatOrigin
    // carries); a captain row keeps the team affiliation over the fork
    // lineage (`team_parent_id`'s precedence). Two different vocabularies:
    // a subagent row never carries the team or fork edge.
    let subagent = entry
        .metadata
        .as_ref()
        .and_then(|m| m.get("subagent"))
        .and_then(|s| s.as_object());
    let subagent_parent = subagent
        .and_then(|s| s.get("parent"))
        .and_then(|p| p.as_str())
        .filter(|p| !p.is_empty())
        .map(str::to_string);
    let is_subagent = subagent.is_some();
    // The dispatch label is the child's display title (the `run/started`
    // row carries the same string); an LLM title must not outrank it.
    let title = if is_subagent {
        subagent
            .and_then(|s| s.get("label"))
            .and_then(|l| l.as_str())
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .or_else(|| meta.title.clone())
    } else {
        meta.title.clone()
    };
    let parent_id = if is_subagent {
        subagent_parent
    } else {
        entry
            .metadata
            .as_ref()
            .and_then(|m| m.get("team"))
            .and_then(|t| t.get("parent"))
            .and_then(|p| p.as_str())
            .map(str::to_string)
            .or_else(|| entry.parent_session_path.clone())
    };
    ThreadSummary {
        id: entry.id.clone(),
        summary,
        title,
        title_override: None,
        model_id: String::new(),
        provider_id: None,
        access_mode: AccessMode::default().as_i64(),
        // The bound project (sidecar) wins over the header cwd.
        project: meta
            .project
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| entry.cwd.clone()),
        depth: 0,
        parent_id,
        subagent: is_subagent,
        archived: meta.archived,
        pinned: meta.pinned,
        tag: meta.tag.clone(),
        // The two monotone read inputs; the bit derives from them (§ the one
        // read rule in `crate::db`).
        last_settled_at: meta.last_settled_at,
        last_viewed_at: meta.last_viewed_at,
        has_unread: crate::db::derive_unread(meta.last_settled_at, meta.last_viewed_at),
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
        // The core root is two-deep: project buckets over per-session
        // directories, the journal (`log.v1.jsonl`) inside each. A flat
        // `*.jsonl` beside the buckets (the pre-core layout a test store
        // still uses) stays visible — both shapes scan.
        fn walk(dir: &std::path::Path, depth: u8, out: &mut Vec<Stat>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(file_meta) = std::fs::metadata(&path) else {
                    continue;
                };
                if file_meta.is_dir() {
                    if depth > 0 {
                        walk(&path, depth - 1, out);
                    }
                    continue;
                }
                if path.extension().is_none_or(|e| e != "jsonl") {
                    continue;
                }
                let sidecar =
                    std::fs::metadata(manox_harness::session_meta::meta_path(dir, &path)).ok();
                out.push(Stat {
                    path,
                    size: file_meta.len(),
                    mtime_ns: metadata_mtime_ns(&file_meta),
                    sidecar_size: sidecar.as_ref().map_or(0, |m| m.len()),
                    sidecar_mtime_ns: sidecar.as_ref().map_or(0, metadata_mtime_ns),
                });
            }
        }
        let mut out = Vec::new();
        walk(&dir, 2, &mut out);
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
/// their own id. Child rows (delegated subagent sessions) never group —
/// their parent edge resolves against the same session→thread map so the
/// catalogue annotation names the row the client actually sees. Returns
/// the id→active-session-path map (every surfaced row stays addressable
/// for `load_thread` and sidecar flag writes), the active rows, the
/// archived rows, and the child rows.
fn group_by_thread(
    rows: Vec<SessionRow>,
    child_rows: Vec<SessionRow>,
    registry: &HashMap<String, crate::thread_registry::ThreadRegistryEntry>,
) -> (
    HashMap<String, PathBuf>,
    Vec<ThreadSummary>,
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
    // Child rows stay singleton (they carry no thread stamp); their parent
    // session remaps to the parent's THREAD row id when that parent is in
    // this scan — the catalogue's join key — keeping the raw session id
    // when it does not resolve (the team-edge remap's rule).
    let mut children = Vec::with_capacity(child_rows.len());
    for row in child_rows {
        session_paths.insert(row.summary.id.clone(), row.path);
        let mut sum = row.summary;
        sum.parent_id = sum
            .parent_id
            .take()
            .map(|parent| session_to_thread.get(&parent).cloned().unwrap_or(parent));
        children.push(sum);
    }
    (session_paths, active, archived, children)
}

/// Set equality on row id: same rows, any order. The scan rebuilds rows
/// from a HashMap walk and the store holds them reranked; comparing the
/// slices directly would flag every quiet pass as changed. Rows are unique
/// by id (the store's key), so an id-keyed map is exact and linear — the
/// previous `fresh.contains` scan was quadratic in the row count and
/// compared every field of every candidate pair.
fn rows_match_ignoring_order(
    held: &[crate::db::ThreadSummary],
    fresh: &[crate::db::ThreadSummary],
) -> bool {
    if held.len() != fresh.len() {
        return false;
    }
    let fresh_by_id: HashMap<&str, &crate::db::ThreadSummary> =
        fresh.iter().map(|row| (row.id.as_str(), row)).collect();
    held.iter().all(|row| {
        fresh_by_id
            .get(row.id.as_str())
            .is_some_and(|fresh_row| *fresh_row == row)
    })
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
pub fn init_for_test() {
    let handle = standalone_for_test();
    *TEST_OVERRIDE.lock().unwrap() = Some(handle);
}

/// A standalone store handle over `db` (test-support): NO process global
/// involved. Tests that inject a store directly (the goal bridge's journal
/// routing) use this to stay immune to cross-test TEST_OVERRIDE churn —
/// the agent suite runs its store tests in parallel.
#[cfg(any(test, feature = "test-support"))]
pub fn standalone_for_test() -> StoreHandle {
    let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    standalone_for_test_at(&dir)
}

/// [`Self::standalone_for_test`] over an EXPLICIT root — the cross-process
/// shape: two stores sharing one sessions dir are two peers on one disk.
#[cfg(any(test, feature = "test-support"))]
pub fn standalone_for_test_at(dir: &std::path::Path) -> StoreHandle {
    let projects_path = dir.join("projects.json");
    StoreHandle::new(ThreadStore {
        project_aliases: HashMap::new(),
        summaries: Vec::new(),
        archived_summaries: Vec::new(),
        child_summaries: Vec::new(),
        session_paths: HashMap::new(),
        known_projects: Vec::new(),
        order: crate::sidebar_order::SidebarOrder::default(),
        order_dirty: false,
        running: HashSet::new(),
        pending_auth: HashSet::new(),
        live_threads: HashMap::new(),
        sessions_dir: dir.to_path_buf(),
        projects_path,
        decision_overlay: HashMap::new(),
        pending_events: Vec::new(),
        pending_meta_writes: Vec::new(),
        pending_working_dirs: HashMap::new(),
        pending_viewed: HashMap::new(),
        pending_unread: HashMap::new(),
        scanned_once: false,
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
    // The frozen per-handle dir: no state lock (see `StoreCore::dir`).
    global().0.dir.clone()
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

    /// The on-disk journal lookup falls back to a bounded walk of the core
    /// root, and refuses anything but the minters' id charset.
    #[test]
    fn journal_path_on_disk_walks_the_core_root_buckets() {
        let store = standalone_for_test();
        let root = store.0.dir.clone();
        let bucket = root.join("users-x-work-manox");
        let journal = bucket
            .join("s-walked")
            .join(manox_harness::session::identity::JOURNAL_FILE);
        std::fs::create_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::write(&journal, b"").unwrap();
        assert_eq!(
            store
                .read(|s| s.journal_path_on_disk("s-walked"))
                .as_deref(),
            Some(journal.as_path()),
            "a journal the store has not scanned still resolves"
        );

        let registered = root.join("registered.jsonl");
        store.with_mut(|s| s.note_session_path("s-walked", &registered));
        assert_eq!(
            store
                .read(|s| s.journal_path_on_disk("s-walked"))
                .as_deref(),
            Some(registered.as_path()),
            "the registered path wins over the walk"
        );

        assert!(
            store
                .read(|s| s.journal_path_on_disk("../../etc/passwd"))
                .is_none(),
            "an id is not a path"
        );
        assert!(store.read(|s| s.journal_path_on_disk("")).is_none());
        assert!(
            store.read(|s| s.journal_path_on_disk("s-absent")).is_none(),
            "no journal anywhere answers None"
        );
    }

    use super::*;

    fn temp_db() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("store-test-{}", uuid::Uuid::new_v4()))
    }

    /// A rename must land a durable `title` **journal** row, not only the
    /// sidecar.
    ///
    /// The row is the only route the two protocol faces take — the v2
    /// projection folds `SessionTreeEntry::Title` and the AHP translator reads
    /// the journal, while the live `TitleChanged` event is on `translate.rs`'s
    /// `Skip` list. A store-side rename that skipped this would be invisible to
    /// every client following the session, which is exactly the cross-protocol
    /// divergence the store-level journal append exists to prevent.
    #[test]
    fn rename_thread_appends_the_durable_title_row() {
        let db_path = temp_db();
        crate::runtime::init_hermetic_for_test();
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("t-title.jsonl");
        // An existing session file is what the cold-append fallback (no live
        // engine in this test) appends to; a session with no file at all has
        // nowhere to put the row, and the sidecar carries the flag instead.
        std::fs::write(
            &session,
            "{\"type\":\"session\",\"version\":3,\"id\":\"t-title\",\
             \"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/p\",\
             \"metadata\":{\"host\":\"manox\"}}\n",
        )
        .unwrap();
        let store = store_handle();
        store.with_mut(|s| {
            s.insert_summary_for_test("t-title", None);
            s.session_paths
                .insert("t-title".to_string(), session.clone());
            s.sessions_dir = dir.path().to_path_buf();
        });

        store.with_mut(|s| {
            assert_eq!(
                s.rename_thread("t-title", "renamed by the user"),
                RenameOutcome::Renamed
            )
        });

        // Read the row back through the journal walk (not a byte match), so the
        // assertion is on the durable vocabulary a reader actually folds.
        let titles = |path: &std::path::Path| -> Vec<String> {
            crate::runtime::handle()
                .block_on(async {
                    let storage = manox_harness::session::jsonl::JsonlSessionStorage::open(path)
                        .await
                        .unwrap();
                    storage.journal_range(0, u64::MAX).await.unwrap()
                })
                .into_iter()
                .filter_map(|record| match record.entry {
                    manox_harness::session::SessionTreeEntry::Title { title, .. } => Some(title),
                    _ => None,
                })
                .collect()
        };
        let mut landed = Vec::new();
        for _ in 0..500 {
            landed = titles(&session);
            if !landed.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(
            landed,
            vec!["renamed by the user".to_string()],
            "a rename must append a durable title row"
        );
        std::fs::remove_file(db_path).ok();
    }

    /// A rename takes display precedence over a model-generated title (the
    /// `title_override` slot `display_title` reads first), and a blank title is
    /// refused rather than stored — a blank would read as "renamed to nothing"
    /// while the session kept its old name.
    ///
    /// This session has **no journal file**, so the durable leg has nowhere to
    /// land and the outcome says so (`NotPersisted`). The in-memory row still
    /// flips, which is exactly why the outcome — not the row — is what a caller
    /// must report on: the title is visible here and unreadable to every other
    /// process.
    #[test]
    fn rename_thread_reports_whether_it_could_persist() {
        let _path = temp_db();
        // A rename journals as well as writing the sidecar, so the store needs
        // the process runtime the journal append dispatches onto.
        crate::runtime::init_hermetic_for_test();
        let store = store_handle();
        store.with_mut(|s| s.insert_summary_for_test("t-rename", None));
        store.with_mut(|s| {
            let row = s.summary_mut("t-rename").expect("seeded");
            row.title = Some("model said this".to_string());
        });

        store.with_mut(|s| {
            assert_eq!(
                s.rename_thread("t-rename", "user said this"),
                RenameOutcome::NotPersisted,
                "no journal file means the rename cannot be durable"
            )
        });
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
        store.with_mut(|s| assert_eq!(s.rename_thread("t-rename", "   "), RenameOutcome::Blank));
        store.with_mut(|s| assert_eq!(s.rename_thread("t-rename", ""), RenameOutcome::Blank));
        let row = store
            .read(|s| s.summary_by_id("t-rename").cloned())
            .unwrap();
        assert_eq!(
            row.title_override.as_deref(),
            Some("user said this"),
            "a refused rename leaves the previous title intact"
        );

        // An unknown session is refused, not silently dropped.
        store.with_mut(|s| {
            assert_eq!(
                s.rename_thread("no-such-session", "x"),
                RenameOutcome::UnknownSession
            )
        });
    }

    fn store_handle() -> StoreHandle {
        let dir = std::env::temp_dir().join(format!("store-dir-{}", uuid::Uuid::new_v4()));
        let projects_path = dir.join("projects.json");
        StoreHandle::new(ThreadStore {
            project_aliases: HashMap::new(),
            summaries: Vec::new(),
            archived_summaries: Vec::new(),
            child_summaries: Vec::new(),
            session_paths: HashMap::new(),
            known_projects: Vec::new(),
            order: crate::sidebar_order::SidebarOrder::default(),
            order_dirty: false,
            running: HashSet::new(),
            pending_auth: HashSet::new(),
            live_threads: HashMap::new(),
            sessions_dir: dir,
            projects_path,
            decision_overlay: HashMap::new(),
            pending_events: Vec::new(),
            pending_meta_writes: Vec::new(),
            pending_working_dirs: HashMap::new(),
            pending_viewed: HashMap::new(),
            pending_unread: HashMap::new(),
            scanned_once: false,
        })
    }

    #[test]
    fn working_directories_wait_for_the_journal_to_materialize() {
        crate::runtime::init();
        let _db_path = temp_db();
        let dir = std::env::temp_dir().join(format!("store-wd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let handle = store_handle();
        handle.with_mut(|s| s.sessions_dir = dir.clone());

        // A create on a not-yet-materialized session leaves no sidecar: a
        // session that never materializes leaves no disk trace at all.
        handle.with_mut(|s| {
            s.note_session_path("s-blank", &dir.join("s-blank.jsonl"));
            s.set_working_directories("s-blank", vec!["/w/extra".into()]);
        });
        assert!(!dir.join("s-blank.meta.json").exists());

        // The journal materializes; the next store pass flushes the grants.
        // The sidecar write lands on the store's async refresh pump, so the
        // test waits for it rather than racing the spawn.
        write_session_fixture(&dir, "s-blank", "first");
        handle.with_mut(|s| s.flush_materialized_working_dirs());
        let meta_path = dir.join("s-blank.meta.json");
        let mut landed = false;
        for _ in 0..500 {
            if meta_path.exists() {
                landed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(landed, "the flushed grants never reached the sidecar");
        let meta = std::fs::read_to_string(&meta_path).unwrap();
        assert!(meta.contains("/w/extra"), "{meta}");
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

    fn first_summary_of(store: &StoreHandle, id: &str) -> String {
        store.read(|s| {
            s.summaries()
                .iter()
                .find(|row| row.id == id)
                .map(|row| row.summary.clone())
                .expect("the row is listed")
        })
    }

    /// A delegated child run fixture: a header stamped `metadata.subagent`
    /// (the durable lineage the spawn writes) plus one user message, so the
    /// bounded scan sees a live transcript. `parent` is the spawning
    /// session's id; `label` is the dispatch label the `run/started` row
    /// carries.
    fn write_child_session_fixture(
        dir: &std::path::Path,
        id: &str,
        parent: &str,
        label: &str,
    ) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        let content = format!(
            "{{\"type\":\"session\",\"version\":4,\"id\":\"{id}\",\"timestamp\":\"2026-09-13T00:00:00Z\",\"cwd\":\"/proj\",\"metadata\":{{\"host\":\"manox\",\"subagent\":{{\"type\":\"Explore\",\"label\":\"{label}\",\"parent\":\"{parent}\",\"depth\":1}}}}}}\n{{\"type\":\"message\",\"id\":\"m1\",\"parentId\":null,\"timestamp\":\"2026-09-13T00:00:01Z\",\"seq\":0,\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"survey the auth module\"}}],\"timestamp\":1770000000000}}}}\n",
        );
        std::fs::write(&path, content).unwrap();
        path
    }

    /// The fact-layer rule: append-only GROWTH never rescans a session's
    /// bounded facts (the row keeps the cached first message even when the
    /// prefix on disk moved underneath — an impossibility under real
    /// appends, pinned here precisely because the cache must not care);
    /// a SHRINK always rescans.
    #[tokio::test]
    async fn reconcile_reuses_facts_on_growth_and_rescans_on_shrink() {
        let _db_path = temp_db();
        let store = standalone_for_test();
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
        let _db_path = temp_db();
        let store = standalone_for_test();
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
        let _db_path = temp_db();
        let store = standalone_for_test();
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
        let _db_path = temp_db();
        let store = standalone_for_test();
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

    /// A restart rebuilds identical rows from the bounded scan alone — the
    /// cold-cache layer retired with the db, and the list's content never
    /// depended on it.
    #[tokio::test]
    async fn reconcile_rebuilds_identical_rows_after_a_restart() {
        let store = standalone_for_test();
        let dir = store.read(|s| s.sessions_dir.clone());
        write_session_fixture(&dir, "persist", "cold start prompt");

        store.refresh_now().await;
        assert_eq!(first_summary_of(&store, "persist"), "cold start prompt");

        let sessions_dir = dir.clone();
        let projects_path = sessions_dir.join("projects.json");
        let restarted = StoreHandle::new(ThreadStore {
            project_aliases: HashMap::new(),
            summaries: Vec::new(),
            archived_summaries: Vec::new(),
            child_summaries: Vec::new(),
            session_paths: HashMap::new(),
            known_projects: Vec::new(),
            order: crate::sidebar_order::SidebarOrder::default(),
            order_dirty: false,
            running: HashSet::new(),
            pending_auth: HashSet::new(),
            live_threads: HashMap::new(),
            sessions_dir,
            projects_path,
            decision_overlay: HashMap::new(),
            pending_events: Vec::new(),
            pending_meta_writes: Vec::new(),
            pending_working_dirs: HashMap::new(),
            pending_viewed: HashMap::new(),
            pending_unread: HashMap::new(),
            scanned_once: false,
        });
        restarted.refresh_now().await;
        assert_eq!(
            first_summary_of(&restarted, "persist"),
            "cold start prompt",
            "a restarted store rebuilds identical rows from the scan"
        );
    }

    #[test]
    fn register_project_persists_and_survives_reopen() {
        let store = store_handle();
        let path = store.read(|s| s.projects_path.clone());
        store.with_mut(|s| s.register_project("/p/a".into()));
        // Persisted to the registry file...
        assert!(std::fs::read_to_string(&path).unwrap().contains("/p/a"));
        // ...and a fresh store over the same root seeds from it.
        let reopened = StoreHandle::new(ThreadStore {
            project_aliases: HashMap::new(),
            summaries: Vec::new(),
            archived_summaries: Vec::new(),
            child_summaries: Vec::new(),
            session_paths: HashMap::new(),
            known_projects: read_projects_file(&path)
                .iter()
                .map(|p| p.path.clone())
                .collect(),
            order: crate::sidebar_order::SidebarOrder::default(),
            order_dirty: false,
            running: HashSet::new(),
            pending_auth: HashSet::new(),
            live_threads: HashMap::new(),
            sessions_dir: store.read(|s| s.sessions_dir.clone()),
            projects_path: path,
            decision_overlay: HashMap::new(),
            pending_events: Vec::new(),
            pending_meta_writes: Vec::new(),
            pending_working_dirs: HashMap::new(),
            pending_viewed: HashMap::new(),
            pending_unread: HashMap::new(),
            scanned_once: false,
        });
        assert!(reopened.read(|s| s.known_projects.contains(&"/p/a".to_string())));
    }

    #[test]
    fn register_project_dedupes() {
        let _path = temp_db();
        let store = store_handle();
        store.with_mut(|s| {
            s.register_project("/p/a".into());
            s.register_project("/p/a".into());
            s.register_project(String::new());
        });
        let known = store.read(|s| s.known_projects().to_vec());
        assert_eq!(known, vec!["/p/a".to_string()]);
        let registry = store.read(|s| s.projects_path.clone());
        let persisted: Vec<ProjectEntry> =
            serde_json::from_str(&std::fs::read_to_string(&registry).unwrap()).unwrap();
        assert_eq!(
            persisted,
            vec![ProjectEntry {
                path: "/p/a".into(),
                alias: None
            }]
        );
    }

    #[test]
    fn remove_project_persists_and_survives_reopen() {
        let _path = temp_db();
        let store = store_handle();
        store.with_mut(|s| {
            s.register_project("/p/a".into());
            s.register_project("/p/b".into());
            s.remove_project("/p/a");
            // Removing an unknown path is a no-op.
            s.remove_project("/p/missing");
        });
        let registry = store.read(|s| s.projects_path.clone());
        let persisted: Vec<ProjectEntry> =
            serde_json::from_str(&std::fs::read_to_string(&registry).unwrap()).unwrap();
        assert_eq!(
            persisted,
            vec![ProjectEntry {
                path: "/p/b".into(),
                alias: None
            }]
        );
        let known = store.read(|s| s.known_projects().to_vec());
        assert_eq!(known, vec!["/p/b".to_string()]);
    }

    /// The pending-auth badge marker toggles per thread id and only emits
    /// `SummariesUpdated` on an actual state change.
    #[test]
    fn mark_pending_auth_toggles_marker() {
        let _path = temp_db();
        let store = store_handle();
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
    }

    /// The running-set marker (the sidebar spinner source) toggles per thread
    /// id, fires `RunningChanged` only on an actual state change, and is
    /// idempotent under repeated marks — the store contract every host
    /// subscription (foreground, parked, actor) relies on.
    #[test]
    fn mark_running_toggles_marker() {
        let _path = temp_db();
        let store = store_handle();
        let events = store.subscribe();
        // One transition announces both the marker and the catalogue: the
        // running set feeds the summary's `InProgress` bit, so a
        // `RunningChanged` alone would leave the list stale.
        let drain = || {
            let mut running = 0;
            let mut summaries = 0;
            while let Ok(event) = events.try_recv() {
                match *event {
                    ThreadStoreEvent::RunningChanged => running += 1,
                    ThreadStoreEvent::SummariesUpdated => summaries += 1,
                }
            }
            (running, summaries)
        };
        store.with_mut(|s| s.mark_running("t1"));
        assert!(store.read(|s| s.is_running("t1")));
        assert_eq!(drain(), (1, 1));
        // Idempotent mark: no event, no duplicate work.
        store.with_mut(|s| s.mark_running("t1"));
        assert_eq!(drain(), (0, 0));
        // A second thread marks independently.
        store.with_mut(|s| s.mark_running("t2"));
        assert!(store.read(|s| s.is_running("t2")));
        assert_eq!(drain(), (1, 1));
        store.with_mut(|s| s.mark_idle("t1"));
        assert!(!store.read(|s| s.is_running("t1")));
        assert!(store.read(|s| s.is_running("t2")));
        assert_eq!(drain(), (1, 1));
    }

    fn sample_summary(id: &str, archived: bool) -> ThreadSummary {
        ThreadSummary {
            id: id.to_string(),
            summary: String::new(),
            title: None,
            title_override: None,
            model_id: String::new(),
            provider_id: None,
            access_mode: AccessMode::default().as_i64(),
            project: String::new(),
            depth: 0,
            parent_id: None,
            subagent: false,
            archived,
            pinned: false,
            tag: None,
            last_settled_at: None,
            last_viewed_at: None,
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
        let (paths, active, archived, _) = group_by_thread(rows, Vec::new(), &registry);
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
        let (paths, active, _, _) = group_by_thread(rows.clone(), Vec::new(), &HashMap::new());
        assert_eq!(active.len(), 1);
        assert_eq!(paths.get("t").cloned(), Some(PathBuf::from("fork.jsonl")));
        // A stale pointer to a foreign session id degrades the same way.
        let stale = HashMap::from([("t".to_string(), pointer("gone"))]);
        let (_, active, _, _) = group_by_thread(rows, Vec::new(), &stale);
        assert_eq!(active.len(), 1);
    }

    #[test]
    fn group_remaps_team_edge_to_thread_id() {
        let leader = sample_row("leader-sess", Some("TL"), 10, false);
        let mut member = sample_row("member-sess", Some("TM"), 10, false);
        member.summary.parent_id = Some("leader-sess".to_string());
        let (paths, active, _, _) =
            group_by_thread(vec![leader, member], Vec::new(), &HashMap::new());
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
        let (paths, active, _, _) = group_by_thread(rows, Vec::new(), &HashMap::new());
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
        let (_, active, archived, _) = group_by_thread(rows, Vec::new(), &registry);
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
        let (rows, children) = reconcile_summaries(sessions, &mut index).await;
        assert_eq!(rows.len(), 2);
        assert_eq!(children.len(), 0, "no child stamps in this fixture");

        // Pointer on the fork (inside the worktree): one row, thread-keyed,
        // project stays the source's.
        let registry = HashMap::from([(thread_key.to_string(), pointer(fork_id))]);
        let (paths, active, archived, _) = group_by_thread(rows.clone(), Vec::new(), &registry);
        assert!(archived.is_empty());
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, thread_key);
        assert_eq!(active[0].title.as_deref(), Some("the title"));
        assert_eq!(active[0].project, "/proj/a");
        assert_eq!(paths.get(thread_key).cloned(), Some(fork_path.clone()));

        // Pointer back on the base: same single row,
        // now addressable at the base session.
        let registry = HashMap::from([(thread_key.to_string(), pointer(base_id))]);
        let (paths, active, _, _) = group_by_thread(rows, Vec::new(), &registry);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].id, thread_key);
        assert_eq!(paths.get(thread_key).cloned(), Some(base_path));
    }

    /// One `listSessions` walk must be enough to organize sessions into
    /// container trees: a dispatched run scans into the child partition —
    /// parent remapped to the thread row the catalogue serves, dispatch
    /// label as the title — while the sidebar partitions never see it. The
    /// catalogue walk appends the partition (the AHP backend's job); this
    /// pins the store half of that contract.
    #[tokio::test]
    async fn child_runs_scan_into_the_child_partition_with_container_edges() {
        let store = standalone_for_test();
        let dir = store.read(|s| s.sessions_dir.clone());
        write_session_fixture(&dir, "captain-1", "captain prompt");
        write_child_session_fixture(&dir, "run-1", "captain-1", "survey auth");

        store.refresh_now().await;

        let captain_ids: Vec<String> =
            store.read(|s| s.summaries().iter().map(|r| r.id.clone()).collect());
        assert_eq!(
            captain_ids,
            vec!["captain-1".to_string()],
            "the child never ranks as a sidebar row"
        );
        assert!(
            !store.read(|s| s.archived_summaries().iter().any(|r| r.id == "run-1")),
            "the child is not an archived row either"
        );
        let rows = store.read(|s| s.child_summaries().to_vec());
        assert_eq!(rows.len(), 1, "the run scans exactly one child row");
        let child = &rows[0];
        assert_eq!(child.id, "run-1", "the child row id is the run id");
        assert!(child.subagent, "the row carries the member kind");
        assert_eq!(
            child.parent_id.as_deref(),
            Some("captain-1"),
            "the parent resolves to the thread row the catalogue serves (v1.0.0: thread id == session id)"
        );
        assert_eq!(child.display_title(), "survey auth");
        // Addressable for reopen and flag writes; lookup reaches the
        // partition.
        assert!(store.read(|s| s.session_paths.contains_key("run-1")));
        assert_eq!(
            store.read(|s| s.summary_by_id("run-1").map(|r| r.id.clone())),
            Some("run-1".to_string())
        );
    }

    /// The parent edge names the catalogue's join key: a spawning session
    /// grouped under a legacy thread key remaps the child's parent to that
    /// row id (the team-edge remap's rule), so the client's fold finds the
    /// parent row the list actually carries.
    #[tokio::test]
    async fn a_child_parent_remaps_to_the_parent_thread_row() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path();
        // The captain's session id differs from its thread key (a
        // worktree-era multi-session thread).
        let header = r#"{"type":"session","version":4,"id":"captain-old","timestamp":"2026-09-13T00:00:00Z","cwd":"/proj","metadata":{"host":"manox","thread":"thread-old"}}"#;
        std::fs::write(sessions.join("captain-old.jsonl"), header).unwrap();
        write_child_session_fixture(sessions, "run-1", "captain-old", "survey auth");

        let store = standalone_for_test();
        store.with_mut(|s| s.sessions_dir = sessions.to_path_buf());
        store.refresh_now().await;

        let child = store
            .read(|s| s.child_summaries().first().cloned())
            .expect("the child row is listed");
        assert_eq!(
            child.parent_id.as_deref(),
            Some("thread-old"),
            "the parent names the thread row, not the spawning session"
        );
    }

    /// A parent the scan cannot resolve (a foreign host, a vanished
    /// journal) keeps its raw session id — the client degrades the row to
    /// top-level, the same rule an unresolvable team edge follows.
    #[tokio::test]
    async fn a_child_parent_without_a_scan_row_keeps_the_raw_session_id() {
        let store = standalone_for_test();
        let dir = store.read(|s| s.sessions_dir.clone());
        write_child_session_fixture(&dir, "run-orphan", "vanished-parent", "survey auth");

        store.refresh_now().await;

        let child = store
            .read(|s| s.child_summaries().first().cloned())
            .expect("the child row is listed");
        assert_eq!(child.parent_id.as_deref(), Some("vanished-parent"));
    }

    /// A restart rebuilds the container edges from the headers alone: the
    /// annotation a live dispatch produced and the annotation a cold rescan
    /// produces are the same rows, byte for byte.
    #[tokio::test]
    async fn a_restart_rebuilds_the_child_rows_identically() {
        let store = standalone_for_test();
        let dir = store.read(|s| s.sessions_dir.clone());
        write_session_fixture(&dir, "captain-1", "captain prompt");
        write_child_session_fixture(&dir, "run-1", "captain-1", "survey auth");
        store.refresh_now().await;
        let live_rows = store.read(|s| s.child_summaries().to_vec());

        // A fresh store over the same root — the restore-after-restart
        // shape: cold cache, headers only.
        let sessions_dir = dir.clone();
        let projects_path = sessions_dir.join("projects.json");
        let restarted = StoreHandle::new(ThreadStore {
            project_aliases: HashMap::new(),
            summaries: Vec::new(),
            archived_summaries: Vec::new(),
            child_summaries: Vec::new(),
            session_paths: HashMap::new(),
            known_projects: Vec::new(),
            order: crate::sidebar_order::SidebarOrder::default(),
            order_dirty: false,
            running: HashSet::new(),
            pending_auth: HashSet::new(),
            live_threads: HashMap::new(),
            sessions_dir,
            projects_path,
            decision_overlay: HashMap::new(),
            pending_events: Vec::new(),
            pending_meta_writes: Vec::new(),
            pending_working_dirs: HashMap::new(),
            pending_viewed: HashMap::new(),
            pending_unread: HashMap::new(),
            scanned_once: false,
        });
        restarted.refresh_now().await;
        assert_eq!(
            restarted.read(|s| s.child_summaries().to_vec()),
            live_rows,
            "replay resolves the same parent/kind annotation"
        );
        assert_eq!(
            restarted.read(|s| s
                .summaries()
                .iter()
                .map(|r| r.id.clone())
                .collect::<Vec<_>>()),
            vec!["captain-1".to_string()],
            "the restarted walk keeps the child out of the sidebar partitions"
        );
    }

    /// A run dispatched mid-session lands as a catalogue row on attached
    /// clients: the child partition joins the broadcast-gate diff, so a
    /// quiet sweep never re-announces but a new child row does.
    #[tokio::test]
    async fn a_new_child_run_announces_a_store_change() {
        let store = standalone_for_test();
        let dir = store.read(|s| s.sessions_dir.clone());
        write_session_fixture(&dir, "captain-1", "captain prompt");
        store.refresh_now().await;
        let events = store.subscribe();
        while events.try_recv().is_ok() {}

        // A quiet pass announces nothing.
        store.refresh_now().await;
        assert_eq!(
            events.len(),
            0,
            "a child-less rescan does not re-announce the catalogue"
        );

        write_child_session_fixture(&dir, "run-1", "captain-1", "survey auth");
        store.refresh_now().await;
        assert_eq!(
            events.len(),
            1,
            "the new child row is a catalogue change the clients must see"
        );
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

    /// The container edges live on the row: a `metadata.subagent` stamp
    /// makes the row a child (`subagent`), `subagent.parent` names the
    /// spawning session (the reverse edge the child chat's ChatOrigin
    /// carries), and the dispatch label — the string the `run/started` row
    /// carries — is the display title, outranking an LLM sidecar title.
    #[test]
    fn summary_carries_the_subagent_container_edges() {
        let entry = sample_entry(
            "run-1",
            Some(serde_json::json!({
                "subagent": {
                    "type": "Explore",
                    "label": "survey auth",
                    "parent": "captain-1",
                    "depth": 1,
                }
            })),
        );
        let summary = cached_summary(&entry, &manox_harness::session_meta::SessionMeta::default());
        assert!(summary.subagent, "the subagent stamp is the member kind");
        assert_eq!(summary.parent_id.as_deref(), Some("captain-1"));
        assert_eq!(
            summary.title.as_deref(),
            Some("survey auth"),
            "the dispatch label is the child's title"
        );
        assert_eq!(summary.display_title(), "survey auth");
        let titled = manox_harness::session_meta::SessionMeta {
            title: Some("llm said".into()),
            ..Default::default()
        };
        let summary = cached_summary(&entry, &titled);
        assert_eq!(
            summary.display_title(),
            "survey auth",
            "an LLM title never outranks the dispatch label on a child row"
        );

        // A captain row carries no kind bit, and keeps the team-over-fork
        // precedence — the subagent edge is a different vocabulary.
        let captain = sample_entry(
            "captain-1",
            Some(serde_json::json!({ "team": { "parent": "lead" } })),
        );
        let row = cached_summary(
            &captain,
            &manox_harness::session_meta::SessionMeta::default(),
        );
        assert!(!row.subagent);
        assert_eq!(row.parent_id.as_deref(), Some("lead"));

        // A subagent stamp without a parent or label degrades per-field,
        // never into a captain row.
        let orphan = sample_entry(
            "run-2",
            Some(serde_json::json!({ "subagent": { "type": "Explore" } })),
        );
        let row = cached_summary(
            &orphan,
            &manox_harness::session_meta::SessionMeta::default(),
        );
        assert!(row.subagent);
        assert_eq!(row.parent_id, None);
        assert_eq!(
            row.display_title(),
            "hi",
            "no label falls back to the transcript"
        );
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
        let db_path = temp_db();
        crate::runtime::init_hermetic_for_test();
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("t1.jsonl");
        let store = store_handle();
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
        let db_path = temp_db();
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
        let store = store_handle();
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
            access_mode: AccessMode::default().as_i64(),
            project: String::new(),
            depth: parent.is_some() as i32,
            parent_id: parent.map(str::to_string),
            subagent: false,
            archived: false,
            pinned: false,
            tag: None,
            last_settled_at: None,
            last_viewed_at: None,
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
        let db_path = temp_db();
        let store = store_handle();
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
        let db_path = temp_db();
        let store = store_handle();
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
        let db_path = temp_db();
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
        let store = store_handle();
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
        let db_path = temp_db();
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
        let store = store_handle();
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
        let db_path = temp_db();
        let store = store_handle();
        store.with_mut(|s| {
            for (id, project, at, pinned) in rows {
                s.insert_summary_with_times_for_test(
                    id,
                    None,
                    *at,
                    *at,
                    project,
                    None,
                    AccessMode::default().as_i64(),
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
        let (store, _path) = ordered_store(&[
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
    }

    #[test]
    fn a_thread_move_records_into_the_account_without_reordering_the_list() {
        let (store, _path) = ordered_store(&[
            ("t1", "/p/a", 100, false),
            ("t2", "/p/a", 200, false),
            ("t3", "/p/a", 300, false),
        ]);
        assert_eq!(ids(&store), vec!["t3", "t2", "t1"]);
        store.with_mut(|s| {
            s.insert_thread_before("t1", Some("t3"));
            assert!(s.order_dirty, "a real move must persist");
        });
        // The display order is the interaction rank; a stored move never
        // overrides it — only a pin (the head band) or a newer human
        // interaction moves a row.
        assert_eq!(ids(&store), vec!["t3", "t2", "t1"]);
        assert_eq!(
            store.read(|s| s.order.accounts["/p/a"].clone()),
            vec!["t1".to_string(), "t3".to_string(), "t2".to_string()],
            "the account must record the move"
        );
    }

    #[test]
    fn a_move_is_rejected_when_the_anchor_lives_elsewhere() {
        let (store, _path) =
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
    }

    #[test]
    fn pinning_floats_the_row_to_its_partition_head() {
        // A pin journals its decision and persists the account, both of which
        // need the runtime — so the account file goes to a scratch path.
        crate::runtime::init_hermetic_for_test();
        let scratch = std::env::temp_dir().join(format!("order-{}.json", uuid::Uuid::new_v4()));
        crate::sidebar_order::set_order_path_for_test(Some(scratch.clone()));
        let (store, _path) = ordered_store(&[
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
        // The pinned band ranks by interaction too: t2 (200) leads t3 (100).
        assert_eq!(ids(&store), vec!["t2", "t3", "t1"]);
        // Unpinning drops the row back into the interaction rank.
        store.with_mut(|s| s.pin_thread("t2", false));
        assert_eq!(ids(&store), vec!["t3", "t1", "t2"]);
        std::fs::remove_file(&scratch).ok();
        crate::sidebar_order::set_order_path_for_test(None);
    }

    #[test]
    fn a_new_row_surfaces_at_the_head_of_its_partition() {
        let (store, _path) = ordered_store(&[("t1", "/p/a", 100, false)]);
        store.with_mut(|s| {
            s.insert_summary_with_times_for_test(
                "t9",
                None,
                900,
                900,
                "/p/a",
                None,
                AccessMode::default().as_i64(),
            );
            s.rerank();
        });
        assert_eq!(ids(&store), vec!["t9", "t1"]);
    }

    #[test]
    fn folders_follow_the_committed_order_and_the_registry_projects_it() {
        let (store, _path) = ordered_store(&[
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
    }

    #[test]
    fn archiving_leaves_the_account_and_unarchiving_surfaces_at_the_head() {
        // Archiving journals its decision and persists the account, both of
        // which need the runtime — so the account file goes to a scratch path.
        crate::runtime::init_hermetic_for_test();
        let scratch = std::env::temp_dir().join(format!("order-{}.json", uuid::Uuid::new_v4()));
        crate::sidebar_order::set_order_path_for_test(Some(scratch.clone()));
        let (store, _path) =
            ordered_store(&[("t1", "/p/a", 100, false), ("t2", "/p/a", 200, false)]);
        store.with_mut(|s| s.archive_thread("t1", true));
        assert_eq!(ids(&store), vec!["t2"]);
        assert!(
            !store.read(|s| s.order.accounts["/p/a"].contains(&"t1".to_string())),
            "an archived row leaves the account"
        );
        store.with_mut(|s| s.archive_thread("t1", false));
        // A row that returns lands by its interaction stamp: t2 (200) is
        // newer than t1 (100), so t1 surfaces below it.
        assert_eq!(ids(&store), vec!["t2", "t1"]);
        std::fs::remove_file(&scratch).ok();
        crate::sidebar_order::set_order_path_for_test(None);
    }

    // ── the read rule: two monotone stamps, one derived bit ────────────────

    /// The one read rule's table: a session is unread when a turn settled
    /// after the last time it was viewed. Missing stamps mean "never
    /// looked" (any settle reads unread) and "nothing settled since
    /// tracking" (the upgrade path, clear), and the strict comparison makes
    /// a same-second settle+view read as viewed.
    #[test]
    fn the_read_rule_is_settled_after_viewed() {
        use crate::db::derive_unread;
        assert!(!derive_unread(None, None), "the upgrade path reads clear");
        assert!(
            derive_unread(Some(100), None),
            "a settled turn nobody viewed is unread"
        );
        assert!(!derive_unread(None, Some(100)), "nothing settled is clear");
        assert!(
            !derive_unread(Some(100), Some(200)),
            "viewed after settle is clear"
        );
        assert!(
            !derive_unread(Some(200), Some(200)),
            "a same-second settle+view reads viewed (second precision, conservative)"
        );
        assert!(
            derive_unread(Some(201), Some(200)),
            "settled after view is unread"
        );
    }

    /// The stamps are monotone (max) and the bit re-derives on every merge;
    /// stale writes — a late settle older than the view, a late view older
    /// than the settle — change nothing.
    #[test]
    fn stamps_merge_monotone_and_rederive_the_bit() {
        let mut row = crate::db::ThreadSummary {
            id: "t-merge".to_string(),
            ..Default::default()
        };
        assert!(row.merge_stamps(Some(100), None), "the first settle lights");
        assert!(
            !row.merge_stamps(Some(50), None),
            "a stale settle neither lowers the stamp nor flips the bit"
        );
        assert!(row.merge_stamps(None, Some(200)), "the view clears it");
        assert!(
            !row.merge_stamps(None, Some(100)),
            "a stale view never re-lights it"
        );
        assert!(
            row.merge_stamps(Some(300), None),
            "a fresh settle re-lights"
        );
        assert_eq!(row.last_settled_at, Some(300));
        assert_eq!(row.last_viewed_at, Some(200));
    }

    /// A view the store cannot persist yet (the session's sidecar path has
    /// not materialized) parks in memory and survives the first scan that
    /// materializes the row — without it, "watched from birth" sessions
    /// would light as unread on their first settle. The park retires only
    /// once the disk carries the stamp (the decision-overlay discipline).
    #[tokio::test]
    async fn a_view_for_an_unscanned_session_parks_until_the_scan_materializes_it() {
        let _db_path = temp_db();
        let store = standalone_for_test();
        crate::runtime::init_hermetic_for_test();

        // No row, no path: the view parks and lights nothing.
        store.with_mut(|s| assert!(!s.mark_viewed("t-parked", 500)));
        assert_eq!(
            store.read(|s| s.pending_viewed.get("t-parked").copied()),
            Some(500)
        );

        let dir = store.read(|s| s.sessions_dir.clone());
        write_session_fixture(&dir, "t-parked", "hello");
        store.refresh_now().await;
        let stamps = |store: &StoreHandle| {
            store
                .read(|s| {
                    s.summary_by_id("t-parked")
                        .map(|row| (row.last_viewed_at, row.has_unread))
                })
                .expect("the scan materialized the row")
        };
        assert_eq!(
            stamps(&store),
            (Some(500), false),
            "the parked view survives the scan that materializes the row: \
             the session reads viewed (nothing settled yet), not 'never looked'"
        );
        assert!(
            !store.read(|s| s.pending_viewed.is_empty()),
            "the park holds until the disk carries the stamp"
        );

        // The queued sidecar write drains at the next pass's head; the scan
        // then OBSERVES the stamp on disk and retires the park.
        store.refresh_now().await;
        assert_eq!(
            stamps(&store),
            (Some(500), false),
            "the disk carries the view now — still read"
        );
        assert!(
            store.read(|s| s.pending_viewed.is_empty()),
            "an observed park retires"
        );

        // And the rule the park existed for: a settle after the view lights.
        store.with_mut(|s| assert!(s.note_settled("t-parked", 600)));
        assert!(stamps(&store).1, "a settle after the view is unread");
    }

    /// The sweep's licence to poll: a pass over an unchanged store
    /// broadcasts nothing. Quiet passes are what keep the 5s tick a
    /// freshness knob instead of a re-announce storm (a re-ranked sidebar,
    /// a re-published catalogue row).
    #[tokio::test]
    async fn a_quiet_pass_broadcasts_nothing() {
        let _db_path = temp_db();
        let store = standalone_for_test();
        crate::runtime::init_hermetic_for_test();
        let dir = store.read(|s| s.sessions_dir.clone());
        write_session_fixture(&dir, "t-quiet", "hello");
        let rx = store.subscribe();

        store.refresh_now().await;
        let announced = count_summaries_updated(&rx);
        assert!(announced >= 1, "the first pass announces unconditionally");

        store.refresh_now().await;
        assert_eq!(
            count_summaries_updated(&rx),
            0,
            "a fingerprint-identical pass is silent"
        );

        // A real change still announces: a settle flips the derived bit.
        store.with_mut(|s| {
            assert!(s.note_settled("t-quiet", 42));
        });
        assert_eq!(
            count_summaries_updated(&rx),
            1,
            "the derived bit's flip is announced"
        );
    }

    /// The quiet gate on a MULTI-row store: the fresh rows walk a HashMap
    /// (group_by_thread) while the held rows are reranked, so a direct
    /// slice compare would flag every pass as changed and turn each sweep
    /// tick into a re-announce. Five unchanged passes over three rows
    /// announce once — the first.
    #[tokio::test]
    async fn a_quiet_pass_is_quiet_on_a_multi_row_store() {
        let _db_path = temp_db();
        let store = standalone_for_test();
        crate::runtime::init_hermetic_for_test();
        let dir = store.read(|s| s.sessions_dir.clone());
        write_session_fixture(&dir, "t-quiet-a", "a");
        write_session_fixture(&dir, "t-quiet-b", "b");
        write_session_fixture(&dir, "t-quiet-c", "c");
        // Pin one row: the rerank then orders it ahead of the scan's
        // recency walk, so held order and fresh walk order structurally
        // disagree — exactly the false-change the gate must not fire on.
        let pin = manox_harness::session_meta::meta_path(&dir, &dir.join("t-quiet-b.jsonl"));
        std::fs::write(&pin, "{\"pinned\": true}").unwrap();
        let rx = store.subscribe();

        store.refresh_now().await;
        assert!(
            count_summaries_updated(&rx) >= 1,
            "the first pass announces unconditionally"
        );

        for _ in 0..5 {
            store.refresh_now().await;
        }
        assert_eq!(
            count_summaries_updated(&rx),
            0,
            "five unchanged passes over three rows (one pinned): the HashMap walk order \
             must not read as change"
        );
    }

    /// The cross-process leg — the whole derived model's premise: A's
    /// settle stamp (the host's sidecar write) surfaces through B's plain
    /// scan of the shared root, and B's view declaration clears A's badge
    /// on the next pass. No dispatch, no notice, one disk.
    #[tokio::test]
    async fn a_peer_settle_surfaces_through_a_shared_root_scan() {
        let _db_path = temp_db();
        crate::runtime::init_hermetic_for_test();
        let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let a = standalone_for_test_at(&dir);
        let b = standalone_for_test_at(&dir);
        write_session_fixture(&dir, "s-cross", "hello");

        // A scans the row in; the host leg then stamps the sidecar (the
        // settle write belongs to the host, not to any store).
        a.refresh_now().await;
        let path = a
            .read(|s| s.journal_path("s-cross"))
            .expect("the row's journal");
        manox_harness::session_meta::update(&dir, &path, |meta| {
            meta.last_settled_at = Some(42);
        })
        .await
        .expect("the peer stamp writes");

        // B's pass reads the stamp off the shared root and derives unread.
        b.refresh_now().await;
        assert!(
            b.read(|s| s.summary_by_id("s-cross").expect("listed").has_unread),
            "B derives unread from A's stamped sidecar"
        );

        // B's viewer declares the view. The write is QUEUED for the store's
        // pass runner (a direct `refresh_now` does not drain the queue), so
        // wait for it to land on the shared disk before A re-scans — racing
        // the spawned runner is the flake this test used to carry.
        b.with_mut(|s| assert!(s.mark_viewed("s-cross", 100)));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let viewed = manox_harness::session_meta::load(&dir, &path)
                .await
                .ok()
                .and_then(|meta| meta.last_viewed_at);
            if viewed == Some(100) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "B's view write never landed on the shared root"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        b.refresh_now().await;
        a.refresh_now().await;
        assert!(
            !a.read(|s| s.summary_by_id("s-cross").expect("listed").has_unread),
            "B's view clears A's badge too — one shared read fact"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The explicit mark-unread is a LOWERING: a pass that reads the sidecar
    /// before the queued write lands would win the max merge and republish the
    /// old view. The overlay forces the sentinel over that merge and retires
    /// only when a scan observes it on disk; a later view supersedes it.
    #[tokio::test]
    async fn a_marked_unread_holds_the_sentinel_over_a_stale_scan() {
        let _db_path = temp_db();
        crate::runtime::init_hermetic_for_test();
        let store = standalone_for_test();
        let dir = store.read(|s| s.sessions_dir.clone());

        // A recorded settle and an old view: read. The row has no scan path
        // yet, so neither stamp has a durable write to race.
        store.with_mut(|s| {
            s.insert_summary_for_test("s-unread", None);
            s.note_settled("s-unread", 42);
            s.mark_viewed("s-unread", 1_000);
        });
        assert!(!store.read(|s| s.summary_by_id("s-unread").expect("row").has_unread));

        // The disk still says "viewed" — what a scan that started before the
        // lowering's queued write would read.
        write_session_fixture(&dir, "s-unread", "hello");
        let path = dir.join("s-unread.jsonl");
        manox_harness::session_meta::update(&dir, &path, |meta| {
            meta.last_settled_at = Some(42);
            meta.last_viewed_at = Some(1_000);
        })
        .await
        .expect("the stale sidecar writes");

        // The lowering cannot persist yet (no scan path when it ran), so the
        // overlay is the only thing holding it.
        store.with_mut(|s| assert!(s.mark_unread("s-unread")));
        store.refresh_now().await;
        assert!(
            store.read(|s| s.summary_by_id("s-unread").expect("listed").has_unread),
            "the overlay holds the sentinel over the stale scan"
        );
        assert!(
            !store.read(|s| s.pending_unread.is_empty()),
            "the overlay waits for the disk"
        );

        // The deferred write lands: the disk carries the sentinel and the
        // overlay retires.
        manox_harness::session_meta::update(&dir, &path, |meta| {
            meta.last_viewed_at = Some(0);
        })
        .await
        .expect("the lowered sidecar writes");
        store.refresh_now().await;
        assert!(
            store.read(|s| s.summary_by_id("s-unread").expect("listed").has_unread),
            "the sentinel keeps the row unread"
        );
        assert!(
            store.read(|s| s.pending_unread.is_empty()),
            "an observed sentinel retires the overlay"
        );

        // A view supersedes the lowering (and drops its overlay).
        assert!(store.with_mut(|s| s.mark_viewed("s-unread", 2_000)));
        assert!(store.read(|s| s.pending_unread.is_empty()));
        store.refresh_now().await;
        assert!(
            !store.read(|s| s.summary_by_id("s-unread").expect("listed").has_unread),
            "a later view clears the bit"
        );
    }

    fn count_summaries_updated(rx: &async_channel::Receiver<Arc<ThreadStoreEvent>>) -> usize {
        let mut count = 0;
        while let Ok(ev) = rx.try_recv() {
            count += usize::from(matches!(*ev, ThreadStoreEvent::SummariesUpdated));
        }
        count
    }

    /// The sweep's weak handle must not pin the core: a replaced global or
    /// a test teardown drops the old store, and the poller retires on its
    /// next upgrade instead of keeping a stale core alive forever.
    #[test]
    fn a_downgraded_handle_releases_the_core() {
        let store = standalone_for_test();
        let weak = store.downgrade();
        assert!(weak.upgrade().is_some());
        drop(store);
        assert!(
            weak.upgrade().is_none(),
            "a dropped core is not kept alive by the poller's handle"
        );
    }
}
