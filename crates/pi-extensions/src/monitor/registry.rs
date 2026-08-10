//! Self-managed registry for WebSocket monitors.
//!
//! Unlike command monitors (which reuse `BackgroundRegistry`), WebSocket
//! monitors have no overlapping process-management needs. This registry
//! tracks WS connection handles, cancel tokens, and metadata.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::task::{AbortHandle, JoinHandle};

/// How long a finished monitor stays in the registry before GC sweeps it.
const GC_AFTER_EXIT: Duration = Duration::from_secs(300);

/// A unique identifier for a WebSocket monitor.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WsTaskId(pub String);

impl std::fmt::Display for WsTaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// The status of a WebSocket monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsTaskStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Stopped,
}

/// Snapshot of a registry entry for UI / audit consumers.
#[derive(Debug, Clone)]
pub struct WsSnapshot {
    pub url: String,
    pub status: WsTaskStatus,
    pub created_at: Instant,
    pub ended_at: Option<Instant>,
}

struct WsEntry {
    url: String,
    status: WsTaskStatus,
    cancel: tokio_util::sync::CancellationToken,
    /// The driver task's abort handle (`Clone`, unlike `JoinHandle`).
    /// `abort()` forces termination when a graceful token cancel cannot
    /// (stuck handshake/read).
    driver: Option<AbortHandle>,
    created_at: Instant,
    ended_at: Option<Instant>,
}

/// Registry of WebSocket monitors.
pub struct WsMonitorRegistry {
    entries: Mutex<HashMap<String, WsEntry>>,
    next_id: AtomicU64,
}

