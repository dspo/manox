//! Persistent thread → active-session pointer. A thread drives one session
//! at a time; swaps (`EnterWorktree`/`ExitWorktree`/`NewSession`/restore)
//! move the pointer, so the sidebar's thread view never depends on file
//! mtimes. The registry is best-effort UI truth: a failed write degrades the
//! list to "newest session wins" until the next successful write.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

/// One thread's persisted entry: the session file the thread drives now.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThreadRegistryEntry {
    /// The active session's id (its `<id>.jsonl` file stem).
    pub active_session: String,
}

#[cfg(any(test, feature = "test-support"))]
static TEST_PATH: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Point the registry at a scratch file (test-support only — the real state
/// home must never be touched by tests). `None` restores the default path.
#[cfg(any(test, feature = "test-support"))]
pub fn set_registry_path_for_test(path: Option<PathBuf>) {
    *TEST_PATH.lock().unwrap() = path;
}

/// The registry file under the manox state home.
pub fn registry_path() -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(path) = TEST_PATH.lock().unwrap().clone() {
        return path;
    }
    crate::paths::manox_config_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("threads.registry.json")
}

/// Load the registry. A missing file is an empty registry; a corrupt one
/// logs and degrades to empty (the next `set_active` rewrites it).
pub async fn load() -> HashMap<String, ThreadRegistryEntry> {
    let path = registry_path();
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(path = %path.display(), %error, "thread registry unreadable; self-healing on next write");
            HashMap::new()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "thread registry unreadable; treating as empty");
            HashMap::new()
        }
    }
}

/// Move a thread's active-session pointer. Read-modify-write under a
/// process-wide lock, atomic on disk (temp file + rename); failures warn
/// without propagating — the sidebar falls back to the newest session.
pub async fn set_active(thread_id: &str, session_id: &str) {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let _guard = LOCK.get_or_init(tokio::sync::Mutex::default).lock().await;
    let mut map = load().await;
    map.insert(
        thread_id.to_string(),
        ThreadRegistryEntry {
            active_session: session_id.to_string(),
        },
    );
    if let Err(error) = save(&map).await {
        tracing::warn!(error = %error, "failed to persist the thread registry");
    }
}

/// Atomic write (temp file + rename) so a crash cannot truncate the registry.
async fn save(map: &HashMap<String, ThreadRegistryEntry>) -> Result<(), anyhow::Error> {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let bytes = serde_json::to_vec_pretty(map)?;
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy())
            .unwrap_or_default()
    ));
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The path override is process-global; the async lock serializes the
    /// tests that move it (its guard may be held across awaits).
    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn scratch() -> (tokio::sync::MutexGuard<'static, ()>, tempfile::TempDir) {
        let guard = TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        set_registry_path_for_test(Some(dir.path().join("threads.registry.json")));
        (guard, dir)
    }

    #[tokio::test]
    async fn registry_round_trips_active_pointer() {
        let (_guard, _dir) = scratch().await;
        set_active("t1", "s1").await;
        let map = load().await;
        assert_eq!(map.get("t1").map(|e| e.active_session.as_str()), Some("s1"));
        // Moving the pointer overwrites in place; other threads stay put.
        set_active("t1", "s2").await;
        set_active("t2", "s9").await;
        let map = load().await;
        assert_eq!(map.get("t1").map(|e| e.active_session.as_str()), Some("s2"));
        assert_eq!(map.get("t2").map(|e| e.active_session.as_str()), Some("s9"));
    }

    #[tokio::test]
    async fn registry_missing_file_loads_empty() {
        let (_guard, _dir) = scratch().await;
        assert!(load().await.is_empty());
    }

    #[tokio::test]
    async fn registry_corrupt_file_degrades_and_self_heals() {
        let (_guard, dir) = scratch().await;
        let path = dir.path().join("threads.registry.json");
        tokio::fs::write(&path, "{not json").await.unwrap();
        assert!(load().await.is_empty());
        set_active("t1", "s1").await;
        assert_eq!(
            load().await.get("t1").map(|e| e.active_session.as_str()),
            Some("s1")
        );
    }

    #[tokio::test]
    async fn registry_concurrent_set_active_keeps_every_thread() {
        let (_guard, _dir) = scratch().await;
        let mut handles = Vec::new();
        for i in 0..8u32 {
            handles.push(tokio::spawn(async move {
                set_active(&format!("t{i}"), &format!("s{i}")).await;
            }));
        }
        for handle in handles {
            handle.await.unwrap();
        }
        let map = load().await;
        assert_eq!(map.len(), 8, "{map:?}");
        for i in 0..8u32 {
            assert_eq!(
                map.get(&format!("t{i}")).map(|e| e.active_session.as_str()),
                Some(format!("s{i}")).as_deref()
            );
        }
    }
}
