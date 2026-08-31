//! Process-global `TerminalStore` — gpui-free mirror of `agent::thread_store`.
//!
//! Holds an `Arc<ThreadsDatabase>` plus the current session-summary list
//! behind a `TerminalStoreHandle` (`Arc` + lock + channel broadcast).
//! `save_terminal` snapshots a `TerminalHandle`'s id/cwd/title, persists them
//! on the registered runtime, then refreshes the summary list so the sidebar
//! can list reopenable terminals.

use std::sync::Arc;

use chrono::Utc;

use agent::db::{TerminalSession, ThreadsDatabase, default_db_path};

use crate::TerminalHandle;

/// Events emitted by `TerminalStore` to subscribers (sidebar in stage 9).
#[derive(Debug, Clone)]
pub enum TerminalStoreEvent {
    /// The session summary list changed (created / saved / deleted).
    SummariesUpdated,
}

pub struct TerminalStore {
    db: Arc<ThreadsDatabase>,
    summaries: Vec<TerminalSession>,
    /// Events buffered under the state lock; [`TerminalStoreHandle::with_mut`]
    /// drains and broadcasts them once the mutation closure returns.
    pending_events: Vec<TerminalStoreEvent>,
}

/// The gpui-free handle to the terminal store. Cheap to clone (`Arc`); state
/// lives behind a lock and events broadcast to channel subscribers. This is
/// the unit the frontends (and the reopen paths) hold.
#[derive(Clone)]
pub struct TerminalStoreHandle(Arc<TerminalStoreCore>);

pub struct TerminalStoreCore {
    state: parking_lot::RwLock<TerminalStore>,
    /// Event subscribers. Carries `Arc<TerminalStoreEvent>` for parity with
    /// the `TerminalHandle` channel shape; the event is `Clone`, so the `Arc`
    /// can come off once the consumers settle.
    subscribers: parking_lot::Mutex<Vec<async_channel::Sender<Arc<TerminalStoreEvent>>>>,
}

impl TerminalStoreHandle {
    /// Wrap a freshly built [`TerminalStore`].
    pub fn new(store: TerminalStore) -> Self {
        Self(Arc::new(TerminalStoreCore {
            state: parking_lot::RwLock::new(store),
            subscribers: parking_lot::Mutex::new(Vec::new()),
        }))
    }

    /// Subscribe to this store's event stream.
    pub fn subscribe(&self) -> async_channel::Receiver<Arc<TerminalStoreEvent>> {
        let (tx, rx) = async_channel::unbounded();
        self.0.subscribers.lock().push(tx);
        rx
    }

    /// Shared-read the state.
    pub fn read<R>(&self, f: impl FnOnce(&TerminalStore) -> R) -> R {
        let state = self.0.state.read();
        f(&state)
    }

    /// Mutate under the write lock, then broadcast the buffered events.
    /// Three-phase: lock -> mutate (collecting `pending_events`) -> unlock ->
    /// emit. The closure must never await.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut TerminalStore) -> R) -> R {
        let (r, events) = {
            let mut state = self.0.state.write();
            let r = f(&mut state);
            let events = std::mem::take(&mut state.pending_events);
            (r, events)
        };
        self.broadcast(events);
        r
    }

    fn broadcast(&self, events: Vec<TerminalStoreEvent>) {
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

    /// Re-read the db and refresh the summary list. Runs on the registered
    /// runtime so a busy SQLite lock cannot stall the caller;
    /// `SummariesUpdated` broadcasts when the scan lands.
    pub fn refresh(&self) {
        let db = self.read(|s| s.db.clone());
        let this = self.clone();
        crate::runtime::handle().spawn(async move {
            match db.list_terminal_sessions() {
                Ok(list) => this.with_mut(|s| {
                    s.summaries = list;
                    s.pending_events.push(TerminalStoreEvent::SummariesUpdated);
                }),
                Err(e) => tracing::warn!(error = %e, "refresh terminal sessions failed"),
            }
        });
    }

    /// Persist a terminal's session metadata (id/cwd/title) on the runtime,
    /// then refresh the summary list. Scrollback is never persisted.
    /// `created_at` is preserved across re-saves; `updated_at` is bumped to now.
    pub fn save_terminal(&self, terminal: &TerminalHandle) {
        let (id, cwd, title) = terminal.read(|t| {
            (
                t.id.clone(),
                t.cwd.to_string_lossy().to_string(),
                t.title.clone(),
            )
        });
        let db = self.read(|s| s.db.clone());
        let now = Utc::now().timestamp();
        let this = self.clone();
        crate::runtime::handle().spawn(async move {
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
            this.refresh();
        });
    }

    /// Delete a session row and refresh.
    pub fn delete_session(&self, id: &str) {
        let db = self.read(|s| s.db.clone());
        let id = id.to_string();
        let this = self.clone();
        crate::runtime::handle().spawn(async move {
            if let Err(e) = db.delete_terminal_session(&id) {
                tracing::warn!(error = %e, "delete terminal session failed");
            }
            this.refresh();
        });
    }
}

/// `Mutex<Option<_>>` (not a `OnceLock`) so test-support can reset the
/// global between tests.
static GLOBAL: std::sync::Mutex<Option<TerminalStoreHandle>> = std::sync::Mutex::new(None);

/// Test-only override of the process-global `TerminalStore`. `init_for_test`
/// stores an in-memory-db-backed handle here so persistence-bearing tests
/// don't touch the real `~/.manox/threads.db`; `drop_for_test` clears it.
/// Mirrors `thread_store`'s override slot.
#[cfg(any(test, feature = "test-support"))]
static TEST_OVERRIDE: std::sync::Mutex<Option<TerminalStoreHandle>> = std::sync::Mutex::new(None);

/// Open the db, load the session list, and register the process-global
/// handle. Call at App startup, after `terminal::runtime::set_runtime`.
pub fn init() {
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
    let handle = TerminalStoreHandle::new(TerminalStore {
        db: Arc::new(db),
        summaries,
        pending_events: Vec::new(),
    });
    *GLOBAL.lock().unwrap() = Some(handle);
}

/// Returns the global [`TerminalStoreHandle`]. Panics if `init` was not called.
pub fn global() -> TerminalStoreHandle {
    try_global().expect("TerminalStore not initialized, call terminal::init first")
}

/// The global store when initialized (`init` or `init_for_test`); `None`
/// before init so teardown paths can skip store work instead of panicking.
pub fn try_global() -> Option<TerminalStoreHandle> {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(handle) = TEST_OVERRIDE.lock().unwrap().clone() {
        return Some(handle);
    }
    GLOBAL.lock().unwrap().clone()
}

/// Test-only initializer that primes the process-global `TerminalStore` with
/// a caller-provided db (typically `:memory:`) so persistence-bearing tests
/// don't touch the real `~/.manox/threads.db`. Pair every call with
/// `drop_for_test`.
#[cfg(any(test, feature = "test-support"))]
pub fn init_for_test(db: Arc<ThreadsDatabase>) {
    let summaries = db.list_terminal_sessions().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "load terminal sessions failed, starting empty");
        Vec::new()
    });
    let handle = TerminalStoreHandle::new(TerminalStore {
        db,
        summaries,
        pending_events: Vec::new(),
    });
    *TEST_OVERRIDE.lock().unwrap() = Some(handle);
}

/// Release the test-only `TerminalStore` handle so the process-global slot
/// does not leak into other tests. Call this at the end of any test that used
/// `init_for_test` (a Drop guard is the robust pattern).
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
}