impl WsMonitorRegistry {
    pub fn new() -> Self {
        WsMonitorRegistry {
            entries: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// Allocate a new task id and register it as Running.
    pub fn register(&self, url: String, cancel: tokio_util::sync::CancellationToken) -> WsTaskId {
        self.gc();
        let id = WsTaskId(format!(
            "ws_{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        ));
        self.entries.lock().expect("entries lock poisoned").insert(
            id.0.clone(),
            WsEntry {
                url,
                status: WsTaskStatus::Running,
                cancel,
                driver: None,
                created_at: Instant::now(),
                ended_at: None,
            },
        );
        id
    }

    /// Attach the driver task handle after `tokio::spawn` (the handle does
    /// not exist at registration time). Only the `AbortHandle` is kept — it
    /// is `Clone`, so `abort` can fire outside the registry lock.
    pub fn set_driver(&self, id: &WsTaskId, driver: JoinHandle<()>) {
        let abort_handle = driver.abort_handle();
        let mut entries = self.entries.lock().expect("entries lock poisoned");
        if let Some(entry) = entries.get_mut(&id.0) {
            entry.driver = Some(abort_handle);
        }
    }

    /// Mark a task's terminal status.
    pub fn set_status(&self, id: &WsTaskId, status: WsTaskStatus) {
        let mut entries = self.entries.lock().expect("entries lock poisoned");
        if let Some(entry) = entries.get_mut(&id.0) {
            entry.status = status;
            if status != WsTaskStatus::Running {
                entry.ended_at = Some(Instant::now());
            }
        }
    }

    /// Request a graceful stop: cancel the token and let the driver's exit
    /// path flush and record its own terminal status.
    pub fn cancel(&self, id: &WsTaskId) -> bool {
        self.cancel_str(&id.0)
    }

    /// String-keyed `cancel` (the `TaskStopTool` fallback path holds a bare
    /// id string).
    pub fn cancel_str(&self, id: &str) -> bool {
        let entries = self.entries.lock().expect("entries lock poisoned");
        if let Some(entry) = entries.get(id) {
            entry.cancel.cancel();
            true
        } else {
            false
        }
    }

    /// Force-stop: cancel the token and abort the driver task. Used by
    /// teardown paths that cannot await the graceful exit (a `Drop`).
    /// Aborting runs outside the registry lock.
    pub fn abort(&self, id: &WsTaskId) -> bool {
        let driver = {
            let entries = self.entries.lock().expect("entries lock poisoned");
            match entries.get(&id.0) {
                Some(entry) => {
                    entry.cancel.cancel();
                    entry.driver.clone()
                }
                None => return false,
            }
        };
        if let Some(driver) = driver {
            driver.abort();
        }
        true
    }

    /// List all active (running) task ids.
    pub fn active_ids(&self) -> Vec<WsTaskId> {
        self.entries
            .lock()
            .expect("entries lock poisoned")
            .iter()
            .filter(|(_, e)| e.status == WsTaskStatus::Running)
            .map(|(id, _)| WsTaskId(id.clone()))
            .collect()
    }

    /// Snapshot of one entry, for UI / audit consumers.
    pub fn snapshot(&self, id: &WsTaskId) -> Option<WsSnapshot> {
        self.entries
            .lock()
            .expect("entries lock poisoned")
            .get(&id.0)
            .map(|e| WsSnapshot {
                url: e.url.clone(),
                status: e.status,
                created_at: e.created_at,
                ended_at: e.ended_at,
            })
    }

    /// Sweep tasks that ended long enough ago.
    pub fn gc(&self) {
        let now = Instant::now();
        self.entries
            .lock()
            .expect("entries lock poisoned")
            .retain(|_, e| match e.ended_at {
                Some(t) => now.duration_since(t) < GC_AFTER_EXIT,
                None => true,
            });
    }
}

impl Default for WsMonitorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_list_active() {
        let reg = WsMonitorRegistry::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        let id = reg.register("wss://example.com/ws".into(), cancel.clone());
        assert!(id.0.starts_with("ws_"));
        assert_eq!(reg.active_ids().len(), 1);
        reg.set_status(&id, WsTaskStatus::Completed);
        assert!(reg.active_ids().is_empty());
    }

    #[test]
    fn cancel_returns_true_for_existing_task() {
        let reg = WsMonitorRegistry::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        let id = reg.register("wss://example.com/ws".into(), cancel.clone());
        assert!(reg.cancel(&id));
        assert!(reg.cancel_str(&id.0));
        assert!(!reg.cancel(&WsTaskId("nonexistent".into())));
        assert!(!reg.cancel_str("nonexistent"));
        assert!(cancel.is_cancelled());
    }

    #[test]
    fn snapshot_exposes_metadata() {
        let reg = WsMonitorRegistry::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        let id = reg.register("wss://example.com/ws".into(), cancel);
        let snap = reg.snapshot(&id).expect("registered entry");
        assert_eq!(snap.url, "wss://example.com/ws");
        assert_eq!(snap.status, WsTaskStatus::Running);
        assert!(snap.ended_at.is_none());
        reg.set_status(&id, WsTaskStatus::Failed);
        let snap = reg.snapshot(&id).unwrap();
        assert_eq!(snap.status, WsTaskStatus::Failed);
        assert!(snap.ended_at.is_some());
    }

    #[tokio::test]
    async fn abort_cancels_token_and_aborts_driver() {
        let reg = WsMonitorRegistry::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        let id = reg.register("wss://example.com/ws".into(), cancel.clone());
        let handle = tokio::spawn(std::future::pending::<()>());
        reg.set_driver(&id, handle);
        assert!(reg.abort(&id));
        assert!(cancel.is_cancelled());
        assert!(!reg.abort(&WsTaskId("nonexistent".into())));
    }

    #[test]
    fn gc_cleans_expired_entries() {
        let reg = WsMonitorRegistry::new();
        let cancel = tokio_util::sync::CancellationToken::new();
        let id = reg.register("wss://example.com/ws".into(), cancel);
        // Simulate a completed task from long ago.
        {
            let mut entries = reg.entries.lock().unwrap();
            let entry = entries.get_mut(&id.0).unwrap();
            entry.status = WsTaskStatus::Completed;
            entry.ended_at = Some(Instant::now() - GC_AFTER_EXIT - Duration::from_secs(1));
        }
        reg.gc();
        assert!(reg.entries.lock().unwrap().is_empty());
    }
}
