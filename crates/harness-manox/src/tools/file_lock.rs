//! Process-global file write lock registry.
//!
//! NOWAIT try-lock: concurrent writes to the same path are rejected with an
//! error so agents coordinate disjoint write ranges instead of silently
//! clobbering one another. The lock is the enforced backstop behind the
//! system-prompt convention "assign disjoint write ranges"; contention is
//! expected to be near-zero, and a conflict is a signal to re-coordinate,
//! not a silent stall.
//!
//! Reads are not locked — a torn read is recovered by `edit_file`'s stale-TAG
//! re-read path, and adding a shared read lock would entangle with the NOWAIT
//! write semantics for no real benefit. `bash` writes are also out of scope:
//! a shell command's touched paths are not statically knowable, so bash-heavy
//! work is coordinated by assigning it to disjoint directories.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Who currently holds the exclusive write lock on a path.
#[derive(Clone, Debug)]
pub struct HeldBy {
    /// The owning agent's label (member name / subagent_type / "lead").
    pub owner: String,
    pub acquired_at: Instant,
}

struct Registry {
    entries: Mutex<HashMap<PathBuf, HeldBy>>,
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

fn registry() -> &'static Registry {
    REGISTRY.get_or_init(|| Registry {
        entries: Mutex::new(HashMap::new()),
    })
}

/// Normalize a resolved path to a stable lock key. Canonicalizes when the
/// file (or its parent) exists so two writers that spell the same target
/// differently still collide; falls back to the resolved absolute path for
/// not-yet-created files, matching `resolve_path`'s non-canonicalizing stance
/// used by the hashline snapshot keys.
fn key(path: &Path) -> PathBuf {
    if let Ok(canon) = path.canonicalize() {
        return canon;
    }
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(canon_parent) = parent.canonicalize()
    {
        return canon_parent.join(name);
    }
    path.to_path_buf()
}

/// Try to acquire an exclusive write lock on `path` for `owner`. On success
/// returns a guard that releases on drop; on conflict returns the current
/// holder so the caller can name it in the error.
pub fn try_acquire(path: &Path, owner: &str) -> Result<FileWriteGuard, HeldBy> {
    let key = key(path);
    let mut entries = registry()
        .entries
        .lock()
        .expect("file write lock registry poisoned");
    if let Some(held) = entries.get(&key) {
        return Err(held.clone());
    }
    entries.insert(
        key.clone(),
        HeldBy {
            owner: owner.to_string(),
            acquired_at: Instant::now(),
        },
    );
    Ok(FileWriteGuard { key: Some(key) })
}

/// RAII guard that releases the held write lock on drop.
#[derive(Debug)]
pub struct FileWriteGuard {
    key: Option<PathBuf>,
}

impl FileWriteGuard {
    /// Release the lock early. The guard's `Drop` is a no-op after this.
    pub fn release(mut self) {
        if let Some(key) = self.key.take() {
            let mut entries = registry()
                .entries
                .lock()
                .expect("file write lock registry poisoned");
            entries.remove(&key);
        }
    }
}

impl Drop for FileWriteGuard {
    fn drop(&mut self) {
        if let Some(key) = self.key.take() {
            let mut entries = registry()
                .entries
                .lock()
                .expect("file write lock registry poisoned");
            entries.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_acquire_wins_second_gets_holder() {
        let a = Path::new("/tmp/manox-file-lock-acquire");
        let g = try_acquire(a, "lead").expect("first acquire");
        let err = try_acquire(a, "plan").unwrap_err();
        assert_eq!(err.owner, "lead");
        drop(g);
    }

    #[test]
    fn drop_releases() {
        let p = Path::new("/tmp/manox-file-lock-drop");
        {
            let _g = try_acquire(p, "lead").expect("acquire");
            assert!(try_acquire(p, "plan").is_err());
        }
        assert!(try_acquire(p, "plan").is_ok(), "released after drop");
    }

    #[test]
    fn release_is_idempotent_with_drop() {
        let p = Path::new("/tmp/manox-file-lock-release");
        let g = try_acquire(p, "lead").expect("acquire");
        g.release();
        assert!(try_acquire(p, "plan").is_ok(), "released early");
    }

    #[test]
    fn distinct_paths_do_not_collide() {
        let a = Path::new("/tmp/manox-file-lock-distinct-a");
        let b = Path::new("/tmp/manox-file-lock-distinct-b");
        let _ga = try_acquire(a, "lead").expect("a");
        let gb = try_acquire(b, "plan").expect("b should not collide with a");
        gb.release();
    }
}
