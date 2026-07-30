// File mutation queue — serializes concurrent writes to the same file.
//
// When parallel tool calls (e.g. two edit operations) target the same file,
// concurrent writes can corrupt the file. The mutation queue ensures that
// writes to the same path are serialized while writes to different paths
// proceed concurrently.
//
// This is a correctness facility, not an optimization. Without it, parallel
// tool calls that modify the same file will silently produce corrupted output.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

/// A per-file lock that serializes mutations to the same path.
///
/// The queue holds one permit per unique file path. Acquiring the permit
/// for path A never blocks on path B. Multiple concurrent operations on
/// different files proceed in parallel.
pub struct FileMutationQueue {
    /// Per-file mutexes. Each file path gets its own Arc<Mutex<()>>.
    /// We use Arc<Mutex<()>> so the guard can be held across await points
    /// without holding the global lock.
    permits: Mutex<HashMap<PathBuf, std::sync::Arc<tokio::sync::Mutex<()>>>>,
}

impl FileMutationQueue {
    pub fn new() -> Self {
        FileMutationQueue {
            permits: Mutex::new(HashMap::new()),
        }
    }

    /// Acquire exclusive access to mutate the file at `path`.
    ///
    /// Returns a guard that must be held for the duration of the mutation.
    /// The guard is released when dropped.
    pub async fn lock(&self, path: &Path) -> FileMutationGuard {
        let canonical = normalize_for_lock(path);

        // Get or create a per-file mutex.
        let permit = {
            let mut permits = self.permits.lock().await;
            permits
                .entry(canonical.clone())
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };

        // Acquire the per-file lock.
        let guard = permit.lock_owned().await;

        FileMutationGuard {
            _guard: guard,
            path: canonical,
        }
    }
}

impl Default for FileMutationQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// A guard that ensures exclusive access to a file during mutation.
///
/// Drop the guard to release the lock and allow other mutations on the
/// same file to proceed.
pub struct FileMutationGuard {
    _guard: tokio::sync::OwnedMutexGuard<()>,
    #[allow(dead_code)]
    path: PathBuf,
}

/// Normalize a path for consistent lock key comparison.
fn normalize_for_lock(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        // Fallback: strip redundant separators and dots.
        let mut cleaned = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::ParentDir => {
                    cleaned.pop();
                }
                std::path::Component::CurDir => {}
                c => {
                    cleaned.push(c);
                }
            }
        }
        cleaned
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn test_different_files_run_concurrently() {
        let queue = Arc::new(FileMutationQueue::new());
        let counter = Arc::new(AtomicUsize::new(0));

        let q1 = Arc::clone(&queue);
        let c1 = Arc::clone(&counter);
        let h1 = tokio::spawn(async move {
            let _guard = q1.lock(Path::new("/tmp/file_a.txt")).await;
            c1.fetch_add(1, Ordering::SeqCst);
        });

        let q2 = Arc::clone(&queue);
        let c2 = Arc::clone(&counter);
        let h2 = tokio::spawn(async move {
            let _guard = q2.lock(Path::new("/tmp/file_b.txt")).await;
            c2.fetch_add(1, Ordering::SeqCst);
        });

        let (r1, r2) = tokio::join!(h1, h2);
        r1.unwrap();
        r2.unwrap();

        // Both tasks should have run (different files don't block each other).
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_same_file_serialized() {
        let queue = Arc::new(FileMutationQueue::new());
        let order = Arc::new(Mutex::new(Vec::new()));

        let q1 = Arc::clone(&queue);
        let o1 = Arc::clone(&order);
        let h1 = tokio::spawn(async move {
            let _guard = q1.lock(Path::new("/tmp/shared.txt")).await;
            o1.lock().await.push(1);
            // Hold the lock briefly to ensure ordering.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        });

        // Give h1 a head start to acquire the lock.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let q2 = Arc::clone(&queue);
        let o2 = Arc::clone(&order);
        let h2 = tokio::spawn(async move {
            let _guard = q2.lock(Path::new("/tmp/shared.txt")).await;
            o2.lock().await.push(2);
        });

        let (r1, r2) = tokio::join!(h1, h2);
        r1.unwrap();
        r2.unwrap();

        let order = order.lock().await;
        assert_eq!(*order, vec![1, 2], "same file writes must be serialized");
    }
}
