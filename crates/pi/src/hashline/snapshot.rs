//! In-memory snapshot store keyed by path with per-path version history.
//!
//! Each path retains up to `MAX_VERSIONS_PER_PATH` historical snapshots (head =
//! most recent); the store caps total tracked paths at `MAX_PATHS` via LRU
//! eviction, and caps total retained text across all paths at `MAX_TOTAL_BYTES`.
//! Snapshots are session-scoped: a read mints a tag, an edit validates against
//! it, and a successful edit records a fresh head snapshot so the next edit can
//! chain on the returned tag.
//!
//! Two distinct texts that collide on the 6-hex tag are retained as separate
//! versions so callers can still tell them apart via `Snapshot.text` — the tag
//! is only a fast index, never the identity.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use super::hash::compute_tag;

/// Maximum versions retained per path (LRU eviction drops the oldest).
const MAX_VERSIONS_PER_PATH: usize = 4;
/// Maximum distinct paths tracked before the least-recently-touched is evicted.
/// Wide sessions routinely touch far more than a few dozen files; evicting a
/// path downgrades a genuinely in-session tag to a misleading "not from this
/// session" rejection. Retention stays bounded by [`MAX_TOTAL_BYTES`].
const MAX_PATHS: usize = 256;
/// Global ceiling on retained snapshot text across all paths (UTF-8 bytes).
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;
/// Maximum distinct path-key directories kept under the disk root. Beyond
/// this, [`SnapshotStore::disk_gc`] drops the least-recently-modified dirs so
/// the cache cannot grow unbounded over the application's lifetime.
const MAX_DISK_PATHS: usize = 256;

/// Canonical store key for a path. Realpath resolves symlinks and the macOS
/// `/tmp` vs `/private/tmp` split, and collapses case-insensitive spellings onto
/// the on-disk form, so every spelling of one file fuses onto one snapshot
/// entry. Missing paths (new-file writes) fall back to the parent's realpath
/// plus the basename, then to the input unchanged.
fn canonical(path: &Path) -> PathBuf {
    if let Ok(c) = std::fs::canonicalize(path) {
        return c;
    }
    if let Some(parent) = path.parent()
        && let Ok(cp) = std::fs::canonicalize(parent)
        && let Some(name) = path.file_name()
    {
        return cp.join(name);
    }
    path.to_path_buf()
}

/// Public canonical store key (see [`canonical`]) for callers that need the
/// same path identity the snapshot store uses — e.g. the edit tool's noop
/// loop guard, which must not be bypassed by `/tmp` vs `/private/tmp`
/// spellings of one file.
pub fn canonical_path(path: &Path) -> PathBuf {
    canonical(path)
}

/// A recorded file snapshot: its normalized text, the tag derived from it,
/// and optional set of lines the read tool displayed.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub path: PathBuf,
    pub text: String,
    pub tag: String,
    pub recorded_at: i64,
    /// 1-indexed lines a producer (read/search) actually displayed under this
    /// tag. `None` means "no provenance recorded".
    pub seen_lines: Option<HashSet<usize>>,
}

/// Session-scoped store of file snapshots, keyed by path with per-path version
/// history. Interior mutability is supplied by the owner (a `Mutex` on
/// `tool::ToolState`), not by this type.
///
/// An optional disk tier ([`SnapshotStore::enable_disk`]) mirrors recorded
/// snapshot texts under a root directory so a rebuilt store (app restart,
/// session fork, worktree re-entry) can rehydrate a tag it never minted in
/// memory. Seen-line provenance is memory-only; a disk hit rehydrates without
/// it, which the seen-line gate treats as "no provenance" (apply as before).
#[derive(Debug, Default)]
pub struct SnapshotStore {
    by_path: HashMap<PathBuf, Vec<Snapshot>>,
    /// Insertion/recency order of paths; tail is most-recently-touched. Used
    /// for LRU eviction when the path count exceeds `MAX_PATHS` or total
    /// retained bytes exceed `MAX_TOTAL_BYTES`.
    path_order: Vec<PathBuf>,
    /// Sum of `text.len()` across all retained snapshots.
    total_bytes: usize,
    /// Optional disk-cache root. Snapshots mirror to `<root>/<pathkey>/<tag>`
    /// so a fresh store can rehydrate tags across process/harness rebuilds.
    disk: Option<PathBuf>,
}

