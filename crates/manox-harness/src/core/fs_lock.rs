//! Cross-process advisory locks over persistent sibling lock files.
//!
//! Shared read-modify-write state (the thread registry, the sidebar order,
//! session sidecars, the gateway endpoint) lives in files under the state
//! root that MORE THAN ONE manox process may touch concurrently — the
//! runtime is multi-instance by design. Each guarded file gets a sibling
//! `<name>.lock` file; the lock is an exclusive `flock` on an fd held for
//! the critical section.
//!
//! Two invariants make the locks crash-safe without any stale-lock
//! recovery: the lock file is created once and NEVER unlinked or replaced
//! (its inode is stable, so a rename can never split lockers onto two
//! inodes), and `flock` is held per open file description — the kernel
//! releases it when the holder's last fd closes, so a crashed process can
//! never leave a lock behind.
//!
//! Budgets exist so a TRANSIENT contention is never misread as ownership:
//! fail-fast callers (the session lease, the gateway lock) pass a small
//! non-zero budget instead of [`Duration::ZERO`]. A genuinely held lock —
//! including one leaked by a bug in the holding process — still times out
//! into `WouldBlock`, which is the correct answer for it.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The retry cadence for contended acquires. Lock critical sections are
/// single small file writes, so waiters park briefly.
const RETRY_INTERVAL: Duration = Duration::from_millis(20);

/// The lock file guarding `guarded`: the full name with `.lock` appended,
/// so `<id>.jsonl` fences behind `<id>.jsonl.lock` (`with_extension` would
/// mangle multi-dot names).
pub fn lock_path_for(guarded: &Path) -> PathBuf {
    let mut name = guarded.as_os_str().to_os_string();
    name.push(".lock");
    guarded.with_file_name(name)
}

/// A held lock. Dropping the guard closes the fd and releases the flock.
#[derive(Debug)]
pub struct FileLock {
    _file: File,
}

/// One `LOCK_EX | LOCK_NB` attempt on `lock_path`, creating the lock file
/// if absent. `Err(WouldBlock)` when someone else holds it.
fn try_once(lock_path: &Path) -> io::Result<FileLock> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    let fd = file.as_raw_fd();
    // SAFETY: `flock` is a standard POSIX operation on a valid owned fd.
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret == 0 {
        return Ok(FileLock { _file: file });
    }
    let err = io::Error::last_os_error();
    let contended = err
        .raw_os_error()
        .is_some_and(|code| code == libc::EWOULDBLOCK || code == libc::EAGAIN);
    if contended {
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("lock held elsewhere: {}", lock_path.display()),
        ))
    } else {
        Err(err)
    }
}

/// Acquire the exclusive lock, retrying contention until `budget` elapses.
/// `budget == ZERO` is a single attempt. The lock file's parent directory
/// is created first — a lock file must be addressable wherever the
/// guarded file will be, including a deferred-fresh session whose journal
/// directory does not exist yet. Contention that outlives the budget
/// returns `WouldBlock`; other IO errors propagate as-is. Callers with
/// degrade semantics (best-effort state files) treat any error as
/// skip-this-write, never as a hang.
pub fn lock_exclusive(lock_path: &Path, budget: Duration) -> io::Result<FileLock> {
    if let Some(parent) = lock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let start = Instant::now();
    loop {
        match try_once(lock_path) {
            Ok(lock) => return Ok(lock),
            Err(err) if err.kind() != io::ErrorKind::WouldBlock => return Err(err),
            Err(_) => {}
        }
        let elapsed = start.elapsed();
        if elapsed >= budget {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "timed out after {elapsed:?} (budget {budget:?}) waiting for lock: {}",
                    lock_path.display()
                ),
            ));
        }
        std::thread::sleep(RETRY_INTERVAL.min(budget - elapsed));
    }
}

/// The async flavor: the retry loop parks on the blocking pool, never on
/// the async runtime's worker threads.
pub async fn lock_exclusive_async(lock_path: &Path, budget: Duration) -> io::Result<FileLock> {
    let owned = lock_path.to_path_buf();
    tokio::task::spawn_blocking(move || lock_exclusive(&owned, budget))
        .await
        .map_err(|e| io::Error::other(format!("flock task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contention_surfaces_as_would_block_and_release_lets_the_next_taker_in() {
        let dir = tempfile::tempdir().unwrap();
        let guarded = dir.path().join("state.json");
        let lock_path = lock_path_for(&guarded);
        assert_eq!(lock_path.file_name().unwrap(), "state.json.lock");

        // A second fd standing in for another holder: flock contention does
        // not care which process owns the conflicting open file description.
        let holder = try_once(&lock_path).unwrap();
        assert!(matches!(
            lock_exclusive(&lock_path, Duration::ZERO)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        ));

        drop(holder);
        // Bounded, not ZERO: macOS can take a moment to propagate a flock
        // release after the closing fd's close() — an immediate re-flock
        // on a fresh fd may transiently see EWOULDBLOCK. The retry cadence
        // is the contract; a lock that stayed held would time out.
        let second = lock_exclusive(&lock_path, Duration::from_secs(1)).unwrap();
        drop(second);
    }

    #[test]
    fn bounded_wait_outlives_a_stable_holder_then_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = lock_path_for(&dir.path().join("state.json"));
        let _holder = try_once(&lock_path).unwrap();
        let started = Instant::now();
        let err = lock_exclusive(&lock_path, Duration::from_millis(60)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        // The budget is honored on the retry cadence, not exceeded wholesale.
        assert!(started.elapsed() >= Duration::from_millis(60));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn async_acquire_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = lock_path_for(&dir.path().join("state.json"));
        let _guard = lock_exclusive_async(&lock_path, Duration::ZERO)
            .await
            .unwrap();
        assert!(matches!(
            lock_exclusive_async(&lock_path, Duration::ZERO)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        ));
    }
}
