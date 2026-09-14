//! Per-session write leases — the multi-instance concurrency primitive.
//!
//! manox runs multi-instance: the desktop app, `cx web`, and embedder
//! hosts may share one state root. Nobody may drive (append turns to) a
//! session another process is driving. The lease is an exclusive
//! non-blocking `flock` over the sibling `<id>.jsonl.lock` file, acquired
//! once per driven session and held for the engine actor's lifetime:
//!
//! - fail-fast — a contender surfaces `LeaseError::HeldElsewhere`, mapped
//!   to the `session/already-owned` RPC code;
//! - held by the actor (not the facade handle): the actor drains and
//!   settles final rows after the facade drops, so the lease must outlive
//!   the facade — and it must not outlive the actor, or a closed session
//!   would stay locked against every other process until exit;
//! - crash-safe with no recovery path: the kernel releases the flock when
//!   the holder dies, and the lock file is never unlinked so its inode is
//!   stable;
//! - reentrant per process: a second acquire of a live lease returns the
//!   same entry (reconnects and co-viewing clients within one process are
//!   legal);
//! - read-only paths (cold reads, fork sources) never take it.
//!
//! Within one append the journal's `WriteFence` still serializes bytes on
//! disk; the lease decides WHO may append, the fence decides the order.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

/// Typed failure of [`acquire`].
#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("another process holds the write lease for {}", path.display())]
    HeldElsewhere { path: PathBuf },
    #[error("cannot acquire the write lease for {}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// One held lease. The flock lives until the last `Arc<LeaseEntry>`
/// drops; the field is a pure guard, never read.
#[derive(Debug)]
pub struct LeaseEntry {
    _lock: manox_harness::fs_lock::FileLock,
}

/// Process-wide lease registry. Values are `Weak`: the map never keeps a
/// lease alive — when the last external holder (the engine actor) drops,
/// the weak dies and the flock releases with it; the next acquire flocks
/// fresh. Dead entries are replaced in place on access, and the map is
/// unbounded by design: one small entry per session ever driven,
/// process-lifetime.
static LEASES: OnceLock<Mutex<HashMap<PathBuf, Weak<LeaseEntry>>>> = OnceLock::new();

fn leases() -> &'static Mutex<HashMap<PathBuf, Weak<LeaseEntry>>> {
    LEASES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Acquire (or join) this process's write lease for the session file.
/// Contention with another process is fail-fast but not literally
/// single-shot: the budget is a fraction of a second so the macOS
/// close-release propagation quirk (see `manox_harness::fs_lock`) can
/// never surface as a phantom `HeldElsewhere` — a genuinely driven
/// session still times out into the error. Driving is exclusive, never
/// queued.
pub fn acquire(session_path: &Path) -> Result<Arc<LeaseEntry>, LeaseError> {
    const ACQUIRE_BUDGET: Duration = Duration::from_millis(250);
    let mut map = leases().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = map.get(session_path).and_then(Weak::upgrade) {
        return Ok(entry);
    }
    let lock_path = manox_harness::fs_lock::lock_path_for(session_path);
    let _lock =
        manox_harness::fs_lock::lock_exclusive(&lock_path, ACQUIRE_BUDGET).map_err(|err| {
            if err.kind() == std::io::ErrorKind::WouldBlock {
                LeaseError::HeldElsewhere {
                    path: session_path.to_path_buf(),
                }
            } else {
                LeaseError::Io {
                    path: session_path.to_path_buf(),
                    source: err,
                }
            }
        })?;
    let entry = Arc::new(LeaseEntry { _lock });
    map.insert(session_path.to_path_buf(), Arc::downgrade(&entry));
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is process-global; this lock serializes lease tests so
    /// one test's temp paths never collide with another's (each test uses
    /// its own paths, but the serialization keeps the Weak-slot reasoning
    /// single-threaded).
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn same_process_acquire_joins_the_live_lease() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let session = tempfile::tempdir().unwrap().keep().join("s.jsonl");
        let first = acquire(&session).unwrap();
        let second = acquire(&session).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        drop(first);
        drop(second);
    }

    #[test]
    fn foreign_holder_surfaces_as_held_elsewhere() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let session = tempfile::tempdir().unwrap().keep().join("s.jsonl");
        // A second fd standing in for the other process: flock contention
        // does not care which process owns the conflicting description.
        let holder = manox_harness::fs_lock::lock_exclusive(
            &manox_harness::fs_lock::lock_path_for(&session),
            Duration::ZERO,
        )
        .unwrap();
        match acquire(&session) {
            Err(LeaseError::HeldElsewhere { .. }) => {}
            other => panic!("expected HeldElsewhere, got {other:?}"),
        }
        drop(holder);
        assert!(acquire(&session).is_ok());
    }

    #[test]
    fn lease_releases_when_the_last_holder_drops_and_the_lock_file_persists() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("s.jsonl");
        let lock_path = manox_harness::fs_lock::lock_path_for(&session);
        let entry = acquire(&session).unwrap();
        drop(entry);
        // The registry's Weak died with the holder: a raw flock now
        // succeeds (another process could take over) — bounded, not ZERO,
        // for the macOS close-release quirk — and the lock file stays on
        // disk: never unlinked, its inode is the lock identity.
        assert!(manox_harness::fs_lock::lock_exclusive(&lock_path, Duration::from_secs(1)).is_ok());
        assert!(lock_path.exists());
    }
}