impl SnapshotStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store whose recorded snapshots also persist under `root` and whose
    /// misses rehydrate from disk. `root` is created lazily on first write.
    pub fn with_disk(root: PathBuf) -> Self {
        SnapshotStore {
            by_path: HashMap::new(),
            path_order: Vec::new(),
            total_bytes: 0,
            disk: Some(root),
        }
    }

    /// Enable (or re-point) the disk tier on an existing store.
    pub fn enable_disk(&mut self, root: PathBuf) {
        self.disk = Some(root);
    }

    /// True when a disk tier is configured.
    pub fn has_disk(&self) -> bool {
        self.disk.is_some()
    }
    /// Record a snapshot for `path` from normalized `text`. Computes the tag,
    /// appends a new version (evicting the oldest if over the per-path cap),
    /// refreshes path recency, and returns the recorded snapshot.
    ///
    /// De-duplication checks both `hash` AND `text` equality: two distinct
    /// texts that collide on the 6-hex tag are different snapshots — fusing
    /// them under one entry would corrupt seen-lines.
    pub fn record(&mut self, path: &Path, text: &str) -> Snapshot {
        let path = canonical(path);
        let tag = compute_tag(text);
        let now = chrono::Utc::now().timestamp_millis();

        let versions = self.by_path.entry(path.to_path_buf()).or_default();

        // De-duplicate: same hash AND same text means identical content.
        if let Some(existing) = versions.iter_mut().find(|s| s.tag == tag && s.text == text) {
            existing.recorded_at = now;
            // Promote to head: remove and re-push.
            let snap = existing.clone();
            versions.retain(|s| s.tag != tag || s.text != text);
            versions.push(snap.clone());
            self.touch_path(&path);
            self.disk_write(&path, &snap);
            return snap;
        }

        let snap = Snapshot {
            path: path.clone(),
            text: text.to_string(),
            tag,
            recorded_at: now,
            seen_lines: None,
        };

        self.total_bytes += text.len();
        versions.push(snap.clone());
        if versions.len() > MAX_VERSIONS_PER_PATH {
            if let Some(evicted) = versions.first() {
                self.total_bytes = self.total_bytes.saturating_sub(evicted.text.len());
            }
            versions.remove(0);
        }

        self.touch_path(&path);
        self.evict_if_over_limit();
        self.disk_write(&path, &snap);
        snap
    }

    /// Look up a historical snapshot by `(path, tag)`. Memory first; on a miss
    /// with a disk tier configured, rehydrate from disk and retain it.
    pub fn get(&mut self, path: &Path, tag: &str) -> Option<&Snapshot> {
        let path = canonical(path);
        let in_memory = self
            .by_path
            .get(&path)
            .is_some_and(|versions| versions.iter().any(|s| s.tag == tag));
        if !in_memory {
            self.disk_hydrate(&path, tag)?;
        }
        self.by_path
            .get(&path)
            .and_then(|versions| versions.iter().find(|s| s.tag == tag))
    }

    /// The most recently recorded snapshot for `path`, if any.
    pub fn head(&self, path: &Path) -> Option<&Snapshot> {
        let path = canonical(path);
        self.by_path.get(&path).and_then(|v| v.last())
    }

    /// Mutable lookup of a historical snapshot by `(path, tag)`. Used to merge
    /// revealed lines into `seen_lines` after a gate rejection surfaced them.
    pub fn get_mut(&mut self, path: &Path, tag: &str) -> Option<&mut Snapshot> {
        let path = canonical(path);
        self.by_path
            .get_mut(&path)
            .and_then(|versions| versions.iter_mut().find(|s| s.tag == tag))
    }

    /// Look up a snapshot by `(path, full_text)` — exact text match. Does not
    /// refresh recency. Returns `None` when no version matches.
    pub fn by_content(&self, path: &Path, full_text: &str) -> Option<&Snapshot> {
        let path = canonical(path);
        self.by_path
            .get(&path)
            .and_then(|versions| versions.iter().find(|s| s.text == full_text))
    }

    /// Find every retained snapshot whose tag equals `hash`, across all tracked
    /// paths. Used for tag-based path recovery (model mistyped the path).
    pub fn find_by_hash(&self, hash: &str) -> Vec<&Snapshot> {
        let mut matches = Vec::new();
        for versions in self.by_path.values() {
            for version in versions {
                if version.tag == hash {
                    matches.push(version);
                }
            }
        }
        matches
    }

    /// Record which lines of a snapshot were displayed by a read tool. Merges
    /// into the existing `seen_lines` set. No-op when the tag is not retained.
    pub fn record_seen_lines(&mut self, path: &Path, tag: &str, lines: &HashSet<usize>) {
        let path = canonical(path);
        let Some(versions) = self.by_path.get_mut(&path) else {
            return;
        };
        let Some(snapshot) = versions.iter_mut().find(|s| s.tag == tag) else {
            return;
        };
        if let Some(existing) = &mut snapshot.seen_lines {
            existing.extend(lines);
        } else {
            snapshot.seen_lines = Some(lines.clone());
        }
    }

    /// Move retained version history (and read provenance) from `from` to `to`.
    /// No-op when `from` has no history. Used by file moves so tags minted from
    /// reads of the source path stay valid at the destination.
    pub fn relocate(&mut self, from: &Path, to: &Path) {
        let from = canonical(from);
        let to = canonical(to);
        let Some(source_versions) = self.by_path.remove(&from) else {
            return;
        };
        let relocated: Vec<Snapshot> = source_versions
            .into_iter()
            .map(|mut s| {
                s.path = to.clone();
                s
            })
            .collect();

        let dest_versions = self.by_path.entry(to.clone()).or_default();
        // Merge: keep existing versions, append relocated, de-dup by hash+text.
        let mut seen = HashSet::new();
        let mut merged: Vec<Snapshot> = Vec::new();
        for v in dest_versions.drain(..) {
            let key = (v.tag.clone(), v.text.clone());
            if seen.insert(key) {
                merged.push(v);
            }
        }
        for v in relocated {
            let key = (v.tag.clone(), v.text.clone());
            if seen.insert(key) {
                merged.push(v);
            }
        }
        // Keep only the newest MAX_VERSIONS_PER_PATH.
        if merged.len() > MAX_VERSIONS_PER_PATH {
            merged.sort_by_key(|a| a.recorded_at);
            let removed: Vec<_> = merged
                .drain(..merged.len() - MAX_VERSIONS_PER_PATH)
                .collect();
            for r in &removed {
                self.total_bytes = self.total_bytes.saturating_sub(r.text.len());
            }
        }
        // Re-sort by recorded_at ascending.
        merged.sort_by_key(|a| a.recorded_at);
        *dest_versions = merged;
        self.touch_path(&to);
    }

    /// Drop all version history for a single path.
    pub fn invalidate(&mut self, path: &Path) {
        let path = canonical(path);
        if let Some(versions) = self.by_path.remove(&path) {
            for v in &versions {
                self.total_bytes = self.total_bytes.saturating_sub(v.text.len());
            }
        }
        self.path_order.retain(|p| p != &path);
        self.disk_remove(&path);
    }

    /// Drop all version histories.
    pub fn clear(&mut self) {
        self.by_path.clear();
        self.path_order.clear();
        self.total_bytes = 0;
    }

    fn touch_path(&mut self, path: &Path) {
        self.path_order.retain(|p| p != path);
        self.path_order.push(path.to_path_buf());
        while self.path_order.len() > MAX_PATHS {
            let evicted = self.path_order.remove(0);
            self.invalidate(&evicted);
        }
    }

    /// Evict whole-path histories (LRU order) until total_bytes is under the
    /// global cap. Called after every insertion.
    fn evict_if_over_limit(&mut self) {
        while self.total_bytes > MAX_TOTAL_BYTES && !self.path_order.is_empty() {
            let evicted = self.path_order.remove(0);
            self.invalidate(&evicted);
        }
    }

    // ── Disk tier ─────────────────────────────────────────────────────────

    fn disk_dir(&self, path: &Path) -> Option<PathBuf> {
        self.disk.as_ref().map(|root| root.join(disk_key(path)))
    }

    /// Mirror a recorded snapshot to disk (atomic temp+rename), best-effort,
    /// then prune the path's mirrored versions to [`MAX_VERSIONS_PER_PATH`].
    fn disk_write(&self, path: &Path, snap: &Snapshot) {
        let Some(dir) = self.disk_dir(path) else {
            return;
        };
        let is_new_dir = !dir.exists();
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let tmp = dir.join(format!("{}.tmp", snap.tag));
        let file = dir.join(&snap.tag);
        if std::fs::write(&tmp, snap.text.as_bytes()).is_err() {
            return;
        }
        let _ = std::fs::rename(&tmp, &file);
        // Prune to the per-path cap, dropping the oldest mirrored versions.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut files: Vec<(PathBuf, std::time::SystemTime)> = entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .chars()
                        .all(|c| c.is_ascii_hexdigit())
                })
                .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
                .collect();
            if files.len() > MAX_VERSIONS_PER_PATH {
                files.sort_by_key(|(_, m)| *m);
                for (p, _) in files.iter().take(files.len() - MAX_VERSIONS_PER_PATH) {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        // A brand-new path-key dir may push the root past its cap; GC runs
        // only on new dirs so it is amortized, not once per write.
        if is_new_dir {
            self.disk_gc();
        }
    }

    /// Rehydrate a snapshot from disk on a memory miss and retain it in
    /// memory. Returns the hydrated snapshot, or `None`. Seen-line provenance
    /// is not persisted, so a hydrated snapshot carries none.
    fn disk_hydrate(&mut self, path: &Path, tag: &str) -> Option<Snapshot> {
        let dir = self.disk_dir(path)?;
        let text = std::fs::read_to_string(dir.join(tag)).ok()?;
        let normalized = super::normalize_to_lf(&text);
        if compute_tag(&normalized) != tag {
            return None;
        }
        let snap = Snapshot {
            path: path.to_path_buf(),
            text: normalized,
            tag: tag.to_string(),
            recorded_at: chrono::Utc::now().timestamp_millis(),
            seen_lines: None,
        };
        let versions = self.by_path.entry(path.to_path_buf()).or_default();
        versions.retain(|s| s.tag != tag);
        versions.push(snap.clone());
        // Keep the hydrated path within the per-path cap, mirroring record().
        if versions.len() > MAX_VERSIONS_PER_PATH {
            if let Some(evicted) = versions.first() {
                self.total_bytes = self.total_bytes.saturating_sub(evicted.text.len());
            }
            versions.remove(0);
        }
        self.total_bytes += snap.text.len();
        self.touch_path(path);
        Some(snap)
    }

    /// Remove a path's mirrored snapshots from disk.
    fn disk_remove(&self, path: &Path) {
        if let Some(dir) = self.disk_dir(path) {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Bound the number of path-key directories under the disk root, evicting the
    /// least-recently-modified ones past [`MAX_DISK_PATHS`]. Best-effort.
    fn disk_gc(&self) {
        let Some(root) = self.disk.as_ref() else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        let mut dirs: Vec<(PathBuf, std::time::SystemTime)> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?)))
            .collect();
        if dirs.len() <= MAX_DISK_PATHS {
            return;
        }
        dirs.sort_by_key(|(_, m)| *m);
        for (dir, _) in dirs.iter().take(dirs.len() - MAX_DISK_PATHS) {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Stable directory name for a canonical path: a 16-hex fingerprint so
/// arbitrary path characters never reach the filesystem layout. Uses FNV-1a
/// (not `DefaultHasher`), whose output is fixed across std/toolchain versions
/// — a persisted key must not silently orphan every snapshot on an upgrade.
fn disk_key(path: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}
#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn record_and_head() {
        let mut store = SnapshotStore::new();
        let snap = store.record(&p("a.rs"), "fn main() {}\n");
        assert_eq!(snap.tag, compute_tag("fn main() {}\n"));
        assert_eq!(store.head(&p("a.rs")).unwrap().tag, snap.tag);
    }

    #[test]
    fn get_finds_historical_tag() {
        let mut store = SnapshotStore::new();
        let v1 = store.record(&p("a.rs"), "a\n");
        let v2 = store.record(&p("a.rs"), "b\n");
        assert_ne!(v1.tag, v2.tag);
        assert_eq!(store.get(&p("a.rs"), &v1.tag).unwrap().text, "a\n");
        assert_eq!(store.get(&p("a.rs"), &v2.tag).unwrap().text, "b\n");
    }

    #[test]
    fn dedup_identical_reread() {
        let mut store = SnapshotStore::new();
        store.record(&p("a.rs"), "x\n");
        store.record(&p("a.rs"), "x\n");
        // Identical re-read must not grow version history.
        assert_eq!(store.by_path.get(&p("a.rs")).map(|v| v.len()), Some(1));
    }

    #[test]
    fn per_path_version_cap() {
        let mut store = SnapshotStore::new();
        for i in 0..(MAX_VERSIONS_PER_PATH + 2) {
            store.record(&p("a.rs"), &format!("v{i}\n"));
        }
        assert_eq!(
            store.by_path.get(&p("a.rs")).map(|v| v.len()).unwrap(),
            MAX_VERSIONS_PER_PATH
        );
    }

    #[test]
    fn global_path_cap_evicts_lru() {
        let mut store = SnapshotStore::new();
        for i in 0..(MAX_PATHS + 2) {
            store.record(&p(&format!("file{i}.rs")), "x\n");
        }
        assert!(store.by_path.len() <= MAX_PATHS);
        // The earliest-recorded paths should have been evicted.
        assert!(store.head(&p("file0.rs")).is_none());
    }

    #[test]
    fn by_content_finds_exact_text() {
        let mut store = SnapshotStore::new();
        store.record(&p("a.rs"), "hello\n");
        let found = store.by_content(&p("a.rs"), "hello\n");
        assert!(found.is_some());
        assert_eq!(found.unwrap().text, "hello\n");
        // Different text should not match.
        assert!(store.by_content(&p("a.rs"), "bye\n").is_none());
    }

    #[test]
    fn find_by_hash_across_paths() {
        let mut store = SnapshotStore::new();
        let s1 = store.record(&p("a.rs"), "x\n");
        let s2 = store.record(&p("b.rs"), "x\n");
        // Same content → same tag.
        assert_eq!(s1.tag, s2.tag);
        let matches = store.find_by_hash(&s1.tag);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn record_seen_lines_merges() {
        let mut store = SnapshotStore::new();
        let snap = store.record(&p("a.rs"), "a\nb\nc\n");
        assert!(snap.seen_lines.is_none());
        let mut lines = HashSet::new();
        lines.insert(1);
        store.record_seen_lines(&p("a.rs"), &snap.tag, &lines);
        let head = store.head(&p("a.rs")).unwrap();
        assert_eq!(head.seen_lines.as_ref().unwrap().len(), 1);
        // Merge more.
        let mut lines2 = HashSet::new();
        lines2.insert(3);
        store.record_seen_lines(&p("a.rs"), &snap.tag, &lines2);
        let head = store.head(&p("a.rs")).unwrap();
        assert_eq!(head.seen_lines.as_ref().unwrap().len(), 2);
    }

    #[test]
    fn relocate_transfers_history() {
        let mut store = SnapshotStore::new();
        let s1 = store.record(&p("old.rs"), "a\n");
        store.record(&p("old.rs"), "b\n");
        store.relocate(&p("old.rs"), &p("new.rs"));
        assert!(store.head(&p("old.rs")).is_none());
        assert!(store.head(&p("new.rs")).is_some());
        // The tag from old should still be findable at new.
        assert!(store.get(&p("new.rs"), &s1.tag).is_some());
    }

    #[test]
    fn total_bytes_tracked() {
        let mut store = SnapshotStore::new();
        store.record(&p("a.rs"), "hello\n");
        assert!(store.total_bytes > 0);
        store.invalidate(&p("a.rs"));
        assert_eq!(store.total_bytes, 0);
    }

    #[test]
    fn disk_tier_rehydrates_across_stores() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let disk = dir.path().join("snapshots");

        let mut store = SnapshotStore::with_disk(disk.clone());
        let snap = store.record(&file, "fn main() {}\n");

        // A brand-new store over the same disk root rehydrates the tag the
        // first store minted, even though its memory is empty.
        let mut fresh = SnapshotStore::with_disk(disk);
        let hydrated = fresh.get(&file, &snap.tag).expect("disk rehydrate");
        assert_eq!(hydrated.text, "fn main() {}\n");
        assert_eq!(hydrated.tag, snap.tag);
        // No provenance survives the disk round-trip.
        assert!(hydrated.seen_lines.is_none());
    }

    #[test]
    fn disk_tier_miss_without_match_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let disk = dir.path().join("snapshots");

        let mut store = SnapshotStore::with_disk(disk);
        assert!(store.get(&file, "DEAD00").is_none());
    }

    #[test]
    fn disk_tier_invalidate_removes_mirror() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let disk = dir.path().join("snapshots");

        let mut store = SnapshotStore::with_disk(disk.clone());
        let snap = store.record(&file, "fn main() {}\n");
        store.invalidate(&file);

        // After invalidation a fresh store can no longer rehydrate the tag.
        let mut fresh = SnapshotStore::with_disk(disk);
        assert!(fresh.get(&file, &snap.tag).is_none());
    }

    #[test]
    fn memory_only_store_never_touches_disk() {
        let mut store = SnapshotStore::new();
        assert!(!store.has_disk());
        let snap = store.record(&p("a.rs"), "x\n");
        assert_eq!(store.get(&p("a.rs"), &snap.tag).unwrap().tag, snap.tag);
    }

    #[test]
    fn disk_key_is_stable_and_version_independent() {
        // FNV-1a over the same path always yields the same 16-hex key, so a
        // toolchain upgrade cannot orphan persisted snapshot dirs.
        let key = disk_key(std::path::Path::new("/tmp/a.rs"));
        assert_eq!(key, disk_key(std::path::Path::new("/tmp/a.rs")));
        assert_eq!(key.len(), 16);
        assert!(key.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(key, disk_key(std::path::Path::new("/tmp/b.rs")));
    }

    #[test]
    fn disk_gc_caps_pathkey_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("snapshots");
        // Record snapshots for more paths than the disk cap, forcing GC.
        let mut store = SnapshotStore::with_disk(disk.clone());
        for i in 0..(MAX_DISK_PATHS + 8) {
            let file = dir.path().join(format!("f{i}.rs"));
            std::fs::write(&file, format!("v{i}\n")).unwrap();
            store.record(&file, &format!("v{i}\n"));
        }
        let count = std::fs::read_dir(&disk)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .count();
        assert!(
            count <= MAX_DISK_PATHS,
            "disk GC must cap path dirs, found {count}"
        );
    }
}
