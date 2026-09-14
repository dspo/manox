// Append-only JSONL session storage (format version 4; a v3 file loads
// and lazily migrates on its next append).
//
// Layout of a session file (the caller picks the path — typically a
// `timestamp_sessionId.jsonl` under a per-cwd directory, matching the TS Pi
// repo naming):
//   line 0 — a session header: `{"type":"session","version":4,"id":..,"timestamp":..,"cwd":..,"parentSession"?:..,"metadata"?:..}`
//              (a v3 header, `"version":3`, loads and migrates).
//   line 1.. — session-tree entries, appended in occurrence order. A `leaf`
//              entry records a cursor move to an older branch point
//              (`targetId`); any other entry implicitly makes itself the
//              cursor. The leaf cursor is `targetId` for a trailing leaf
//              entry, otherwise the last entry's id. Appends are strictly
//              additive — no field of an existing line is ever rewritten;
//              the two whole-file replaces are the v3→v4 lazy migration and
//              the inversion heal, both laying the chain-dense order down
//              through the atomic temp+rename below.
//
// Two storage instances over one file (the cold-append path, embedder
// processes) each hold a private append lock — nothing serializes them
// against each other but the write fence below. The fence (an exclusive
// `flock` plus the stale-view reload in `append_entry_locked`) serializes
// and linearizes those instances, and the loader tolerates the damage they
// already produced: an entry line may sit before its parent as long as the
// chain is globally intact — every parent exists somewhere in the file, no
// cycle, and a v4 `seq` equals the chain depth. Memory is ordered by the
// chain and the next append rewrites the file in chain order (the lazy
// heal), so line order alone can never make a session unreadable.
//
// `open` takes the exact file path. A missing file is created with `metadata`
// as its header; an existing file must begin with a valid v3 or v4 session
// header, otherwise this errors rather than guessing at a repair.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Mutex, broadcast};

use crate::session::{SessionStorage, SessionTreeEntry};
use serde_json::Value as JsonValue;

/// Current on-disk session format version (v4: chain-dense `seq` on every
/// entry, the journal of architecture doc §C).
const FORMAT_VERSION: u32 = 4;

/// The previous format. v3 files still open (seq is backfilled from chain
/// depth in memory) and are rewritten in full as v4 under the append lock on
/// the first append — the lazy §C.1 migration.
const LEGACY_FORMAT_VERSION: u32 = 3;

/// Broadcast capacity for [`JournalEvent`] subscribers. A lagging subscriber
/// gets [`broadcast::error::RecvError::Lagged`] and must resynchronize from a
/// fresh chain read (the L5 companion rule; the session-core pump treats lag
/// as a follow-stream resync, never as silent data loss).
const JOURNAL_BROADCAST_CAPACITY: usize = 4096;

/// One journal event as broadcast by [`JsonlSessionStorage`] at every append,
/// in strict seq order (sent under the append lock).
#[derive(Debug, Clone)]
pub struct JournalEvent {
    /// Chain depth of the appended entry (dense 0-based along its chain).
    pub seq: u64,
    pub entry: std::sync::Arc<SessionTreeEntry>,
}

/// One record of a chain read ([`JsonlSessionStorage::journal_range`]).
#[derive(Debug, Clone)]
pub struct JournalRecord {
    pub seq: u64,
    pub entry: SessionTreeEntry,
}

/// Session metadata written once as the file header and read back on reopen.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonlSessionMetadata {
    pub id: String,
    pub cwd: String,
    #[serde(default = "chrono::Utc::now")]
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Path of the session this one forked from, if any. `PathBuf` up to
    /// the JSON boundary (review #775): serialization of a non-UTF8 path
    /// errors LOUDLY here instead of being silently lossy-mangled into
    /// the header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_path: Option<PathBuf>,
    /// Free-form metadata carried in the header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<JsonValue>,
}

/// The first line of a v3 session file. Field names are camelCase to match the
/// TS Pi header schema (multi-word fields like `parentSession` would otherwise
/// leak snake_case onto disk).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionHeader {
    /// Discriminator fixed to `"session"`.
    #[serde(rename = "type")]
    type_tag: String,
    version: u32,
    id: String,
    timestamp: chrono::DateTime<chrono::Utc>,
    cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent_session: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<JsonValue>,
}

/// JSONL session storage backed by a single append-only file.
pub struct JsonlSessionStorage {
    jsonl_path: PathBuf,
    /// All entries after the header, cached in memory.
    entries: Mutex<Vec<SessionTreeEntry>>,
    /// Current leaf cursor. For a `leaf` entry this is its `targetId`;
    /// otherwise it is the last appended entry's id.
    leaf_id: Mutex<Option<String>>,
    /// entry id → chain depth (§C.1 seq). Assigned at the single append point
    /// under `append_lock` (L4); on load the depths come from a memoized
    /// parent walk — order-independent, so a recoverable line-order inversion
    /// changes nothing here.
    seq_index: Mutex<std::collections::HashMap<String, u64>>,
    /// The on-disk format version of the opened file (3 until the first
    /// append rewrites it to 4).
    file_version: Mutex<u32>,
    /// Ordered journal append notifications (one per successful append, in
    /// seq order, sent under the append lock).
    journal_tx: broadcast::Sender<JournalEvent>,
    /// Serializes the write → index → cursor sequence so concurrent appends
    /// never interleave the three steps and diverge disk order from the
    /// in-memory index or the cursor.
    append_lock: Mutex<()>,
    /// Metadata read from the header (file is authoritative on reopen).
    pub metadata: JsonlSessionMetadata,
    /// The file has not been written yet: the header and buffered entries
    /// live in memory and the file materializes on the first assistant
    /// message, matching the TS deferred-first-assistant contract. Until
    /// then the session is invisible to `list` and `open`.
    deferred: Mutex<bool>,
    /// The file's line order diverges from the chain order — a recoverable
    /// write-order inversion (two storage instances racing one file before
    /// the fence existed). Set at load, cleared by the next append, which
    /// rewrites the file in chain order (the lazy heal).
    needs_rewrite: Mutex<bool>,
    /// The file's byte length as this instance last saw it on disk. An
    /// append under the write fence compares it against the live size: a
    /// mismatch means another instance wrote since, and the in-memory view
    /// reloads before this append parents onto a stale tail.
    disk_size: Mutex<u64>,
}

impl JsonlSessionStorage {
    /// The session file path.
    pub fn path(&self) -> &Path {
        &self.jsonl_path
    }
}

impl JsonlSessionStorage {
    /// Create a new session file at `path`, writing `metadata` as the header.
    ///
    /// The path is the exact file location — the caller owns the naming scheme
    /// (the TS Pi repo writes `timestamp_sessionId.jsonl` under a per-cwd
    /// directory). A missing parent directory is created. This errors if the
    /// file already exists; reopen an existing file with [`Self::open`].
    pub async fn create(
        path: &Path,
        metadata: JsonlSessionMetadata,
    ) -> Result<Self, anyhow::Error> {
        if path.exists() {
            anyhow::bail!("session file already exists: {}", path.display());
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }

        let header = SessionHeader {
            type_tag: "session".into(),
            version: FORMAT_VERSION,
            id: metadata.id.clone(),
            timestamp: metadata.created_at,
            cwd: metadata.cwd.clone(),
            parent_session: metadata.parent_session_path.clone(),
            metadata: metadata.metadata.clone(),
        };
        // create must never write a header its own `open` would reject — the
        // same wire validator guards both paths.
        validate_header_wire(&serde_json::to_value(&header).expect("header serializes"))?;
        let line = serde_json::to_string(&header)? + "\n";
        let header_len = line.len() as u64;
        tokio::fs::write(path, line).await?;
        let (journal_tx, _) = broadcast::channel(JOURNAL_BROADCAST_CAPACITY);
        Ok(JsonlSessionStorage {
            jsonl_path: path.to_path_buf(),
            entries: Mutex::new(Vec::new()),
            leaf_id: Mutex::new(None),
            seq_index: Mutex::new(std::collections::HashMap::new()),
            file_version: Mutex::new(FORMAT_VERSION),
            journal_tx,
            append_lock: Mutex::new(()),
            metadata,
            deferred: Mutex::new(false),
            needs_rewrite: Mutex::new(false),
            disk_size: Mutex::new(header_len),
        })
    }

    /// Create a session whose file materializes on the first assistant
    /// message — the TS deferred-first-assistant contract for new and
    /// branched sessions. The header is validated (so a later materialization
    /// never writes a file its own `open` would reject) but not written;
    /// appends buffer in memory until an assistant message arrives, at which
    /// point the header and every buffered entry are written in order. An
    /// empty session never touches disk and therefore never appears in
    /// [`crate::session::repository::SessionRepository::list`].
    pub async fn create_deferred(
        path: &Path,
        metadata: JsonlSessionMetadata,
    ) -> Result<Self, anyhow::Error> {
        if path.exists() {
            anyhow::bail!("session file already exists: {}", path.display());
        }
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            tokio::fs::create_dir_all(parent).await?;
        }
        let header = SessionHeader {
            type_tag: "session".into(),
            version: FORMAT_VERSION,
            id: metadata.id.clone(),
            timestamp: metadata.created_at,
            cwd: metadata.cwd.clone(),
            parent_session: metadata.parent_session_path.clone(),
            metadata: metadata.metadata.clone(),
        };
        validate_header_wire(&serde_json::to_value(&header).expect("header serializes"))?;
        let (journal_tx, _) = broadcast::channel(JOURNAL_BROADCAST_CAPACITY);
        Ok(JsonlSessionStorage {
            jsonl_path: path.to_path_buf(),
            entries: Mutex::new(Vec::new()),
            leaf_id: Mutex::new(None),
            seq_index: Mutex::new(std::collections::HashMap::new()),
            file_version: Mutex::new(FORMAT_VERSION),
            journal_tx,
            append_lock: Mutex::new(()),
            metadata,
            deferred: Mutex::new(true),
            needs_rewrite: Mutex::new(false),
            disk_size: Mutex::new(0),
        })
    }

    /// Open an existing session file at `path`.
    ///
    /// The file must exist and begin with a valid v3 or v4 session header;
    /// this errors rather than guessing at a repair. Unlike [`Self::create`], a
    /// missing or mis-typed path surfaces as an error so a recovery path can
    /// never silently materialize an empty session.
    pub async fn open(path: &Path) -> Result<Self, anyhow::Error> {
        Self::load(path).await
    }

    async fn load(path: &Path) -> Result<Self, anyhow::Error> {
        // Read fence: a shared `flock` covering ONLY the byte read — the
        // snapshot is stable once the bytes are in memory, and a concurrent
        // append's exclusive fence waits no longer than that. The parse
        // (CPU-bound; a 57k-entry file spends real time here) runs outside
        // the lock so it never parks other instances' appends.
        let bytes = {
            let mut file = File::open(path).await?;
            flock_shared(&file).await?;
            let mut bytes = Vec::new();
            bytes.reserve(file.metadata().await?.len() as usize);
            file.read_to_end(&mut bytes).await?;
            bytes
        };
        let parsed = parse_file(path, &bytes)?;
        let (journal_tx, _) = broadcast::channel(JOURNAL_BROADCAST_CAPACITY);

        Ok(JsonlSessionStorage {
            jsonl_path: path.to_path_buf(),
            entries: Mutex::new(parsed.entries),
            leaf_id: Mutex::new(parsed.leaf_id),
            seq_index: Mutex::new(parsed.seq_index),
            file_version: Mutex::new(parsed.header_version),
            journal_tx,
            append_lock: Mutex::new(()),
            metadata: parsed.metadata,
            deferred: Mutex::new(false),
            needs_rewrite: Mutex::new(parsed.needs_rewrite),
            disk_size: Mutex::new(bytes.len() as u64),
        })
    }

    /// Re-read the whole file through the fenced fd and replace this
    /// instance's in-memory view (entries, depths, cursor, heal flag, disk
    /// size). Called only with the write fence held and only when the file
    /// grew since this view was built — another instance appended. Journal
    /// followers of THIS instance may now lag behind the reloaded view; the
    /// L5 rule covers that: a lagging follower resynchronizes via a fresh
    /// chain read.
    async fn reload_under_fence(&self, fence: &mut WriteFence) -> Result<(), anyhow::Error> {
        fence.file.seek(std::io::SeekFrom::Start(0)).await?;
        let mut bytes = Vec::new();
        fence.file.read_to_end(&mut bytes).await?;
        let parsed = parse_file(&self.jsonl_path, &bytes)?;
        *self.entries.lock().await = parsed.entries;
        *self.seq_index.lock().await = parsed.seq_index;
        *self.leaf_id.lock().await = parsed.leaf_id;
        *self.file_version.lock().await = parsed.header_version;
        *self.needs_rewrite.lock().await = parsed.needs_rewrite;
        *self.disk_size.lock().await = bytes.len() as u64;
        Ok(())
    }

    /// Bulk-append a chain of entries under ONE lock hold with ONE disk
    /// write (the fork-copy path). Ids are checked against the existing
    /// set and the batch itself in a single pass (O(n + m) instead of the
    /// per-row O(n·m) of repeated `append_entry`), seqs are derived from
    /// the existing index plus the batch's own parents, and the WHOLE
    /// batch is validated and serialized before anything touches disk —
    /// an invalid entry anywhere (empty/duplicate id, unknown parent,
    /// unserializable entry) rejects the entire batch with no partial
    /// prefix written.
    ///
    /// Constraints (the fork path always satisfies them): the file must
    /// be materialized and on the current format version — a deferred or
    /// v3 file is rejected rather than silently materialized/migrated
    /// mid-batch; route those through [`Self::append_entry`]. Index,
    /// cursor, and journal-broadcast updates mirror `append_entry` per
    /// entry, in batch order, under the same lock.
    pub async fn append_entries(&self, entries: &[SessionTreeEntry]) -> Result<(), anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        if *self.deferred.lock().await {
            anyhow::bail!("append_entries requires a materialized session (file is deferred)");
        }
        if *self.file_version.lock().await < FORMAT_VERSION {
            anyhow::bail!("append_entries requires a v4 session file (append once to migrate)");
        }
        let Some(last) = entries.last() else {
            return Ok(());
        };
        // The write fence (B3), same contract as `append_entry_locked`: the
        // batch's phase-1 validation must run against on-disk truth, so a
        // file that grew since this view was built reloads first —
        // otherwise the batch's seqs and parents derive from a stale tail
        // and fork the chain.
        let mut fence = WriteFence::acquire(&self.jsonl_path).await?;
        if let Some(fenced) = fence.as_mut()
            && fenced.file.metadata().await?.len() != *self.disk_size.lock().await
        {
            self.reload_under_fence(fenced).await?;
            if *self.deferred.lock().await {
                anyhow::bail!("append_entries requires a materialized session (file is deferred)");
            }
            if *self.file_version.lock().await < FORMAT_VERSION {
                anyhow::bail!("append_entries requires a v4 session file (append once to migrate)");
            }
        }
        if *self.needs_rewrite.lock().await {
            // A line-order-inverted file heals through the per-entry path
            // (each append may take the whole-file rewrite); the batch's
            // single multi-line append cannot lay the healed order down.
            // Drop the fence FIRST: `append_entry_locked` acquires its own,
            // and flock counts per open file description — a second fd's
            // blocking LOCK_EX out of this process would wait forever on
            // the one still held here (self-deadlock).
            drop(fence);
            for entry in entries {
                self.append_entry_locked(entry, false).await?;
            }
            return Ok(());
        }
        // Phase 1 (no disk): one existing-id set + one depth map; validate
        // and serialize the batch.
        let existing_ids: HashSet<String> = {
            let es = self.entries.lock().await;
            es.iter().map(|e| e.id().to_string()).collect()
        };
        let mut depths: HashMap<String, u64> = self.seq_index.lock().await.clone();
        let mut batch_ids: HashSet<String> = HashSet::new();
        let mut lines = String::new();
        let mut seqs = Vec::with_capacity(entries.len());
        for entry in entries {
            let id = entry.id();
            if id.is_empty() {
                anyhow::bail!("refusing entry with empty id");
            }
            if existing_ids.contains(id) || batch_ids.contains(id) {
                anyhow::bail!("duplicate entry id {id}");
            }
            let seq = match entry.parent_id() {
                None => 0,
                Some(parent) => match depths.get(parent) {
                    Some(depth) => depth + 1,
                    None => anyhow::bail!(
                        "entry {id} references unknown parent {parent}: chain is broken"
                    ),
                },
            };
            lines.push_str(&v4_line(entry, seq)?);
            batch_ids.insert(id.to_string());
            depths.insert(id.to_string(), seq);
            seqs.push(seq);
        }
        // Phase 2: ONE fenced write; then index/cursor/broadcast under the
        // same lock hold, mirroring append_entry_locked's post-write order.
        // A fence-less non-deferred append means the file vanished beneath
        // a live session (external deletion): recover by whole-file rewrite
        // — header + history + batch — exactly the single-entry path's
        // degradation, never a panic and never a headerless zombie.
        match fence.as_mut() {
            Some(fenced) => {
                fenced.file.write_all(lines.as_bytes()).await?;
                *self.disk_size.lock().await += lines.len() as u64;
            }
            None => {
                let written = self.rewrite_file_v4_locked(Some(&lines)).await?;
                *self.needs_rewrite.lock().await = false;
                *self.disk_size.lock().await = written;
            }
        }
        {
            let mut es = self.entries.lock().await;
            let mut index = self.seq_index.lock().await;
            for (entry, seq) in entries.iter().zip(&seqs) {
                es.push(entry.clone());
                index.insert(entry.id().to_string(), *seq);
            }
        }
        *self.leaf_id.lock().await = last.leaf_cursor_after();
        for (entry, seq) in entries.iter().zip(&seqs) {
            let _ = self.journal_tx.send(JournalEvent {
                seq: *seq,
                entry: std::sync::Arc::new(entry.clone()),
            });
        }
        Ok(())
    }

    /// The write → index → cursor sequence, atomic under
    /// [`Self::append_lock`]. Trait methods take the lock and delegate here.
    /// A duplicate or empty id is refused before anything touches disk — the
    /// walk index would otherwise silently overwrite one entry with another.
    ///
    /// v4 (§C.1/L4): the seq — the new entry's chain depth — is assigned
    /// here and nowhere else, stamped into the line, recorded in
    /// `seq_index`, and broadcast in seq order while the lock is held.
    /// Appending to a file still on v3 rewrites it in full as v4 first (the
    /// lazy migration; the rewrite reuses the buffered-seq values computed
    /// at load).
    ///
    /// `force_materialize` writes the entry — and every buffered row before
    /// it — to disk even while the session is still deferred (the K5
    /// acceptance-time append: a persisted Submit must survive a crash that
    /// happens before any assistant message materializes the file).
    async fn append_entry_locked(
        &self,
        entry: &SessionTreeEntry,
        force_materialize: bool,
    ) -> Result<(), anyhow::Error> {
        if entry.id().is_empty() {
            anyhow::bail!("refusing entry with empty id");
        }
        let exists = self
            .entries
            .lock()
            .await
            .iter()
            .any(|e| e.id() == entry.id());
        if exists {
            anyhow::bail!("duplicate entry id {}", entry.id());
        }
        // Cross-instance fence (B3): the per-instance lock held by the caller
        // cannot see a SECOND storage object over the same file — the
        // cold-append path and embedder processes open their own instances,
        // and two private locks interleaving writes is how a child line once
        // landed before its parent. The fence serializes this whole critical
        // section at the OS level, and the stale-view guard below reloads
        // on-disk truth when another instance appended since this view was
        // built, so parents chain onto the CURRENT tail instead of forking a
        // sibling branch from a stale one.
        let mut fence = WriteFence::acquire(&self.jsonl_path).await?;
        let mut entry = entry.clone();
        if let Some(fence) = fence.as_mut()
            && fence.file.metadata().await?.len() != *self.disk_size.lock().await
        {
            // The file moved since this view was built. Reload, then repair
            // the caller's parent choice: an append onto what WAS the leaf
            // re-points onto the CURRENT leaf. An append onto an explicit
            // branch point (an older entry, never the leaf) keeps its
            // parent — branch intent survives the recovery.
            let pre_reload_leaf = self.leaf_id.lock().await.clone();
            self.reload_under_fence(fence).await?;
            let current_leaf = self.leaf_id.lock().await.clone();
            if entry.parent_id() == pre_reload_leaf.as_deref()
                && entry.parent_id() != current_leaf.as_deref()
            {
                entry = reparent_entry(&entry, current_leaf.as_deref())?;
            }
            // The reloaded file may already hold this id (another instance
            // persisted the same row): refuse before anything touches disk.
            if self
                .entries
                .lock()
                .await
                .iter()
                .any(|e| e.id() == entry.id())
            {
                anyhow::bail!("duplicate entry id {}", entry.id());
            }
        }
        // Single stamp point: the parent must already be indexed — in one
        // instance's view by construction, across instances by the fence +
        // reload above.
        let seq = match entry.parent_id() {
            None => 0u64,
            Some(parent) => {
                let seq_index = self.seq_index.lock().await;
                match seq_index.get(parent) {
                    Some(depth) => depth + 1,
                    None => anyhow::bail!(
                        "entry {} references unknown parent {parent}: chain is broken",
                        entry.id()
                    ),
                }
            }
        };
        let line = v4_line(&entry, seq)?;
        // A deferred session materializes on the first assistant message: the
        // header plus every buffered entry are written in one shot, so the
        // on-disk order matches the in-memory index (TS `_persist`). Before
        // that boundary the row lives ONLY in memory — writing it straight to
        // disk (the pre-fix bug) produced headerless zombie files on every
        // boot / new-session click (two default rows, no `session` header),
        // invisible to the scan forever.
        let is_assistant = matches!(
            entry,
            SessionTreeEntry::Message {
                message: crate::types::AgentMessage::Assistant { .. },
                ..
            }
        );
        if *self.deferred.lock().await {
            // A deferred session materializes on the first assistant message
            // (TS `_persist`) — or on ANY entry the caller marks durable (K5:
            // a session carrying an accepted Submit has interacted, so it is
            // no zombie; the accepted text must be on disk before the
            // receipt's crash window opens).
            if is_assistant || force_materialize {
                let written = self.rewrite_file_v4_locked(Some(&line)).await?;
                *self.deferred.lock().await = false;
                *self.disk_size.lock().await = written;
            }
        } else if *self.file_version.lock().await < FORMAT_VERSION
            || *self.needs_rewrite.lock().await
            || fence.is_none()
        {
            // Lazy v3 → v4 migration, the inversion heal, or an externally
            // deleted file: one atomic whole-file replace writes the header
            // and the in-memory chain (plus this line) — only the layout
            // changes, never the history.
            let written = self.rewrite_file_v4_locked(Some(&line)).await?;
            *self.needs_rewrite.lock().await = false;
            *self.disk_size.lock().await = written;
        } else {
            let fenced = fence
                .as_mut()
                .expect("a fence-less non-deferred append takes the rewrite arm");
            fenced.file.write_all(line.as_bytes()).await?;
            *self.disk_size.lock().await += line.len() as u64;
        }
        // A buffered (pre-materialization) row needs no disk write: the index
        // below carries it until the flush rewrites the file wholesale.
        // Index the entry before moving the cursor, mirroring TS Pi's order:
        // a concurrent `get_leaf_id` must never see a cursor whose target is
        // absent from the index, which would read as session corruption.
        self.entries.lock().await.push(entry.clone());
        self.seq_index
            .lock()
            .await
            .insert(entry.id().to_string(), seq);
        // The cursor follows this entry: a `leaf` entry redirects to its
        // `targetId`, otherwise the entry becomes the cursor itself.
        *self.leaf_id.lock().await = entry.leaf_cursor_after();
        // Ordered notification: sent under the append lock so subscribers
        // observe strictly increasing seq. No subscribers is fine.
        let _ = self.journal_tx.send(JournalEvent {
            seq,
            entry: std::sync::Arc::new(entry.clone()),
        });
        Ok(())
    }

    /// Rewrite the whole file as v4 (header + every indexed entry with its
    /// stamped seq, optionally plus one more line) in a single write. Used by
    /// deferred materialization and the lazy v3 migration, both under
    /// `append_lock`.
    async fn rewrite_file_v4_locked(&self, extra_line: Option<&str>) -> Result<u64, anyhow::Error> {
        let header = SessionHeader {
            type_tag: "session".into(),
            version: FORMAT_VERSION,
            id: self.metadata.id.clone(),
            timestamp: self.metadata.created_at,
            cwd: self.metadata.cwd.clone(),
            parent_session: self.metadata.parent_session_path.clone(),
            metadata: self.metadata.metadata.clone(),
        };
        let mut content = serde_json::to_string(&header)? + "\n";
        {
            let entries = self.entries.lock().await;
            let seq_index = self.seq_index.lock().await;
            for entry in entries.iter() {
                let seq = seq_index.get(entry.id()).copied().ok_or_else(|| {
                    anyhow::anyhow!("entry {} missing from seq index", entry.id())
                })?;
                content.push_str(&v4_line(entry, seq)?);
            }
        }
        if let Some(extra) = extra_line {
            content.push_str(extra);
        }
        // K7 (§C durability): the whole-file rewrite is an atomic replace —
        // write a sibling temp, fsync it, then rename over the target. The
        // former truncate+write left a crash (or any concurrent reader, e.g.
        // the sidebar scan or an ecosystem tool reading the journal) staring
        // at a half-written or empty session file; with rename, readers see
        // either the complete old file or the complete new one, never a
        // truncation in between. `append_lock` serializes rewrites WITHIN
        // one instance only — two instances over one file (the B3 cold
        // append race) each hold their own — so the temp name itself is
        // unique per rewrite (pid + process-local counter): a concurrent
        // writer can never truncate this rewrite's artifact mid-flight.
        // The `.tmp` suffix keeps session-directory scans
        // (extension == "jsonl") off the artifact.
        static REWRITE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let tmp = self.jsonl_path.with_extension(format!(
            "jsonl.{}.{}.tmp",
            std::process::id(),
            REWRITE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut file = File::create(&tmp).await?;
        file.write_all(content.as_bytes()).await?;
        file.sync_all().await?;
        tokio::fs::rename(&tmp, &self.jsonl_path).await?;
        // Best-effort: fsync the containing directory so the rename itself
        // survives a crash. The data is already consistent without it (the
        // rename happened); a directory that cannot be opened or synced only
        // loses that metadata-flush guarantee, never file contents.
        #[cfg(unix)]
        if let Some(dir) = self.jsonl_path.parent()
            && let Ok(dir_file) = std::fs::File::open(dir)
        {
            let _ = dir_file.sync_all();
        }
        *self.file_version.lock().await = FORMAT_VERSION;
        Ok(content.len() as u64)
    }

    // ── v4 journal read face (§C.3) ────────────────────────────────────────

    /// Subscribe to ordered journal appends. A lagging receiver sees
    /// [`broadcast::error::RecvError::Lagged`] and must resynchronize via a
    /// fresh chain read (L5 companion rule).
    pub fn subscribe_journal(&self) -> broadcast::Receiver<JournalEvent> {
        self.journal_tx.subscribe()
    }

    /// The seq of the active leaf (chain length − 1; 0 for an empty
    /// journal). Dense along the active chain by construction.
    pub async fn journal_cursor(&self) -> u64 {
        let leaf_id = self.leaf_id.lock().await.clone();
        match leaf_id {
            None => 0,
            Some(id) => {
                let seq = self.seq_index.lock().await.get(&id).copied();
                // A leaf the index does not know is an internal
                // inconsistency (load builds both together, appends keep
                // them in step); 0 keeps the old fallback but debug
                // builds fail loud instead of masking it (round 1 P2-13).
                debug_assert!(
                    seq.is_some(),
                    "journal_cursor: leaf {id} missing from the seq index"
                );
                seq.unwrap_or(0)
            }
        }
    }

    /// Read a seq range off the active chain (inclusive bounds, clamped).
    /// The active chain is dense 0-based, so a chain position *is* its seq.
    pub async fn journal_range(
        &self,
        from_seq: u64,
        to_seq: u64,
    ) -> Result<Vec<JournalRecord>, anyhow::Error> {
        let entries = self.entries.lock().await;
        let leaf_id = self.leaf_id.lock().await.clone();
        let target_id = match &leaf_id {
            Some(id) if entries.iter().any(|e| e.id() == id) => id.clone(),
            // An empty journal (or a cursor pointing at a since-removed
            // entry — corruption get_leaf_id already rejects) yields no
            // records; `None` cursor means empty.
            _ => return Ok(Vec::new()),
        };
        let mut index: std::collections::HashMap<&str, &SessionTreeEntry> =
            entries.iter().map(|e| (e.id(), e)).collect();
        let mut chain: Vec<&SessionTreeEntry> = Vec::new();
        let mut current_id: Option<&str> = Some(&target_id);
        while let Some(id) = current_id {
            let entry = match index.remove(id) {
                Some(e) => e,
                None => anyhow::bail!("entry {id} not found: session chain is broken"),
            };
            current_id = entry.parent_id();
            chain.push(entry);
        }
        chain.reverse();
        Ok(chain
            .into_iter()
            .enumerate()
            .map(|(position, entry)| JournalRecord {
                seq: position as u64,
                entry: entry.clone(),
            })
            .filter(|record| record.seq >= from_seq && record.seq <= to_seq)
            .collect())
    }
}

/// Serialize one entry as a v4 journal line: the entry's own fields plus the
/// stamped `seq` (§C.1). The envelope keys are exclusive (§C.1 rule), so
/// inserting `seq` cannot collide with a payload field.
fn v4_line(entry: &SessionTreeEntry, seq: u64) -> Result<String, anyhow::Error> {
    let mut value = serde_json::to_value(entry)?;
    if let Some(obj) = value.as_object_mut() {
        obj.insert("seq".into(), JsonValue::from(seq));
    }
    Ok(serde_json::to_string(&value)? + "\n")
}

/// Parse and wire-validate one session header line (without touching any
/// entry line). The shared header half of `load`, extracted for the
/// repository's bounded scans: a host-membership check or a list row needs
/// the header only, at microsecond cost. Returns the metadata plus the
/// on-disk format version.
pub fn parse_header_line(line: &[u8]) -> Result<(JsonlSessionMetadata, u32), anyhow::Error> {
    let header: JsonValue =
        serde_json::from_slice(line).map_err(|e| anyhow::anyhow!("invalid session header: {e}"))?;
    validate_header_wire(&header)?;
    let header: SessionHeader = serde_json::from_value(header)
        .map_err(|e| anyhow::anyhow!("invalid session header: {e}"))?;
    let version = header.version;
    let metadata = JsonlSessionMetadata {
        id: header.id,
        cwd: header.cwd,
        created_at: header.timestamp,
        parent_session_path: header.parent_session,
        metadata: header.metadata,
    };
    Ok((metadata, version))
}

/// Everything [`JsonlSessionStorage::load`] derives from one file's bytes.
struct ParsedFile {
    metadata: JsonlSessionMetadata,
    header_version: u32,
    entries: Vec<SessionTreeEntry>,
    seq_index: std::collections::HashMap<String, u64>,
    leaf_id: Option<String>,
    /// The file's line order diverges from the chain order (a recoverable
    /// write-order inversion); the next append lays the chain order down.
    needs_rewrite: bool,
}

/// Parse a session file's bytes: the header, then the two-pass entry
/// validation. Shared by `load` (from disk, after the read fence) and the
/// append-time stale-view reload (from the write fence's fd), so both
/// enforce exactly one set of invariants.
fn parse_file(path: &Path, bytes: &[u8]) -> Result<ParsedFile, anyhow::Error> {
    // Torn-tail tolerance (§二.6): whether the file ends mid-line (no
    // trailing '\n') is decided once, up front. An unparseable TAIL line in
    // a file without the trailing newline is a torn append — the non-atomic
    // `write_all` racing a crash or a concurrent reader — and is dropped
    // with a warning instead of failing the whole chain (pre-fix, one bad
    // byte made the session permanently unreadable, compounding with the
    // cold read into a silent empty history). A parse failure on ANY
    // earlier line — or on a newline-terminated tail — stays a hard error:
    // committed history is never silently truncated.
    let ends_with_newline = bytes.last() == Some(&b'\n');
    // A zero-byte file is "empty"; a file whose first line exists but is
    // blank is "blank" — two distinct rejections, exactly as before.
    if bytes.is_empty() {
        anyhow::bail!("session file is empty (no header line)");
    }
    let mut raw_lines = bytes.split(|&b| b == b'\n');
    // The header is the FIRST line, blank or not — a blank first line is an
    // explicit rejection (the file is not a session file), never skipped
    // over in favor of the first non-empty line.
    let header_line = raw_lines
        .next()
        .ok_or_else(|| anyhow::anyhow!("session file is empty (no header line)"))?;
    if header_line.trim_ascii().is_empty() {
        anyhow::bail!("session file header is blank");
    }
    // Blank lines BETWEEN entries are skipped, as before.
    let lines = raw_lines.filter(|line| !line.trim_ascii().is_empty());
    let header: JsonValue = serde_json::from_slice(header_line)
        .map_err(|e| anyhow::anyhow!("invalid session header: {e}"))?;
    validate_header_wire(&header)?;
    let header: SessionHeader = serde_json::from_value(header)
        .map_err(|e| anyhow::anyhow!("invalid session header: {e}"))?;
    let metadata = JsonlSessionMetadata {
        id: header.id,
        cwd: header.cwd,
        created_at: header.timestamp,
        parent_session_path: header.parent_session,
        metadata: header.metadata,
    };

    // Pass 1 — line-level integrity: every line must parse, carry a
    // well-formed wire shape, deserialize, and hold a unique non-empty id.
    // Order-sensitive checks are deferred to pass 2 so a recoverable
    // write-order inversion cannot condemn the whole session.
    let entry_lines: Vec<&[u8]> = lines.collect();
    let v4 = header.version >= FORMAT_VERSION;
    let mut entries: Vec<SessionTreeEntry> = Vec::with_capacity(entry_lines.len());
    let mut wire_seq: Vec<Option<u64>> = Vec::with_capacity(entry_lines.len());
    let mut seen_ids = std::collections::HashSet::new();
    for (index, line) in entry_lines.iter().enumerate() {
        let value: JsonValue = match serde_json::from_slice(line) {
            Ok(value) => value,
            Err(err) => {
                if !ends_with_newline && index + 1 == entry_lines.len() {
                    tracing::warn!(
                        path = %path.display(),
                        "dropping an unterminated unparseable tail line (torn append): {err}"
                    );
                    break;
                }
                return Err(anyhow::anyhow!(
                    "unparseable session entry line in {}: {err}",
                    path.display()
                ));
            }
        };
        // Wire-level structural checks before deserializing: a missing
        // required field must not be silently read as `null` (TS
        // `parseEntryLine` treats a missing `parentId`/`targetId` as an
        // invalid entry).
        validate_entry_wire(&value)?;
        let seq = value.get("seq").and_then(JsonValue::as_u64);
        let entry: SessionTreeEntry = serde_json::from_value(value)?;
        // A duplicate id would make the walk index silently overwrite one
        // entry with the other — reject the file instead of restoring a
        // wrong ancestry.
        if entry.id().is_empty() {
            anyhow::bail!("session file contains an entry with an empty id");
        }
        if !seen_ids.insert(entry.id().to_string()) {
            anyhow::bail!("duplicate entry id {} in session file", entry.id());
        }
        wire_seq.push(seq);
        entries.push(entry);
    }

    // Pass 2 — chain integrity, order-independent. Every parent must exist
    // SOMEWHERE in the file; depths come from a memoized parent walk (a
    // cycle is corruption, not a legal shape); a v4 line must carry its
    // depth as `seq`. Line order is deliberately NOT part of these checks:
    // two storage instances writing one file (each holds a private append
    // lock) can land a child line before its parent while the seq values
    // stay truthful. The loader reconciles against the seq values and
    // orders memory by the chain; the next append heals the file.
    let id_pos: std::collections::HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| (entry.id(), i))
        .collect();
    let mut depth: Vec<u64> = vec![u64::MAX; entries.len()];
    for start in 0..entries.len() {
        if depth[start] != u64::MAX {
            continue;
        }
        // Walk up the parent chain, stacking unvisited nodes, until a
        // memoized ancestor or a root resolves; then unwind the stack.
        let mut stack: Vec<usize> = Vec::new();
        let mut cursor = start;
        loop {
            let Some(parent) = entries[cursor].parent_id() else {
                depth[cursor] = 0;
                break;
            };
            let Some(&parent_index) = id_pos.get(parent) else {
                anyhow::bail!(
                    "session entry {} references unknown parent {parent}",
                    entries[cursor].id()
                );
            };
            stack.push(cursor);
            if depth[parent_index] != u64::MAX {
                break;
            }
            // A cycle never reaches a memoized node or a root, so the stack
            // outgrows the file — that is the cycle guard.
            if stack.len() > entries.len() {
                anyhow::bail!(
                    "session entry chain contains a cycle at {}",
                    entries[start].id()
                );
            }
            cursor = parent_index;
        }
        while let Some(node) = stack.pop() {
            let parent = entries[node]
                .parent_id()
                .expect("stacked nodes have parents");
            depth[node] = depth[id_pos[parent]] + 1;
        }
    }

    let mut needs_rewrite = false;
    let mut seq_index = std::collections::HashMap::with_capacity(entries.len());
    for (i, entry) in entries.iter().enumerate() {
        if v4 {
            // A mismatch between the stamped seq and the chain depth is
            // corruption, not a renumbering — same rule as before, now
            // checked order-independently.
            match wire_seq[i] {
                Some(seq) if seq == depth[i] => {}
                Some(seq) => anyhow::bail!(
                    "session entry {} carries seq {seq} but its chain depth is {}",
                    entry.id(),
                    depth[i]
                ),
                None => {
                    anyhow::bail!("v4 session entry {} is missing its seq field", entry.id())
                }
            }
        }
        // An entry whose parent sits LATER in the file is the inversion
        // shape the pre-fence races left behind.
        if entry.parent_id().is_some_and(|parent| id_pos[parent] > i) {
            needs_rewrite = true;
        }
        seq_index.insert(entry.id().to_string(), depth[i]);
    }
    if needs_rewrite {
        // Memory takes the chain's order (depth, then file position as the
        // deterministic tie-break), so the cursor tail, `get_entries`
        // windowing and any later whole-file rewrite all read the
        // reconciled sequence.
        let mut order: Vec<usize> = (0..entries.len()).collect();
        order.sort_by_key(|&i| (depth[i], i));
        entries = order.iter().map(|&i| entries[i].clone()).collect();
    }
    // The cursor follows the chain tail: a trailing `leaf` entry redirects
    // to its `targetId`, otherwise the tail entry's own id. For a well
    // ordered file the tail is the last line, exactly as before.
    let leaf_id = entries.last().and_then(SessionTreeEntry::leaf_cursor_after);

    Ok(ParsedFile {
        metadata,
        header_version: header.version,
        entries,
        seq_index,
        leaf_id,
        needs_rewrite,
    })
}

/// The cross-instance write fence: an exclusive `flock` over the session
/// file, acquired by path. The per-instance append lock cannot see a SECOND
/// storage object over the same file — the cold-append path and embedder
/// processes open their own instances — and two private locks interleaving
/// `write_all` calls is exactly how a child line once landed before its
/// parent. The fence serializes append critical sections across ALL
/// instances of the file, process-wide and system-wide.
struct WriteFence {
    file: File,
}

impl WriteFence {
    /// Acquire the fence, retrying a bounded number of times when the path
    /// was renamed mid-acquire (the v3 migration and the inversion heal swap
    /// the file with an atomic rename; an fd opened before the rename would
    /// fence a replaced inode). `Ok(None)` when the file does not exist — a
    /// deferred session has nothing on disk to fence, and no other instance
    /// can contend for a file that is not there.
    async fn acquire(path: &Path) -> Result<Option<Self>, anyhow::Error> {
        for _ in 0..16 {
            if !tokio::fs::try_exists(path).await? {
                return Ok(None);
            }
            let owned_path = path.to_path_buf();
            let attempt =
                tokio::task::spawn_blocking(move || -> Result<Option<File>, anyhow::Error> {
                    use std::os::unix::io::AsRawFd;
                    let file = std::fs::OpenOptions::new()
                        .append(true)
                        .read(true)
                        .open(&owned_path)?;
                    let fd = file.as_raw_fd();
                    // Blocking acquire: critical sections are one line long,
                    // so waiters queue briefly; the blocking pool absorbs
                    // the parked threads.
                    if unsafe { libc::flock(fd, libc::LOCK_EX) } != 0 {
                        anyhow::bail!(
                            "failed to fence {}: {}",
                            owned_path.display(),
                            std::io::Error::last_os_error()
                        );
                    }
                    // The path must still resolve to the fenced inode: a
                    // rename between open and lock leaves this fd fencing
                    // the replaced file. `None` asks the caller to retry.
                    let fenced = file.metadata()?;
                    let current = std::fs::metadata(&owned_path)?;
                    use std::os::unix::fs::MetadataExt;
                    if fenced.dev() != current.dev() || fenced.ino() != current.ino() {
                        return Ok(None);
                    }
                    Ok(Some(File::from_std(file)))
                })
                .await
                .map_err(|e| anyhow::anyhow!("fence task failed: {e}"))?;
            match attempt {
                Ok(Some(file)) => return Ok(Some(Self { file })),
                Ok(None) => continue,
                Err(error) => return Err(error),
            }
        }
        anyhow::bail!(
            "session file {} kept swapping under the write fence",
            path.display()
        )
    }
}

/// Take a shared `flock` on an already-open file — the read fence. Loads
/// see a stable snapshot, never a mid-append tail; the lock lives with the
/// file handle and releases when the caller drops it.
async fn flock_shared(file: &File) -> Result<(), anyhow::Error> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    let rc = tokio::task::spawn_blocking(move || unsafe { libc::flock(fd, libc::LOCK_SH) })
        .await
        .map_err(|e| anyhow::anyhow!("flock task failed: {e}"))?;
    if rc != 0 {
        anyhow::bail!(
            "failed to fence a session file for reading: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Re-point an entry at a new parent — the stale-leaf recovery of the
/// fenced append. Round-trips through JSON so new entry variants need no
/// case-by-case handling on this rare path.
fn reparent_entry(
    entry: &SessionTreeEntry,
    parent: Option<&str>,
) -> Result<SessionTreeEntry, anyhow::Error> {
    let mut value = serde_json::to_value(entry)?;
    if let Some(obj) = value.as_object_mut() {
        let parent = match parent {
            Some(id) => JsonValue::String(id.to_string()),
            None => JsonValue::Null,
        };
        obj.insert("parentId".into(), parent);
    }
    Ok(serde_json::from_value(value)?)
}

/// Wire-level header checks on the raw JSON, mirroring the TS
/// `parseHeaderLine`: type/version identity, non-empty id and cwd, and — the
/// distinction serde's `Option` cannot make — a present-but-null
/// `parentSession` or `metadata` is rejected while an absent one is fine.
/// Shared by `load` (rejecting damaged files) and `create` (never writing a
/// header its own `open` would reject). v3 headers are accepted on read
/// (seq backfilled in memory; the file becomes v4 on first append).
fn validate_header_wire(value: &JsonValue) -> Result<(), anyhow::Error> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("session header is not an object"))?;
    if obj.get("type").and_then(JsonValue::as_str) != Some("session") {
        anyhow::bail!("session file first line is not a session header");
    }
    let version = obj
        .get("version")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| anyhow::anyhow!("session header is missing version"))?;
    if version != FORMAT_VERSION as u64 && version != LEGACY_FORMAT_VERSION as u64 {
        anyhow::bail!("unsupported session version {version}");
    }
    match obj.get("id") {
        Some(JsonValue::String(id)) if !id.is_empty() => {}
        _ => anyhow::bail!("session header is missing id"),
    }
    match obj.get("cwd") {
        Some(JsonValue::String(cwd)) if !cwd.is_empty() => {}
        _ => anyhow::bail!("session header is missing cwd"),
    }
    if !matches!(obj.get("timestamp"), Some(JsonValue::String(_))) {
        anyhow::bail!("session header is missing timestamp");
    }
    if obj.contains_key("parentSession")
        && !matches!(obj.get("parentSession"), Some(JsonValue::String(_)))
    {
        anyhow::bail!("session header parentSession must be a string");
    }
    if obj.contains_key("metadata") && !matches!(obj.get("metadata"), Some(JsonValue::Object(_))) {
        anyhow::bail!("session header metadata must be an object");
    }
    Ok(())
}

/// Wire-level structural checks on a raw entry object before deserializing,
/// mirroring the TS `parseEntryLine`: `parentId` (and `targetId` on `leaf`
/// entries) must be present as `null|string` — a missing field is corruption,
/// not a silent root or empty cursor.
fn validate_entry_wire(value: &JsonValue) -> Result<(), anyhow::Error> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("session entry is not an object"))?;
    let kind = obj
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    let field_ok = |name: &str| {
        matches!(
            obj.get(name),
            Some(JsonValue::Null) | Some(JsonValue::String(_))
        )
    };
    if !field_ok("parentId") {
        anyhow::bail!("session entry of type {kind} has invalid parentId (must be null|string)");
    }
    if kind == "leaf" && !field_ok("targetId") {
        anyhow::bail!("leaf entry has invalid targetId (must be null|string)");
    }
    Ok(())
}

#[async_trait::async_trait]
impl SessionStorage for JsonlSessionStorage {
    async fn create_entry_id(&self) -> Result<String, anyhow::Error> {
        Ok(uuid::Uuid::new_v4().to_string())
    }

    async fn append_entry(&self, entry: &SessionTreeEntry) -> Result<(), anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        self.append_entry_locked(entry, false).await
    }

    async fn append_entry_durable(&self, entry: &SessionTreeEntry) -> Result<(), anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        self.append_entry_locked(entry, true).await
    }

    async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, anyhow::Error> {
        let entries = self.entries.lock().await;
        Ok(entries.iter().find(|e| e.id() == id).cloned())
    }

    async fn get_leaf_id(&self) -> Result<Option<String>, anyhow::Error> {
        let leaf_id = self.leaf_id.lock().await.clone();
        // A cursor pointing at a since-removed entry is corruption, not a
        // branch — surface it rather than silently walking from nothing.
        if let Some(id) = &leaf_id {
            let exists = self.entries.lock().await.iter().any(|e| e.id() == id);
            if !exists {
                anyhow::bail!("leaf id {id} not found among session entries");
            }
        }
        Ok(leaf_id)
    }

    async fn set_leaf_id(&self, leaf_id: Option<&str>) -> Result<(), anyhow::Error> {
        let _guard = self.append_lock.lock().await;
        // Validate the target exists before recording the move.
        if let Some(id) = leaf_id {
            let exists = self.entries.lock().await.iter().any(|e| e.id() == id);
            if !exists {
                anyhow::bail!("entry {id} not found");
            }
        }
        let parent_id = self.leaf_id.lock().await.clone();
        let id = self.create_entry_id().await?;
        let entry = SessionTreeEntry::Leaf {
            id,
            parent_id,
            timestamp: chrono::Utc::now(),
            target_id: leaf_id.map(|s| s.to_string()),
        };
        // Reuse the shared append path so the leaf entry lands on disk, in the
        // in-memory index, and as the cursor through one code path — the
        // cursor becomes the leaf's `targetId` via `leaf_cursor_after`.
        self.append_entry_locked(&entry, false).await
    }

    async fn get_entries(
        &self,
        cursor: crate::session::SessionEntryCursor,
    ) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        let entries = self.entries.lock().await;
        let tail = entries.iter().skip(cursor.after_entry_seq);
        Ok(match cursor.limit {
            Some(limit) => tail.take(limit).cloned().collect(),
            None => tail.cloned().collect(),
        })
    }

    async fn find_entries(
        &self,
        entry_type: crate::session::EntryType,
    ) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        Ok(self
            .entries
            .lock()
            .await
            .iter()
            .filter(|e| crate::session::entry_kind(e) == entry_type)
            .cloned()
            .collect())
    }

    async fn get_label(&self, id: &str) -> Result<Option<String>, anyhow::Error> {
        Ok(self
            .entries
            .lock()
            .await
            .iter()
            .filter_map(|e| match e {
                SessionTreeEntry::Label {
                    target_id, label, ..
                } if target_id == id => Some(label.as_deref().unwrap_or("").trim()),
                _ => None,
            })
            // The latest label for a target wins; a blank one clears it.
            .next_back()
            .filter(|l| !l.is_empty())
            .map(str::to_string))
    }

    async fn get_session_name(&self) -> Result<Option<String>, anyhow::Error> {
        Ok(self
            .entries
            .lock()
            .await
            .iter()
            .filter_map(|e| match e {
                SessionTreeEntry::SessionInfo { name, .. } => {
                    Some(name.as_deref().unwrap_or("").trim())
                }
                _ => None,
            })
            .next_back()
            .filter(|n| !n.is_empty())
            .map(str::to_string))
    }

    async fn get_session_stats(&self) -> Result<crate::session::SessionStats, anyhow::Error> {
        let entries = self.entries.lock().await;
        let mut stats = crate::session::SessionStats::default();
        for entry in entries.iter() {
            let usage = match entry {
                SessionTreeEntry::Message { message, .. } => {
                    stats.message_count += 1;
                    match message {
                        crate::types::AgentMessage::Assistant { usage, .. } => Some(&**usage),
                        _ => None,
                    }
                }
                SessionTreeEntry::Compaction { usage, .. }
                | SessionTreeEntry::BranchSummary { usage, .. } => usage.as_ref(),
                _ => None,
            };
            // An entry recorded before cost accounting reports no cost; its
            // tokens are unpriced and stay out of every figure, so the totals
            // describe one consistent set of calls.
            let Some(usage) = usage.filter(|u| u.cost.is_some()) else {
                continue;
            };
            let cost = usage.cost.as_ref().expect("filtered on cost presence");
            stats.cached_tokens += usage.cache_read_input_tokens;
            stats.uncached_tokens += usage.input_tokens + usage.cache_creation_input_tokens;
            // Summed from the classes rather than the provider's reported
            // total, which only some shapes populate.
            stats.total_tokens += usage.input_tokens
                + usage.output_tokens
                + usage.cache_read_input_tokens
                + usage.cache_creation_input_tokens;
            stats.cost_total += cost.total;
        }
        Ok(stats)
    }

    async fn get_path(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        let entries = self.entries.lock().await;

        let target_id = match leaf_id {
            None => return Ok(Vec::new()),
            Some(id) if entries.iter().any(|e| e.id() == id) => id.to_string(),
            // An explicit id unknown to storage is an error — the TS
            // storage's `not_found`. Silently walking from another entry
            // would fabricate a path the caller never asked for.
            Some(id) => anyhow::bail!("entry {id} not found"),
        };

        let mut index: std::collections::HashMap<&str, &SessionTreeEntry> =
            entries.iter().map(|e| (e.id(), e)).collect();

        let mut path: Vec<&SessionTreeEntry> = Vec::new();
        let mut current_id: Option<&str> = Some(&target_id);
        while let Some(id) = current_id {
            // `remove` doubles as cycle protection: each entry is visited at
            // most once. A miss is either a parent id with no entry — the TS
            // storage's `invalid_session` — or a parent-id cycle; both mean
            // the chain is broken, and a truncated path would silently drop
            // history, so this is an error, never a partial result.
            let entry = match index.remove(id) {
                Some(e) => e,
                None => anyhow::bail!("entry {id} not found: session chain is broken"),
            };
            current_id = entry.parent_id();
            path.push(entry);
        }

        path.reverse();
        Ok(path.into_iter().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    /// §二.6 torn-tail tolerance: an unparseable TAIL line in a file
    /// WITHOUT the trailing newline is a torn append (the non-atomic
    /// `write_all` racing a crash or a concurrent reader) — dropped with a
    /// warning, the committed chain stays readable. A newline-terminated
    /// bad tail, or a bad line mid-chain, stays a hard error: committed
    /// history is never silently truncated.
    #[tokio::test]
    async fn torn_tail_tolerated_committed_corruption_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let header = "{\"type\":\"session\",\"version\":3,\"id\":\"tt-1\",\"timestamp\":\"2026-01-01T00:00:00Z\",\"cwd\":\"/tmp\",\"metadata\":{\"host\":\"manox\"}}";
        let valid = "{\"type\":\"model_change\",\"id\":\"tt-m0\",\"parentId\":null,\"timestamp\":\"2026-01-01T00:00:01Z\",\"provider\":\"p\",\"modelId\":\"m\"}";
        let torn = "{\"type\":\"model_ch";

        // Torn tail: no trailing newline after the partial line.
        let torn_path = dir.path().join("tt-torn.jsonl");
        std::fs::write(&torn_path, format!("{header}\n{valid}\n{torn}")).unwrap();
        let storage = JsonlSessionStorage::open(&torn_path)
            .await
            .expect("the torn tail is tolerated");
        let records = storage.journal_range(0, u64::MAX).await.unwrap();
        assert_eq!(
            records.len(),
            1,
            "the committed entry survives, the torn tail drops"
        );

        // Newline-terminated bad tail: a committed line that does not parse
        // is corruption, not a torn write.
        let bad_tail = dir.path().join("tt-badtail.jsonl");
        std::fs::write(&bad_tail, format!("{header}\n{valid}\n{torn}\n")).unwrap();
        assert!(
            JsonlSessionStorage::open(&bad_tail).await.is_err(),
            "a terminated bad tail is a hard error"
        );

        // Mid-chain corruption: never tolerated.
        let mid = dir.path().join("tt-mid.jsonl");
        std::fs::write(&mid, format!("{header}\n{torn}\n{valid}\n")).unwrap();
        assert!(
            JsonlSessionStorage::open(&mid).await.is_err(),
            "a mid-chain bad line is a hard error"
        );
    }

    use super::*;
    use crate::types::AgentMessage;

    fn meta() -> JsonlSessionMetadata {
        JsonlSessionMetadata {
            id: uuid::Uuid::new_v4().to_string(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        }
    }

    #[tokio::test]
    async fn test_jsonl_append_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
            .await
            .unwrap();

        let entry = SessionTreeEntry::Message {
            id: "test-1".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("hello"),
            origin: None,
        };
        storage.append_entry(&entry).await.unwrap();

        let fetched = storage.get_entry("test-1").await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().id(), "test-1");

        let all = storage.get_entries(Default::default()).await.unwrap();
        assert_eq!(all.len(), 1);
    }

    /// An assistant entry whose usage is priced, so it counts toward stats.
    fn priced_assistant(id: &str, input: u64, output: u64, cache_read: u64) -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: id.into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: crate::types::AgentMessage::Assistant {
                content: Vec::new(),
                model: "m".into(),
                provider: "p".into(),
                api: "a".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                raw_stop_reason: None,
                stop_reason: Some(crate::types::StopReason::Stop),
                usage: Box::new(crate::types::Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_read_input_tokens: cache_read,
                    cost: Some(crate::types::Cost {
                        total: 0.5,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                error_message: None,
                timestamp: chrono::Utc::now(),
            },
            origin: None,
        }
    }

    #[tokio::test]
    async fn session_stats_aggregate_priced_usage() {
        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("s.jsonl"), meta())
            .await
            .unwrap();
        storage
            .append_entry(&priced_assistant("a1", 10, 5, 3))
            .await
            .unwrap();
        // A user message counts toward the message total but carries no usage.
        storage
            .append_entry(&SessionTreeEntry::Message {
                id: "u1".into(),
                parent_id: Some("a1".into()),
                timestamp: chrono::Utc::now(),
                message: crate::types::AgentMessage::user("hi"),
                origin: None,
            })
            .await
            .unwrap();
        // A compaction is a model call the session paid for too.
        storage
            .append_entry(&SessionTreeEntry::Compaction {
                id: "c1".into(),
                parent_id: Some("u1".into()),
                timestamp: chrono::Utc::now(),
                summary: "s".into(),
                first_kept_entry_id: None,
                tokens_before: 0,
                retained_tail: None,
                details: None,
                usage: Some(crate::types::Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cost: Some(crate::types::Cost {
                        total: 1.5,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                from_hook: None,
            })
            .await
            .unwrap();

        let stats = storage.get_session_stats().await.unwrap();
        assert_eq!(stats.message_count, 2, "both message entries count");
        assert_eq!(stats.cached_tokens, 3);
        assert_eq!(stats.uncached_tokens, 110);
        assert_eq!(stats.total_tokens, 138);
        assert!(
            (stats.cost_total - 2.0).abs() < 1e-9,
            "{}",
            stats.cost_total
        );
    }

    #[tokio::test]
    async fn session_stats_skip_unpriced_usage_but_still_count_the_message() {
        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("s.jsonl"), meta())
            .await
            .unwrap();
        let mut entry = priced_assistant("a1", 10, 5, 3);
        if let SessionTreeEntry::Message {
            message: crate::types::AgentMessage::Assistant { usage, .. },
            ..
        } = &mut entry
        {
            usage.cost = None;
        }
        storage.append_entry(&entry).await.unwrap();

        let stats = storage.get_session_stats().await.unwrap();
        assert_eq!(stats.message_count, 1);
        assert_eq!(stats.total_tokens, 0, "unpriced tokens stay out");
        assert_eq!(stats.cost_total, 0.0);
    }

    #[tokio::test]
    async fn session_stats_sum_the_classes_not_the_reported_total() {
        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("s.jsonl"), meta())
            .await
            .unwrap();
        let mut entry = priced_assistant("a1", 60, 30, 10);
        if let SessionTreeEntry::Message {
            message: crate::types::AgentMessage::Assistant { usage, .. },
            ..
        } = &mut entry
        {
            // Only some provider shapes report a total; trusting it would make
            // the aggregate disagree between shapes.
            usage.total_tokens = 9999;
        }
        storage.append_entry(&entry).await.unwrap();

        let stats = storage.get_session_stats().await.unwrap();
        assert_eq!(stats.total_tokens, 100);
    }

    #[tokio::test]
    async fn labels_resolve_to_the_latest_and_blank_clears() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let storage = JsonlSessionStorage::create(&path, meta()).await.unwrap();
        let target = SessionTreeEntry::Message {
            id: "m1".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: crate::types::AgentMessage::user("hi"),
            origin: None,
        };
        storage.append_entry(&target).await.unwrap();
        for (id, label) in [
            ("l1", Some("first")),
            ("l2", Some("  second  ")),
            ("l3", None),
            ("l4", Some("final")),
        ] {
            storage
                .append_entry(&SessionTreeEntry::Label {
                    id: id.into(),
                    parent_id: None,
                    timestamp: chrono::Utc::now(),
                    target_id: "m1".into(),
                    label: label.map(Into::into),
                })
                .await
                .unwrap();
        }
        assert_eq!(
            storage.get_label("m1").await.unwrap().as_deref(),
            Some("final")
        );
        assert_eq!(storage.get_label("nope").await.unwrap(), None);

        // Rebuilt from the file on reopen, not only maintained on append.
        drop(storage);
        let reopened = JsonlSessionStorage::open(&path).await.unwrap();
        assert_eq!(
            reopened.get_label("m1").await.unwrap().as_deref(),
            Some("final")
        );
    }

    #[tokio::test]
    async fn a_blank_label_reads_as_absent() {
        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("s.jsonl"), meta())
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::Label {
                id: "l1".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                target_id: "m1".into(),
                label: Some("   ".into()),
            })
            .await
            .unwrap();
        assert_eq!(storage.get_label("m1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn session_name_takes_the_latest_and_trims() {
        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("s.jsonl"), meta())
            .await
            .unwrap();
        assert_eq!(storage.get_session_name().await.unwrap(), None);
        for (id, name) in [("s1", Some("old")), ("s2", Some("  new  "))] {
            storage
                .append_entry(&SessionTreeEntry::SessionInfo {
                    id: id.into(),
                    parent_id: None,
                    timestamp: chrono::Utc::now(),
                    name: name.map(Into::into),
                })
                .await
                .unwrap();
        }
        assert_eq!(
            storage.get_session_name().await.unwrap().as_deref(),
            Some("new")
        );
    }

    #[tokio::test]
    async fn find_entries_filters_by_type() {
        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("s.jsonl"), meta())
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::Message {
                id: "m1".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                message: crate::types::AgentMessage::user("hi"),
                origin: None,
            })
            .await
            .unwrap();
        storage
            .append_entry(&SessionTreeEntry::SessionInfo {
                id: "s1".into(),
                parent_id: Some("m1".into()),
                timestamp: chrono::Utc::now(),
                name: Some("n".into()),
            })
            .await
            .unwrap();

        use crate::session::EntryType;
        assert_eq!(
            storage
                .find_entries(EntryType::Message)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            storage
                .find_entries(EntryType::SessionInfo)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(
            storage
                .find_entries(EntryType::Compaction)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn entry_cursor_windows_in_append_order() {
        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("s.jsonl"), meta())
            .await
            .unwrap();
        for i in 0..5 {
            storage
                .append_entry(&SessionTreeEntry::Message {
                    id: format!("m{i}"),
                    parent_id: (i > 0).then(|| format!("m{}", i - 1)),
                    timestamp: chrono::Utc::now(),
                    message: crate::types::AgentMessage::user(format!("{i}")),
                    origin: None,
                })
                .await
                .unwrap();
        }
        use crate::session::SessionEntryCursor;

        let window = storage
            .get_entries(SessionEntryCursor {
                after_entry_seq: 2,
                limit: Some(2),
            })
            .await
            .unwrap();
        let ids: Vec<&str> = window.iter().map(|e| e.id()).collect();
        assert_eq!(ids, vec!["m2", "m3"], "forward order from the cursor");

        let tail = storage
            .get_entries(SessionEntryCursor {
                after_entry_seq: 3,
                limit: None,
            })
            .await
            .unwrap();
        assert_eq!(tail.len(), 2, "no limit reads to the end");

        // A cursor past the end is how a poller sits idle, not an error.
        let past = storage
            .get_entries(SessionEntryCursor {
                after_entry_seq: 99,
                limit: None,
            })
            .await
            .unwrap();
        assert!(past.is_empty());
    }

    #[tokio::test]
    async fn bash_execution_entry_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let storage = JsonlSessionStorage::create(&path, meta()).await.unwrap();
        storage
            .append_entry(&SessionTreeEntry::Message {
                id: "b1".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                message: AgentMessage::BashExecution {
                    command: "cargo test".into(),
                    output: "tail".into(),
                    exit_code: Some(101),
                    cancelled: false,
                    truncated: true,
                    full_output_path: Some("/tmp/pi-bash-1.log".into()),
                    exclude_from_context: Some(true),
                    timestamp: chrono::Utc::now(),
                },
                origin: None,
            })
            .await
            .unwrap();
        drop(storage);

        let reopened = JsonlSessionStorage::open(&path).await.unwrap();
        let entry = reopened.get_entry("b1").await.unwrap().unwrap();
        let SessionTreeEntry::Message { message, .. } = entry else {
            panic!("expected a message entry");
        };
        match message {
            AgentMessage::BashExecution {
                command,
                output,
                exit_code,
                truncated,
                full_output_path,
                exclude_from_context,
                ..
            } => {
                assert_eq!(command, "cargo test");
                assert_eq!(output, "tail");
                assert_eq!(exit_code, Some(101));
                assert!(truncated);
                assert_eq!(full_output_path.as_deref(), Some("/tmp/pi-bash-1.log"));
                // The withholding must survive the round trip, or a reopened
                // session would start feeding the model what the user hid.
                assert_eq!(exclude_from_context, Some(true));
            }
            other => panic!("expected BashExecution, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_jsonl_leaf_tracking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
            .await
            .unwrap();

        assert!(storage.get_leaf_id().await.unwrap().is_none());

        let msg = SessionTreeEntry::Message {
            id: "m1".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("hi"),
            origin: None,
        };
        storage.append_entry(&msg).await.unwrap();
        assert_eq!(storage.get_leaf_id().await.unwrap(), Some("m1".into()));

        // set_leaf_id persists a `leaf` entry that redirects the cursor to the
        // target, matching the TS Pi v3 schema (not an in-memory override).
        storage.set_leaf_id(Some("m1")).await.unwrap();
        assert_eq!(storage.get_leaf_id().await.unwrap(), Some("m1".into()));

        // A trailing leaf entry redirects: reopening lands the cursor on the
        // target id, not the leaf entry's own id.
        let on_disk = tokio::fs::read_to_string(&path).await.unwrap();
        let leaf_line = on_disk
            .lines()
            .find(|l| l.contains(r#""type":"leaf""#))
            .unwrap();
        assert!(
            leaf_line.contains(r#""targetId":"m1""#),
            "expected leaf entry with targetId, got: {leaf_line}"
        );

        let reopened = JsonlSessionStorage::open(&dir.path().join("session.jsonl"))
            .await
            .unwrap();
        assert_eq!(reopened.get_leaf_id().await.unwrap(), Some("m1".into()));

        // set_leaf_id(None) records a cursor reset to null.
        storage.set_leaf_id(None).await.unwrap();
        assert!(storage.get_leaf_id().await.unwrap().is_none());

        // Pointing the cursor at a non-existent entry is an error, not a
        // silent override.
        let err = storage.set_leaf_id(Some("missing")).await.unwrap_err();
        assert!(err.to_string().contains("not found"), "{}", err);
    }

    #[tokio::test]
    async fn test_reopen_restores_entries_and_leaf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        {
            let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
                .await
                .unwrap();
            let msg = SessionTreeEntry::Message {
                id: "m1".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                message: AgentMessage::user("hi"),
                origin: None,
            };
            storage.append_entry(&msg).await.unwrap();
            // No `leaf` entry exists; the cursor is the last appended entry.
        }

        let storage = JsonlSessionStorage::open(&path).await.unwrap();
        let entries = storage.get_entries(Default::default()).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(storage.get_leaf_id().await.unwrap(), Some("m1".into()));

        // The first line on disk is the v3 session header.
        let header_line = tokio::fs::read_to_string(&path)
            .await
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert!(header_line.contains("\"type\":\"session\""));
        assert!(header_line.contains("\"version\":4"));
    }

    /// A header carrying `parentSession` and `metadata` must write those as
    /// camelCase on disk (multi-word fields would otherwise leak snake_case)
    /// and round-trip them on reopen.
    #[tokio::test]
    async fn test_header_writes_camel_case_parent_and_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let header_meta = JsonlSessionMetadata {
            id: "fork".into(),
            cwd: "/proj".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: Some(PathBuf::from("/sessions/parent.jsonl")),
            metadata: Some(serde_json::json!({ "origin": "forked" })),
        };
        let _storage = JsonlSessionStorage::create(&path, header_meta)
            .await
            .unwrap();

        let header_line = tokio::fs::read_to_string(&path)
            .await
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_string();
        assert!(
            header_line.contains("\"parentSession\":\"/sessions/parent.jsonl\""),
            "expected camelCase parentSession, got: {header_line}"
        );
        assert!(
            !header_line.contains("parent_session"),
            "snake_case parent_session leaked onto disk: {header_line}"
        );
        assert!(
            header_line.contains("\"metadata\":{\"origin\":\"forked\"}"),
            "expected metadata payload, got: {header_line}"
        );

        // Reopen: the file is authoritative, so the fork metadata survives.
        let reopened = JsonlSessionStorage::open(&path).await.unwrap();
        let m = &reopened.metadata;
        assert_eq!(m.id, "fork");
        assert_eq!(
            m.parent_session_path.as_deref(),
            Some(Path::new("/sessions/parent.jsonl"))
        );
        assert_eq!(
            m.metadata.as_ref(),
            Some(&serde_json::json!({ "origin": "forked" }))
        );
    }

    /// Open and surface the error string, sidestepping the `Debug` bound that
    /// `unwrap_err` would impose on the storage. Uses load-only `open`, so a
    /// pre-written bad file surfaces the parse error rather than a silent
    /// recreate.
    async fn open_err(path: &Path) -> String {
        match JsonlSessionStorage::open(path).await {
            Ok(_) => "ok".to_string(),
            Err(e) => e.to_string(),
        }
    }

    #[tokio::test]
    async fn test_open_rejects_bad_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        tokio::fs::write(&path, "not json\n").await.unwrap();
        let err = open_err(&path).await;
        assert!(err.contains("invalid session header"), "{err}");
    }

    /// A blank FIRST line is an explicit rejection, not a skip to the next
    /// non-empty line — the old loader's contract, restored.
    #[tokio::test]
    async fn test_open_rejects_blank_first_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let header = r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#;
        tokio::fs::write(&path, format!("\n{header}\n"))
            .await
            .unwrap();
        let err = open_err(&path).await;
        assert!(err.contains("header is blank"), "{err}");
    }

    #[tokio::test]
    async fn test_open_rejects_wrong_type_tag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let header = serde_json::json!({
            "type": "message",
            "version": 3,
            "id": "x",
            "timestamp": chrono::Utc::now(),
            "cwd": "/t",
        });
        tokio::fs::write(&path, format!("{header}\n"))
            .await
            .unwrap();
        let err = open_err(&path).await;
        assert!(err.contains("not a session header"), "{err}");
    }

    #[tokio::test]
    async fn test_open_rejects_wrong_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let header = serde_json::json!({
            "type": "session",
            "version": 2,
            "id": "x",
            "timestamp": chrono::Utc::now(),
            "cwd": "/t",
        });
        tokio::fs::write(&path, format!("{header}\n"))
            .await
            .unwrap();
        let err = open_err(&path).await;
        assert!(err.contains("unsupported session version"), "{err}");
    }

    #[tokio::test]
    async fn test_open_rejects_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        tokio::fs::write(&path, "").await.unwrap();
        let err = open_err(&path).await;
        assert!(err.contains("no header line"), "{err}");
    }

    #[tokio::test]
    async fn test_path_to_root() {
        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
            .await
            .unwrap();

        let root = SessionTreeEntry::Message {
            id: "root".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("root"),
            origin: None,
        };
        let child = SessionTreeEntry::Message {
            id: "child".into(),
            parent_id: Some("root".into()),
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("child"),
            origin: None,
        };
        let leaf = SessionTreeEntry::Message {
            id: "leaf".into(),
            parent_id: Some("child".into()),
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("leaf"),
            origin: None,
        };

        storage.append_entry(&root).await.unwrap();
        storage.append_entry(&child).await.unwrap();
        storage.append_entry(&leaf).await.unwrap();
        storage.set_leaf_id(Some("leaf")).await.unwrap();

        let path = storage.get_path(Some("leaf")).await.unwrap();
        assert_eq!(path.len(), 3);
        assert_eq!(path[0].id(), "root");
        assert_eq!(path[1].id(), "child");
        assert_eq!(path[2].id(), "leaf");
    }

    /// The walk crosses compaction boundaries: projection onto the active
    /// context is the session layer's job, not the walk's.
    #[tokio::test]
    async fn test_path_walks_past_compaction() {
        use crate::session::Session;

        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
            .await
            .unwrap();

        let pre = SessionTreeEntry::Message {
            id: "pre".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("pre-compaction"),
            origin: None,
        };
        let compaction = SessionTreeEntry::Compaction {
            id: "comp".into(),
            parent_id: Some("pre".into()),
            timestamp: chrono::Utc::now(),
            summary: "summarized".into(),
            first_kept_entry_id: None,
            tokens_before: 1000,
            retained_tail: None,
            usage: None,
            details: None,
            from_hook: None,
        };
        let post = SessionTreeEntry::Message {
            id: "post".into(),
            parent_id: Some("comp".into()),
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("post-compaction"),
            origin: None,
        };

        storage.append_entry(&pre).await.unwrap();
        storage.append_entry(&compaction).await.unwrap();
        storage.append_entry(&post).await.unwrap();
        storage.set_leaf_id(Some("post")).await.unwrap();

        let path = storage.get_path(Some("post")).await.unwrap();
        assert_eq!(path.len(), 3);
        assert_eq!(path[0].id(), "pre");
        assert_eq!(path[1].id(), "comp");
        assert_eq!(path[2].id(), "post");

        // The context projection keeps the compaction plus everything after
        // it; with no first_kept_entry_id, nothing before it survives.
        let session = Session::new(storage);
        let ctx = session.build_context_entries().await.unwrap();
        assert_eq!(ctx.len(), 2);
        assert_eq!(ctx[0].id(), "comp");
        assert_eq!(ctx[1].id(), "post");
    }

    #[tokio::test]
    async fn test_compaction_boundary_is_path_relative() {
        use crate::session::Session;

        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
            .await
            .unwrap();
        let base = chrono::Utc::now();
        let compaction = |id: &str, parent: &str, secs: i64| SessionTreeEntry::Compaction {
            id: id.into(),
            parent_id: Some(parent.into()),
            timestamp: base + chrono::Duration::seconds(secs),
            summary: id.into(),
            first_kept_entry_id: None,
            tokens_before: 0,
            retained_tail: None,
            usage: None,
            details: None,
            from_hook: None,
        };
        let message = |id: &str, parent: &str, secs: i64| SessionTreeEntry::Message {
            id: id.into(),
            parent_id: Some(parent.into()),
            timestamp: base + chrono::Duration::seconds(secs),
            message: AgentMessage::user(id),
            origin: None,
        };

        let root = SessionTreeEntry::Message {
            id: "root".into(),
            parent_id: None,
            timestamp: base,
            message: AgentMessage::user("root"),
            origin: None,
        };
        storage.append_entry(&root).await.unwrap();
        storage
            .append_entry(&compaction("compA", "root", 1))
            .await
            .unwrap();
        storage
            .append_entry(&message("postA", "compA", 2))
            .await
            .unwrap();
        storage
            .append_entry(&compaction("compB", "root", 3))
            .await
            .unwrap();

        let session = Session::new(storage);

        session.storage().set_leaf_id(Some("postA")).await.unwrap();
        let ts = session.latest_compaction_timestamp().await.unwrap();
        assert_eq!(ts, Some(base + chrono::Duration::seconds(1)));

        session.storage().set_leaf_id(Some("compB")).await.unwrap();
        let ts = session.latest_compaction_timestamp().await.unwrap();
        assert_eq!(ts, Some(base + chrono::Duration::seconds(3)));

        session.storage().set_leaf_id(Some("root")).await.unwrap();
        let ts = session.latest_compaction_timestamp().await.unwrap();
        assert_eq!(ts, None);
    }

    /// A Message entry persisted by manox must write camelCase `parentId` so
    /// the file is a valid TS Pi v3 session (and other tools reading it do not
    /// silently lose ancestry). Guards against dropping `rename_all` on the
    /// variant.
    #[tokio::test]
    async fn test_message_entry_writes_camel_case_parent_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
            .await
            .unwrap();

        let root = SessionTreeEntry::Message {
            id: "root".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("root"),
            origin: None,
        };
        let child = SessionTreeEntry::Message {
            id: "child".into(),
            parent_id: Some("root".into()),
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("child"),
            origin: None,
        };
        storage.append_entry(&root).await.unwrap();
        storage.append_entry(&child).await.unwrap();

        let on_disk = tokio::fs::read_to_string(&path).await.unwrap();
        let child_line = on_disk
            .lines()
            .find(|l| l.contains("\"id\":\"child\""))
            .unwrap();
        assert!(
            child_line.contains("\"parentId\":\"root\""),
            "expected camelCase parentId on disk, got: {child_line}"
        );
        assert!(
            !child_line.contains("parent_id"),
            "snake_case parent_id leaked onto disk: {child_line}"
        );

        // Ancestry survives the disk round-trip.
        let reopened = JsonlSessionStorage::open(&dir.path().join("session.jsonl"))
            .await
            .unwrap();
        let path = reopened.get_path(Some("child")).await.unwrap();
        assert_eq!(path.len(), 2);
        assert_eq!(path[1].parent_id(), Some("root"));
    }

    /// A real TS Pi v3 session file uses camelCase entry fields, stores a
    /// message's own timestamp as epoch milliseconds, and writes no `leaf`
    /// entries. Such a file must load with the leaf cursor at the last entry.
    #[tokio::test]
    async fn test_loads_real_ts_pi_v3_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        // Mirrors the on-disk shape captured from a real TS Pi session: header,
        // a model_change (camelCase modelId), a thinking_level_change
        // (camelCase thinkingLevel), and a message whose inner timestamp is
        // integer millis. No `leaf` entry.
        let contents = concat!(
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"model_change","id":"c1","parentId":null,"timestamp":"2026-05-28T07:13:46.617Z","provider":"anthropic","modelId":"claude-opus-4-7"}"#,
            "\n",
            r#"{"type":"thinking_level_change","id":"t1","parentId":"c1","timestamp":"2026-05-28T07:13:46.617Z","thinkingLevel":"medium"}"#,
            "\n",
            r#"{"type":"message","id":"m1","parentId":"t1","timestamp":"2026-05-28T07:14:32.753Z","message":{"role":"user","content":[{"type":"text","text":"hello"}],"timestamp":1779952472751}}"#,
            "\n",
        );
        tokio::fs::write(&path, contents).await.unwrap();

        let storage = JsonlSessionStorage::open(&path).await.unwrap();

        let entries = storage.get_entries(Default::default()).await.unwrap();
        assert_eq!(entries.len(), 3);
        // No `leaf` entry: the cursor is the last appended entry.
        assert_eq!(storage.get_leaf_id().await.unwrap(), Some("m1".into()));

        // The model_change and thinking_level_change deserialized with their
        // camelCase fields mapped.
        match &entries[0] {
            SessionTreeEntry::ModelChange { model_id, .. } => {
                assert_eq!(model_id, "claude-opus-4-7");
            }
            other => panic!("expected ModelChange, got {other:?}"),
        }
        match &entries[1] {
            SessionTreeEntry::ThinkingLevelChange { thinking_level, .. } => {
                assert_eq!(thinking_level, "medium");
            }
            other => panic!("expected ThinkingLevelChange, got {other:?}"),
        }
        // The message entry's inner message carried an epoch-millis timestamp
        // and a text content block.
        match &entries[2] {
            SessionTreeEntry::Message {
                id,
                parent_id,
                message,
                ..
            } => {
                assert_eq!(id, "m1");
                // camelCase `parentId` must deserialize into `parent_id` — a
                // missing `rename_all` on the variant silently drops ancestry.
                assert_eq!(parent_id.as_deref(), Some("t1"));
                match message {
                    AgentMessage::User { content, .. } => {
                        assert!(matches!(
                        &content[0], crate::types::ContentBlock::Text { text, .. } if text == "hello"
                                    ));
                    }
                    other => panic!("expected User message, got {other:?}"),
                }
            }
            other => panic!("expected Message entry, got {other:?}"),
        }

        // The full ancestry chain must survive a load: walking from the leaf
        // reaches the model_change and thinking_level_change entries via
        // camelCase `parentId`.
        let path = storage.get_path(Some("m1")).await.unwrap();
        assert_eq!(path.len(), 3);
        assert_eq!(path[2].id(), "m1");
        assert_eq!(path[2].parent_id(), Some("t1"));
        assert_eq!(path[1].id(), "t1");
        assert_eq!(path[0].id(), "c1");
        assert!(path[0].parent_id().is_none());
    }

    #[tokio::test]
    async fn test_loads_custom_entry_with_string_and_object_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        // Real TS Pi sessions carry `custom` entries whose `data` is either a
        // plain string or a JSON object. Both must load and expose id/parentId.
        let contents = concat!(
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"custom","id":"x1","parentId":null,"timestamp":"2026-05-28T07:13:46.617Z","customType":"note","data":"a plain string"}"#,
            "\n",
            r#"{"type":"custom","id":"x2","parentId":"x1","timestamp":"2026-05-28T07:13:46.617Z","customType":"flag","data":{"on":true,"n":3}}"#,
            "\n",
        );
        tokio::fs::write(&path, contents).await.unwrap();

        let storage = JsonlSessionStorage::open(&path).await.unwrap();
        let entries = storage.get_entries(Default::default()).await.unwrap();
        assert_eq!(entries.len(), 2);

        match &entries[0] {
            SessionTreeEntry::Custom {
                id,
                parent_id,
                custom_type,
                data,
                ..
            } => {
                assert_eq!(id, "x1");
                assert!(parent_id.is_none());
                assert_eq!(custom_type, "note");
                assert_eq!(data, &Some(serde_json::json!("a plain string")));
            }
            other => panic!("expected Custom (string data), got {other:?}"),
        }
        match &entries[1] {
            SessionTreeEntry::Custom {
                id,
                parent_id,
                custom_type,
                data,
                ..
            } => {
                assert_eq!(id, "x2");
                assert_eq!(parent_id.as_deref(), Some("x1"));
                assert_eq!(custom_type, "flag");
                assert_eq!(data, &Some(serde_json::json!({"on": true, "n": 3})));
            }
            other => panic!("expected Custom (object data), got {other:?}"),
        }

        // Custom entries link into the ancestry tree via parentId like any
        // other entry.
        let path = storage.get_path(Some("x2")).await.unwrap();
        assert_eq!(path.len(), 2);
        assert_eq!(path[1].id(), "x2");
        assert_eq!(path[1].parent_id(), Some("x1"));
        assert_eq!(path[0].id(), "x1");
    }

    /// A real TS Pi v3 session file may carry every entry kind in the flat
    /// wire shape, including a trailing `leaf` entry that redirects the
    /// cursor. Each must load into the matching variant with camelCase fields
    /// mapped, and a trailing leaf must land the cursor on its `targetId`.
    #[tokio::test]
    async fn test_loads_all_entry_wire_shapes() {
        use crate::types::ContentBlock;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let contents = concat!(
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
            "\n",
            // branch_summary: flat (summary is a string, fromId present),
            // not a nested object.
            r#"{"type":"branch_summary","id":"b1","parentId":null,"timestamp":"2026-05-28T07:13:46.617Z","fromId":"b0","summary":"did work","details":{"files":["a.rs"]},"usage":{"input":10,"output":5,"cacheRead":0,"cacheWrite":0,"totalTokens":15,"cost":{"input":1,"output":2,"cacheRead":0,"cacheWrite":0,"total":3}},"fromHook":true}"#,
            "\n",
            // label: targetId + label (no `text`).
            r#"{"type":"label","id":"l1","parentId":"b1","timestamp":"2026-05-28T07:13:46.617Z","targetId":"b1","label":"checkpoint"}"#,
            "\n",
            // custom_message: string content + display.
            r#"{"type":"custom_message","id":"cm1","parentId":"l1","timestamp":"2026-05-28T07:13:46.617Z","customType":"note","content":"hi","display":true}"#,
            "\n",
            // custom_message: array content (text + image with mimeType).
            r#"{"type":"custom_message","id":"cm2","parentId":"cm1","timestamp":"2026-05-28T07:13:46.617Z","customType":"attach","content":[{"type":"text","text":"see"},{"type":"image","data":"QkFE","mimeType":"image/png"}],"display":false}"#,
            "\n",
            // session_info: name omitted (optional).
            r#"{"type":"session_info","id":"si1","parentId":"cm2","timestamp":"2026-05-28T07:13:46.617Z"}"#,
            "\n",
            // custom: data omitted (optional).
            r#"{"type":"custom","id":"cu1","parentId":"si1","timestamp":"2026-05-28T07:13:46.617Z","customType":"marker"}"#,
            "\n",
            // trailing leaf entry: cursor redirects to targetId, not the
            // leaf entry's own id.
            r#"{"type":"leaf","id":"lf1","parentId":"cu1","timestamp":"2026-05-28T07:13:46.617Z","targetId":"b1"}"#,
            "\n",
        );
        tokio::fs::write(&path, contents).await.unwrap();

        let storage = JsonlSessionStorage::open(&path).await.unwrap();

        // Trailing leaf redirects the cursor to its targetId.
        assert_eq!(storage.get_leaf_id().await.unwrap(), Some("b1".into()));

        let entries = storage.get_entries(Default::default()).await.unwrap();
        assert_eq!(entries.len(), 7);

        match &entries[0] {
            SessionTreeEntry::BranchSummary {
                from_id,
                summary,
                details,
                usage,
                from_hook,
                ..
            } => {
                assert_eq!(from_id, "b0");
                assert_eq!(summary, "did work");
                assert_eq!(details, &Some(serde_json::json!({"files": ["a.rs"]})));
                assert!(usage.is_some());
                assert_eq!(*from_hook, Some(true));
            }
            other => panic!("expected BranchSummary, got {other:?}"),
        }
        match &entries[1] {
            SessionTreeEntry::Label {
                target_id, label, ..
            } => {
                assert_eq!(target_id, "b1");
                assert_eq!(label.as_deref(), Some("checkpoint"));
            }
            other => panic!("expected Label, got {other:?}"),
        }
        match &entries[2] {
            SessionTreeEntry::CustomMessage {
                content, display, ..
            } => {
                assert_eq!(content.len(), 1);
                assert!(matches!(&content[0], ContentBlock::Text { text, .. } if text == "hi"));
                assert!(*display);
            }
            other => panic!("expected CustomMessage (string), got {other:?}"),
        }
        match &entries[3] {
            SessionTreeEntry::CustomMessage { content, .. } => {
                assert_eq!(content.len(), 2);
                assert!(matches!(&content[0], ContentBlock::Text { text, .. } if text == "see"));
                assert!(
                    matches!(&content[1], ContentBlock::Image { mime_type, .. } if mime_type == "image/png")
                );
            }
            other => panic!("expected CustomMessage (array), got {other:?}"),
        }
        match &entries[4] {
            SessionTreeEntry::SessionInfo { name, .. } => {
                assert!(name.is_none());
            }
            other => panic!("expected SessionInfo, got {other:?}"),
        }
        match &entries[5] {
            SessionTreeEntry::Custom { data, .. } => {
                assert!(data.is_none());
            }
            other => panic!("expected Custom, got {other:?}"),
        }
        match &entries[6] {
            SessionTreeEntry::Leaf { target_id, .. } => {
                assert_eq!(target_id.as_deref(), Some("b1"));
            }
            other => panic!("expected Leaf, got {other:?}"),
        }
    }

    /// A `Label` entry with no label text must omit the field on disk (TS
    /// types it `string | undefined`), not serialize it as `null`.
    #[tokio::test]
    async fn test_label_entry_omits_unset_label_field() {
        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
            .await
            .unwrap();
        let entry = SessionTreeEntry::Label {
            id: "lab1".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            target_id: "t".into(),
            label: None,
        };
        storage.append_entry(&entry).await.unwrap();

        let on_disk = tokio::fs::read_to_string(dir.path().join("session.jsonl"))
            .await
            .unwrap();
        let label_line = on_disk
            .lines()
            .find(|l| l.contains("\"type\":\"label\""))
            .unwrap();
        assert!(
            !label_line.contains("\"label\":"),
            "unset label field leaked onto disk: {label_line}"
        );
    }

    /// A full branching lifecycle must stay consistent across disk round-trips:
    /// append → branch back via `set_leaf_id` → append again → reopen → walk.
    /// The later message parents onto the leaf's `targetId` (the cursor), and
    /// the leaf entry never appears in the walked context — matching TS
    /// `setLeafId` / `leafIdAfterEntry` / `buildSessionPath`.
    #[tokio::test]
    async fn test_branch_lifecycle_round_trips_consistently() {
        use crate::session::Session;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
            .await
            .unwrap();
        let session = Session::new(storage);

        let m1 = session
            .append_message(AgentMessage::user("first"))
            .await
            .unwrap();
        // Branch back to m1: a leaf entry is persisted, cursor redirects to m1.
        session.storage().set_leaf_id(Some(&m1)).await.unwrap();
        assert_eq!(
            session.storage().get_leaf_id().await.unwrap(),
            Some(m1.clone())
        );

        // A message appended after the branch parents onto the cursor (m1),
        // not the leaf entry's own id.
        let m2 = session
            .append_message(AgentMessage::user("second"))
            .await
            .unwrap();
        assert_eq!(
            session.storage().get_leaf_id().await.unwrap(),
            Some(m2.clone())
        );

        let entries = session
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap();
        // m1, the leaf entry, m2 — leaf is persisted, not an in-memory override.
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().any(|e| e.id() == m1));
        assert!(entries.iter().any(|e| e.id() == m2));
        assert!(
            entries.iter().any(|e| matches!(e, SessionTreeEntry::Leaf { target_id, .. } if target_id.as_deref() == Some(&m1))),
            "no leaf entry redirecting to m1 was persisted"
        );

        // Reopen: the trailing message is the cursor, the leaf survives on disk.
        let reopened = JsonlSessionStorage::open(&dir.path().join("session.jsonl"))
            .await
            .unwrap();
        assert_eq!(reopened.get_leaf_id().await.unwrap(), Some(m2.clone()));
        assert_eq!(
            reopened
                .get_entries(Default::default())
                .await
                .unwrap()
                .len(),
            3
        );

        // The walked context skips the leaf entry: m2 → m1, no leaf in path.
        let session = Session::new(reopened);
        let ctx = session.build_context_entries().await.unwrap();
        assert_eq!(ctx.len(), 2);
        assert_eq!(ctx[0].id(), m1);
        assert_eq!(ctx[1].id(), m2);
        assert!(
            !ctx.iter()
                .any(|e| matches!(e, SessionTreeEntry::Leaf { .. })),
            "leaf entry leaked into the walked context"
        );

        // The on-disk file is a strictly append-only sequence of valid lines.
        let on_disk = tokio::fs::read_to_string(&path).await.unwrap();
        let mut types: Vec<String> = on_disk
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .unwrap()
                    .get("type")
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(types.remove(0), "session");
        assert_eq!(types, vec!["message", "leaf", "message"]);
    }

    /// A TS-written session file carries no retained tail on its compaction
    /// entries: the kept segment is reconstructed by walking the tree from
    /// `firstKeptEntryId`. Loading such a file must rebuild the full context —
    /// summary carrier, kept messages, and post-boundary messages — with each
    /// message traced to the entry that produced it.
    #[tokio::test]
    async fn test_ts_file_without_retained_tail_rebuilds_the_kept_segment() {
        use crate::session::Session;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        // Mirrors a real TS Pi session after one compaction: messages m1..m3,
        // a compaction keeping from m2 onward (firstKeptEntryId, no tail
        // payload), then a post-compaction message. No `leaf` entry.
        let contents = concat!(
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-05-28T07:14:00.000Z","message":{"role":"user","content":[{"type":"text","text":"first question"}],"timestamp":1779952440000}}"#,
            "\n",
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-05-28T07:14:10.000Z","message":{"role":"user","content":[{"type":"text","text":"first answer"}],"timestamp":1779952450000}}"#,
            "\n",
            r#"{"type":"message","id":"m3","parentId":"m2","timestamp":"2026-05-28T07:14:20.000Z","message":{"role":"user","content":[{"type":"text","text":"follow up"}],"timestamp":1779952460000}}"#,
            "\n",
            r#"{"type":"compaction","id":"c1","parentId":"m3","timestamp":"2026-05-28T07:15:00.000Z","summary":"prior turns summarized","firstKeptEntryId":"m2","tokensBefore":9000}"#,
            "\n",
            r#"{"type":"message","id":"m4","parentId":"c1","timestamp":"2026-05-28T07:15:30.000Z","message":{"role":"user","content":[{"type":"text","text":"after compaction"}],"timestamp":1779952530000}}"#,
            "\n",
        );
        tokio::fs::write(&path, contents).await.unwrap();

        let storage = JsonlSessionStorage::open(&path).await.unwrap();
        let session = Session::new(storage);
        let context = session.build_session_context().await.unwrap();

        fn text_of(m: &AgentMessage) -> &str {
            match m {
                AgentMessage::User { content, .. } => match &content[0] {
                    crate::types::ContentBlock::Text { text, .. } => text.as_str(),
                    _ => "",
                },
                _ => "",
            }
        }

        // Summary carrier first, then the kept m2..m3 walked out of the tree,
        // then the post-boundary m4. m1 was summarized away.
        assert_eq!(context.messages.len(), 4, "{:?}", context.messages);
        assert_eq!(
            text_of(&context.messages[0]),
            "The conversation history before this point was compacted into the following summary:\n\n<summary>\nprior turns summarized\n</summary>"
        );
        assert_eq!(text_of(&context.messages[1]), "first answer");
        assert_eq!(text_of(&context.messages[2]), "follow up");
        assert_eq!(text_of(&context.messages[3]), "after compaction");

        // Every message traces to the entry that produced it — the summary to
        // the compaction entry itself — so a later compaction can resolve a
        // first-kept id for any position.
        assert_eq!(
            context.message_entry_ids,
            vec![
                Some("c1".to_string()),
                Some("m2".to_string()),
                Some("m3".to_string()),
                Some("m4".to_string()),
            ]
        );
        assert_eq!(context.thinking_level, None);
        assert_eq!(context.model, None);
    }

    /// A TS-written file may carry settings entries and damaged messages: a
    /// null message content reads as empty, and the context surfaces the
    /// reasoning tier and the model the path carries.
    #[tokio::test]
    async fn test_ts_file_settings_and_null_content_project() {
        use crate::session::{Session, SessionModelRef};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let contents = concat!(
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"model_change","id":"mc","parentId":null,"timestamp":"2026-05-28T07:13:46.617Z","provider":"anthropic","modelId":"claude-opus-4-7"}"#,
            "\n",
            r#"{"type":"thinking_level_change","id":"tl","parentId":"mc","timestamp":"2026-05-28T07:13:46.617Z","thinkingLevel":"high"}"#,
            "\n",
            r#"{"type":"message","id":"m1","parentId":"tl","timestamp":"2026-05-28T07:14:00.000Z","message":{"role":"user","content":[{"type":"text","text":"hello"}],"timestamp":1779952440000}}"#,
            "\n",
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-05-28T07:14:10.000Z","message":{"role":"assistant","content":null,"model":"claude-opus-4-7","provider":"anthropic","api":"anthropic","stopReason":"stop","timestamp":1779952450000}}"#,
            "\n",
        );
        tokio::fs::write(&path, contents).await.unwrap();

        let storage = JsonlSessionStorage::open(&path).await.unwrap();
        let session = Session::new(storage);
        let context = session.build_session_context().await.unwrap();

        // Settings entries contribute no message; the null-content assistant
        // survives as an empty message.
        assert_eq!(context.messages.len(), 2, "{:?}", context.messages);
        match &context.messages[1] {
            AgentMessage::Assistant { content, .. } => assert!(content.is_empty()),
            other => panic!("expected Assistant, got {other:?}"),
        }
        assert_eq!(context.thinking_level.as_deref(), Some("high"));
        assert_eq!(
            context.model,
            Some(SessionModelRef {
                provider: "anthropic".into(),
                model_id: "claude-opus-4-7".into(),
            })
        );
    }

    /// A walk that cannot complete loudly fails: v4 load validation rejects
    /// a parent id with no entry at `open` time (earlier than the v3 walk,
    /// same guarantee — never a truncated path), and an explicit leaf
    /// unknown to storage errors at `get_path`.
    #[tokio::test]
    async fn test_broken_session_chain_errors_instead_of_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let contents = concat!(
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-05-28T07:14:00.000Z","message":{"role":"user","content":[{"type":"text","text":"one"}],"timestamp":1779952440000}}"#,
            "\n",
            r#"{"type":"message","id":"m2","parentId":"ghost","timestamp":"2026-05-28T07:14:10.000Z","message":{"role":"user","content":[{"type":"text","text":"two"}],"timestamp":1779952450000}}"#,
            "\n",
        );
        tokio::fs::write(&path, contents).await.unwrap();
        // v4 load validation: the unknown parent surfaces at open, naming it.
        let err = match JsonlSessionStorage::open(&path).await {
            Err(e) => e,
            Ok(_) => panic!("open must reject a session whose chain references an unknown parent"),
        };
        assert!(
            err.to_string().contains("ghost"),
            "the error names the missing parent: {err}"
        );
    }

    /// A user message entry for batch/heal tests.
    fn message_entry(id: &str, parent: Option<&str>, text: &str) -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: id.into(),
            parent_id: parent.map(str::to_string),
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user(text),
            origin: None,
        }
    }

    /// append_entries on a line-order-inverted file routes through the
    /// per-entry heal path — which must drop the fence first: flock counts
    /// per open file description, so a second blocking LOCK_EX out of this
    /// process on the still-held file self-deadlocks. The timeout turns a
    /// regression back into a test failure instead of a hung suite.
    #[tokio::test]
    async fn append_entries_heals_an_inverted_file_without_self_deadlock() {
        use crate::session::Session;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        // The inverted fixture: a child line lands before its parent while
        // every seq stays truthful.
        let contents = concat!(
            r#"{"type":"session","version":4,"id":"s1","timestamp":"2026-09-11T08:25:14.336062Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"message","id":"x","parentId":null,"timestamp":"2026-09-11T08:25:20.000Z","seq":0,"message":{"role":"user","content":[{"type":"text","text":"x"}],"timestamp":1770000000000}}"#,
            "\n",
            r#"{"type":"message","id":"c1","parentId":"p","timestamp":"2026-09-11T08:41:34.789962Z","seq":2,"message":{"role":"user","content":[{"type":"text","text":"c1"}],"timestamp":1770000000000}}"#,
            "\n",
            r#"{"type":"message","id":"p","parentId":"x","timestamp":"2026-09-11T08:41:34.789225Z","seq":1,"message":{"role":"user","content":[{"type":"text","text":"p"}],"timestamp":1770000000000}}"#,
            "\n",
        );
        std::fs::write(&path, contents).unwrap();
        let storage = JsonlSessionStorage::open(&path).await.unwrap();
        // The batch chains onto the file's leaf (c1), the fork shape.
        let batch = vec![
            message_entry("b1", Some("c1"), "batch one"),
            message_entry("b2", Some("b1"), "batch two"),
        ];
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            storage.append_entries(&batch),
        )
        .await
        .expect("the heal path must not self-deadlock on the fence")
        .unwrap();

        // The file healed to chain order and carries the batch linearly.
        let reopened = JsonlSessionStorage::open(&path).await.unwrap();
        let session = Session::new(reopened);
        let branch = session.get_branch().await.unwrap();
        let texts: Vec<&str> = branch.iter().map(user_text).collect();
        assert_eq!(texts, vec!["x", "p", "c1", "batch one", "batch two"]);
    }

    /// A file deleted beneath a live session degrades the batch to a
    /// whole-file rewrite (header + history + batch) — the single-entry
    /// path's recovery, never a panic and never a headerless zombie.
    #[tokio::test]
    async fn append_entries_recovers_an_externally_deleted_file() {
        use crate::session::Session;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let storage = JsonlSessionStorage::create(&path, meta()).await.unwrap();
        storage
            .append_entry(&message_entry("m1", None, "history"))
            .await
            .unwrap();
        std::fs::remove_file(&path).unwrap();

        let batch = vec![
            message_entry("b1", Some("m1"), "batch one"),
            message_entry("b2", Some("b1"), "batch two"),
        ];
        storage.append_entries(&batch).await.unwrap();

        // The rewrite recovered a complete file: header present, history
        // and batch on one chain.
        let reopened = JsonlSessionStorage::open(&path).await.unwrap();
        let session = Session::new(reopened);
        let branch = session.get_branch().await.unwrap();
        let texts: Vec<&str> = branch.iter().map(user_text).collect();
        assert_eq!(texts, vec!["history", "batch one", "batch two"]);
    }

    /// The text of a user message entry, for chain-order assertions.
    fn user_text(entry: &SessionTreeEntry) -> &str {
        match entry {
            SessionTreeEntry::Message {
                message: AgentMessage::User { content, .. },
                ..
            } => content
                .iter()
                .find_map(|b| match b {
                    crate::types::ContentBlock::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .unwrap_or(""),
            _ => "",
        }
    }

    /// The recoverable damage the pre-fence write race left behind (a real
    /// session's shape: the seq-2 child line landed before the seq-1
    /// parent, both timestamped within the same millisecond burst). Load
    /// tolerates the inversion — memory takes the chain order, the cursor
    /// is the chain tail — and the next append lays the chain order down on
    /// disk (the lazy heal).
    #[tokio::test]
    async fn an_inverted_child_parent_pair_loads_and_self_heals_on_append() {
        use crate::session::Session;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let contents = concat!(
            r#"{"type":"session","version":4,"id":"s1","timestamp":"2026-09-11T08:25:14.336062Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"message","id":"x","parentId":null,"timestamp":"2026-09-11T08:25:20.000Z","seq":0,"message":{"role":"user","content":[{"type":"text","text":"x"}],"timestamp":1770000000000}}"#,
            "\n",
            r#"{"type":"message","id":"c1","parentId":"p","timestamp":"2026-09-11T08:41:34.789962Z","seq":2,"message":{"role":"user","content":[{"type":"text","text":"c1"}],"timestamp":1770000000000}}"#,
            "\n",
            r#"{"type":"message","id":"p","parentId":"x","timestamp":"2026-09-11T08:41:34.789225Z","seq":1,"message":{"role":"user","content":[{"type":"text","text":"p"}],"timestamp":1770000000000}}"#,
            "\n",
            r#"{"type":"message","id":"c2","parentId":"c1","timestamp":"2026-09-11T08:41:34.827432Z","seq":3,"message":{"role":"user","content":[{"type":"text","text":"c2"}],"timestamp":1770000000000}}"#,
            "\n",
        );
        tokio::fs::write(&path, contents).await.unwrap();

        // The damaged file loads; the branch reads back in CHAIN order and
        // the cursor is the chain tail.
        let session = std::sync::Arc::new(Session::new(
            JsonlSessionStorage::open(&path).await.unwrap(),
        ));
        let branch = session.get_branch().await.unwrap();
        let texts: Vec<&str> = branch.iter().map(user_text).collect();
        assert_eq!(texts, vec!["x", "p", "c1", "c2"], "{branch:?}");

        // The lazy heal: the next append rewrites the file in chain order.
        session
            .append_message(AgentMessage::user("tail"))
            .await
            .unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let mut positions: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut seqs: Vec<u64> = Vec::new();
        for (index, line) in text.lines().enumerate().skip(1) {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            positions.insert(value["id"].as_str().unwrap().to_string(), index);
            seqs.push(value["seq"].as_u64().expect("healed file is v4"));
        }
        assert_eq!(seqs, vec![0, 1, 2, 3, 4], "dense seqs along the chain");
        for line in text.lines().skip(1) {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            if let Some(parent) = value["parentId"].as_str() {
                let own = positions[value["id"].as_str().unwrap()];
                assert!(
                    positions[parent] < own,
                    "every parent precedes its child after the heal"
                );
            }
        }
        // The healed file reopens clean.
        JsonlSessionStorage::open(&path).await.unwrap();
    }

    /// The same inversion tolerance on a v3 file (no seq on the wire — the
    /// chain itself is the only truth, and it is intact).
    #[tokio::test]
    async fn a_v3_inverted_pair_loads() {
        use crate::session::Session;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let contents = concat!(
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-08-21T16:10:51.000Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"message","id":"x","parentId":null,"timestamp":"2026-08-21T16:10:51.000Z","message":{"role":"user","content":[{"type":"text","text":"x"}],"timestamp":1770000000000}}"#,
            "\n",
            r#"{"type":"message","id":"c","parentId":"p","timestamp":"2026-08-21T16:10:52.000Z","message":{"role":"user","content":[{"type":"text","text":"c"}],"timestamp":1770000000000}}"#,
            "\n",
            r#"{"type":"message","id":"p","parentId":"x","timestamp":"2026-08-21T16:10:51.900Z","message":{"role":"user","content":[{"type":"text","text":"p"}],"timestamp":1770000000000}}"#,
            "\n",
        );
        tokio::fs::write(&path, contents).await.unwrap();

        let session = Session::new(JsonlSessionStorage::open(&path).await.unwrap());
        let branch = session.get_branch().await.unwrap();
        let texts: Vec<&str> = branch.iter().map(user_text).collect();
        assert_eq!(texts, vec!["x", "p", "c"], "{branch:?}");
    }

    /// A parent cycle is corruption, not a recoverable ordering artifact —
    /// the global depth walk must reject it rather than loop.
    #[tokio::test]
    async fn a_parent_cycle_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let contents = concat!(
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"message","id":"m1","parentId":"m2","timestamp":"2026-05-28T07:14:00.000Z","message":{"role":"user","content":[{"type":"text","text":"one"}],"timestamp":1779952440000}}"#,
            "\n",
            r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-05-28T07:14:10.000Z","message":{"role":"user","content":[{"type":"text","text":"two"}],"timestamp":1779952450000}}"#,
            "\n",
        );
        tokio::fs::write(&path, contents).await.unwrap();
        let err = open_err(&path).await;
        assert!(err.contains("cycle"), "the error names the cycle: {err}");
    }

    /// Two storage instances over one file (the cold-append shape) append
    /// through the write fence: each sees the other's growth (the
    /// stale-view reload) and re-parents onto the CURRENT tail, so the file
    /// holds one linear chain — no fork, no lost line, no child line before
    /// its parent.
    #[tokio::test]
    async fn two_storage_instances_append_one_linear_chain_under_the_fence() {
        use crate::session::Session;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let a = std::sync::Arc::new(Session::new(
            JsonlSessionStorage::create(&path, meta()).await.unwrap(),
        ));
        let b = std::sync::Arc::new(Session::new(
            JsonlSessionStorage::open(&path).await.unwrap(),
        ));

        a.append_message(AgentMessage::user("a1")).await.unwrap();
        b.append_message(AgentMessage::user("b1")).await.unwrap();
        a.append_message(AgentMessage::user("a2")).await.unwrap();
        b.append_message(AgentMessage::user("b2")).await.unwrap();

        let reopened = JsonlSessionStorage::open(&path).await.unwrap();
        let session = Session::new(reopened);
        let branch = session.get_branch().await.unwrap();
        let texts: Vec<&str> = branch.iter().map(user_text).collect();
        assert_eq!(
            texts,
            vec!["a1", "b1", "a2", "b2"],
            "one linear chain in write order: {branch:?}"
        );
        for pair in branch.windows(2) {
            assert_eq!(
                pair[1].parent_id(),
                Some(pair[0].id()),
                "each entry parents onto its predecessor: {branch:?}"
            );
        }
    }

    /// Concurrent appends must chain onto each other, never fork sibling
    /// branches: the session serializes parent-selection + append, so the
    /// second append's parent is the first's id (upstream 4488ad55c).
    #[tokio::test]
    async fn test_concurrent_appends_form_a_chain_not_siblings() {
        use crate::session::Session;

        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
            .await
            .unwrap();
        let session = std::sync::Arc::new(Session::new(storage));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let session = std::sync::Arc::clone(&session);
            handles.push(tokio::spawn(async move {
                session
                    .append_message(AgentMessage::user("concurrent"))
                    .await
            }));
        }
        let mut ids = Vec::new();
        for handle in handles {
            ids.push(handle.await.unwrap().unwrap());
        }

        // All eight entries sit on one path — each append parented onto the
        // previous one rather than onto a stale leaf. The chain is asserted
        // structurally (whatever order the lock granted), and the branch is
        // exactly the set the spawned appends returned.
        let branch = session.get_branch().await.unwrap();
        assert_eq!(branch.len(), 8, "{branch:?}");
        for pair in branch.windows(2) {
            assert_eq!(
                pair[1].parent_id(),
                Some(pair[0].id()),
                "each entry parents onto its predecessor: {branch:?}"
            );
        }
        let mut branch_ids: Vec<&str> = branch.iter().map(|e| e.id()).collect();
        branch_ids.sort_unstable();
        let mut returned_ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        returned_ids.sort_unstable();
        assert_eq!(branch_ids, returned_ids);
    }

    /// A file whose entries repeat an id is rejected on load — the walk index
    /// would otherwise silently overwrite one entry with the other.
    #[tokio::test]
    async fn test_load_rejects_duplicate_entry_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let contents = concat!(
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-05-28T07:14:00.000Z","message":{"role":"user","content":[{"type":"text","text":"one"}],"timestamp":1779952440000}}"#,
            "\n",
            r#"{"type":"message","id":"m1","parentId":"m1","timestamp":"2026-05-28T07:14:10.000Z","message":{"role":"user","content":[{"type":"text","text":"two"}],"timestamp":1779952450000}}"#,
            "\n",
        );
        tokio::fs::write(&path, contents).await.unwrap();
        let err = JsonlSessionStorage::open(&path)
            .await
            .err()
            .expect("open must fail");
        assert!(err.to_string().contains("duplicate entry id m1"), "{err}");
    }

    /// A direct append with a repeated id is refused before touching disk.
    #[tokio::test]
    async fn test_append_rejects_duplicate_entry_id() {
        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
            .await
            .unwrap();
        let entry = SessionTreeEntry::Message {
            id: "m1".into(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("first"),
            origin: None,
        };
        storage.append_entry(&entry).await.unwrap();
        let dup = SessionTreeEntry::Message {
            id: "m1".into(),
            parent_id: Some("m1".into()),
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("second"),
            origin: None,
        };
        let err = storage.append_entry(&dup).await.unwrap_err();
        assert!(err.to_string().contains("duplicate entry id m1"), "{err}");
        // The rejected entry left no trace: the file and index hold one entry.
        assert_eq!(
            storage.get_entries(Default::default()).await.unwrap().len(),
            1
        );
        assert_eq!(storage.get_leaf_id().await.unwrap().as_deref(), Some("m1"));
    }

    /// An entry with an empty id is refused on append.
    #[tokio::test]
    async fn test_append_rejects_empty_entry_id() {
        let dir = tempfile::tempdir().unwrap();
        let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
            .await
            .unwrap();
        let entry = SessionTreeEntry::Message {
            id: String::new(),
            parent_id: None,
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("bad"),
            origin: None,
        };
        let err = storage.append_entry(&entry).await.unwrap_err();
        assert!(err.to_string().contains("empty id"), "{err}");
        assert!(
            storage
                .get_entries(Default::default())
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// A header line with an empty id or cwd is corruption, not a valid
    /// session — serde would accept both as empty strings.
    #[tokio::test]
    async fn test_load_rejects_empty_header_id_or_cwd() {
        let dir = tempfile::tempdir().unwrap();
        for (field, header) in [
            (
                "id",
                r#"{"type":"session","version":3,"id":"","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
            ),
            (
                "cwd",
                r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":""}"#,
            ),
        ] {
            let path = dir.path().join(format!("bad-{field}.jsonl"));
            tokio::fs::write(&path, format!("{header}\n"))
                .await
                .unwrap();
            let err = JsonlSessionStorage::open(&path)
                .await
                .err()
                .expect("open must fail");
            assert!(
                err.to_string().contains("session header is missing"),
                "{field}: {err}"
            );
        }
    }

    /// An entry without a `parentId` field must not be silently read as a
    /// root node — the field has to be present as `null|string`.
    #[tokio::test]
    async fn test_load_rejects_entry_missing_parent_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let contents = concat!(
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"message","id":"m1","timestamp":"2026-05-28T07:14:00.000Z","message":{"role":"user","content":[{"type":"text","text":"one"}],"timestamp":1779952440000}}"#,
            "\n",
        );
        tokio::fs::write(&path, contents).await.unwrap();
        let err = JsonlSessionStorage::open(&path)
            .await
            .err()
            .expect("open must fail");
        assert!(err.to_string().contains("invalid parentId"), "{err}");
    }

    /// A `leaf` entry without a `targetId` must not silently clear the
    /// cursor — the field has to be present as `null|string`.
    #[tokio::test]
    async fn test_load_rejects_leaf_missing_target_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let contents = concat!(
            r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
            "\n",
            r#"{"type":"leaf","id":"leaf1","parentId":null,"timestamp":"2026-05-28T07:14:00.000Z"}"#,
            "\n",
        );
        tokio::fs::write(&path, contents).await.unwrap();
        let err = JsonlSessionStorage::open(&path)
            .await
            .err()
            .expect("open must fail");
        assert!(err.to_string().contains("invalid targetId"), "{err}");
    }

    /// A present-but-null `metadata` or `parentSession` is corruption — the
    /// distinction serde's `Option` cannot make, so the wire validator has to.
    #[tokio::test]
    async fn test_load_rejects_null_metadata_and_parent_session() {
        let dir = tempfile::tempdir().unwrap();
        for (field, header) in [
            (
                "metadata",
                r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj","metadata":null}"#,
            ),
            (
                "parentSession",
                r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj","parentSession":null}"#,
            ),
        ] {
            let path = dir.path().join(format!("bad-{field}.jsonl"));
            tokio::fs::write(&path, format!("{header}\n"))
                .await
                .unwrap();
            let err = JsonlSessionStorage::open(&path)
                .await
                .err()
                .expect("open must fail");
            assert!(err.to_string().contains(field), "{field}: {err}");
        }
    }

    /// `create` never writes a header its own `open` would reject: an empty
    /// id or cwd surfaces at creation time, not on the next restart.
    #[tokio::test]
    async fn test_create_rejects_empty_id_or_cwd() {
        let dir = tempfile::tempdir().unwrap();
        for (field, value) in [("id", String::new()), ("cwd", String::new())] {
            let mut m = meta();
            if field == "id" {
                m.id = value;
            } else {
                m.cwd = value;
            }
            let err = JsonlSessionStorage::create(&dir.path().join(format!("{field}.jsonl")), m)
                .await
                .err()
                .expect("create must fail");
            assert!(err.to_string().contains("missing"), "{field}: {err}");
        }
    }

    // ── v4 journal semantics (§C.1, L4/L5) ────────────────────────────────
    mod v4 {
        use super::*;

        fn user_message(id: &str, parent: Option<&str>, text: &str) -> SessionTreeEntry {
            SessionTreeEntry::Message {
                id: id.into(),
                parent_id: parent.map(str::to_string),
                timestamp: chrono::Utc::now(),
                message: AgentMessage::user(text),
                origin: None,
            }
        }

        fn turn_start(id: &str, parent: &str) -> SessionTreeEntry {
            SessionTreeEntry::TurnStart {
                id: id.into(),
                parent_id: Some(parent.into()),
                timestamp: chrono::Utc::now(),
            }
        }

        fn tool_call(id: &str, parent: &str, call_id: &str) -> SessionTreeEntry {
            SessionTreeEntry::ToolCall {
                id: id.into(),
                parent_id: Some(parent.into()),
                timestamp: chrono::Utc::now(),
                call_id: call_id.into(),
                name: "Bash".into(),
                title: "run ls".into(),
                status: "running".into(),
                input: None,
            }
        }

        #[tokio::test]
        async fn round_trip_dense_seq_cursor_and_range() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.jsonl");
            let storage = JsonlSessionStorage::create(&path, meta()).await.unwrap();
            let mut rx = storage.subscribe_journal();

            storage
                .append_entry(&user_message("m1", None, "one"))
                .await
                .unwrap();
            storage.append_entry(&turn_start("t1", "m1")).await.unwrap();
            storage
                .append_entry(&tool_call("c1", "t1", "call-9"))
                .await
                .unwrap();

            // Broadcast delivered in strict seq order while appending.
            let seqs: Vec<u64> = {
                let mut got = Vec::new();
                for _ in 0..3 {
                    got.push(rx.recv().await.unwrap().seq);
                }
                got
            };
            assert_eq!(seqs, vec![0, 1, 2]);

            assert_eq!(storage.journal_cursor().await, 2);
            let range = storage.journal_range(0, u64::MAX).await.unwrap();
            assert_eq!(range.len(), 3);
            assert_eq!(range[0].seq, 0);
            assert_eq!(range[2].seq, 2);
            // Envelope-key exclusivity (§C.1): the tool handle rides as
            // callId, never as id.
            assert!(
                matches!(&range[2].entry, SessionTreeEntry::ToolCall { call_id, .. } if call_id == "call-9")
            );

            // Reopen: v4 header, dense stamped lines, same chain.
            drop(storage);
            let reopened = JsonlSessionStorage::open(&path).await.unwrap();
            assert_eq!(reopened.journal_cursor().await, 2);
            let reread = reopened.journal_range(0, u64::MAX).await.unwrap();
            assert_eq!(reread.len(), 3);
            assert_eq!(reread[1].entry.id(), "t1");
        }

        #[tokio::test]
        async fn v3_backfills_on_open_and_rewrites_on_first_append() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.jsonl");
            let contents = concat!(
                r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
                "\n",
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-05-28T07:14:00.000Z","message":{"role":"user","content":[{"type":"text","text":"one"}],"timestamp":1779952440000}}"#,
                "\n",
                r#"{"type":"message","id":"m2","parentId":"m1","timestamp":"2026-05-28T07:14:10.000Z","message":{"role":"user","content":[{"type":"text","text":"two"}],"timestamp":1779952450000}}"#,
                "\n",
            );
            tokio::fs::write(&path, contents).await.unwrap();

            // v3 opens with backfilled seqs; nothing is rewritten yet.
            let storage = JsonlSessionStorage::open(&path).await.unwrap();
            assert_eq!(storage.journal_cursor().await, 1);
            assert_eq!(storage.journal_range(0, 0).await.unwrap().len(), 1);
            let before = tokio::fs::read_to_string(&path).await.unwrap();
            assert!(before.contains("\"version\":3"));

            // First append lazily rewrites the whole file as v4.
            storage
                .append_entry(&user_message("m3", Some("m2"), "three"))
                .await
                .unwrap();
            let after = tokio::fs::read_to_string(&path).await.unwrap();
            assert!(after.contains("\"version\":4"));
            for (want, line) in after.lines().skip(1).enumerate() {
                let value: JsonValue = serde_json::from_str(line).unwrap();
                assert_eq!(
                    value.get("seq").and_then(JsonValue::as_u64),
                    Some(want as u64),
                    "line {want} carries its dense seq: {line}"
                );
            }

            drop(storage);
            let reopened = JsonlSessionStorage::open(&path).await.unwrap();
            assert_eq!(reopened.journal_cursor().await, 2);
        }

        /// K7 regression: a whole-file rewrite (here the lazy v3→v4
        /// migration) must be an atomic replace. A concurrent reader — the
        /// sidebar scan, an ecosystem tool reading the journal, a follow
        /// cold read — samples the path in a tight loop across the rewrite
        /// and must only ever observe a complete file: intact v3 before,
        /// dense v4 after. The reader samples `stat` lengths in a tight
        /// loop — a microsecond period, far below any plausible write
        /// window — so the former truncate+write is caught exposing every
        /// intermediate length (starting at 0), while an atomic rename
        /// only ever exposes the two legal ones.
        #[tokio::test]
        async fn concurrent_reader_never_observes_a_torn_rewrite() {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::{Arc, Mutex as StdMutex};

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.jsonl");

            // A v3 chain big enough that the rewrite's write phase is a
            // window a tight reader loop reliably samples.
            const CHAIN: u32 = 20000;
            let mut contents = String::from(
                "{\"type\":\"session\",\"version\":3,\"id\":\"s1\",\"timestamp\":\"2026-05-28T07:13:46.608Z\",\"cwd\":\"/proj\"}\n",
            );
            for i in 1..=CHAIN {
                let parent = if i == 1 {
                    "null".to_string()
                } else {
                    format!("\"m{}\"", i - 1)
                };
                contents.push_str(&format!(
                    "{{\"type\":\"message\",\"id\":\"m{i}\",\"parentId\":{parent},\"timestamp\":\"2026-05-28T07:14:00.000Z\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"text\",\"text\":\"msg {i}\"}}],\"timestamp\":1779952440000}}}}\n"
                ));
            }
            tokio::fs::write(&path, &contents).await.unwrap();
            let storage = JsonlSessionStorage::open(&path).await.unwrap();

            // High-frequency integrity sampling: the reader thread stats
            // the path in a tight loop and records every distinct observed
            // file length. A complete file is exactly `len_v3` before the
            // rewrite and `len_v4` after it; truncate+write transiently
            // exposes every intermediate length (starting at 0), while an
            // atomic rename exposes only the two valid ones.
            let len_v3 = contents.len() as u64;
            let stop = Arc::new(AtomicBool::new(false));
            let observed = Arc::new(StdMutex::new(Vec::<u64>::new()));
            let reader = {
                let (path, stop, observed) = (path.clone(), stop.clone(), observed.clone());
                std::thread::spawn(move || {
                    let mut last = u64::MAX;
                    while !stop.load(Ordering::Relaxed) {
                        match std::fs::metadata(&path) {
                            Ok(md) => {
                                let len = md.len();
                                if len != last {
                                    observed.lock().unwrap().push(len);
                                    last = len;
                                }
                            }
                            // A missing path mid-existence is also a torn
                            // observation (rename replaces the target, it
                            // never removes it).
                            Err(_) => {
                                observed.lock().unwrap().push(u64::MAX);
                                break;
                            }
                        }
                    }
                })
            };

            // The first append on a v3 file rewrites the whole chain as v4.
            storage
                .append_entry(&user_message(
                    &format!("m{}", CHAIN + 1),
                    Some(&format!("m{CHAIN}")),
                    "final",
                ))
                .await
                .unwrap();

            stop.store(true, Ordering::Relaxed);
            reader.join().unwrap();

            // Judge every observed length against the two legal file
            // states; anything else is a torn observation.
            let after = tokio::fs::read_to_string(&path).await.unwrap();
            let len_v4 = after.len() as u64;
            let torn: Vec<u64> = observed
                .lock()
                .unwrap()
                .iter()
                .copied()
                .filter(|len| *len != len_v3 && *len != len_v4)
                .collect();
            assert!(
                torn.is_empty(),
                "torn rewrite observed: intermediate lengths {torn:?} (v3={len_v3}, v4={len_v4})"
            );

            // Post-conditions: the migrated file is complete v4 with dense
            // seqs, and the atomic replace left no temp residue.
            assert!(after.contains("\"version\":4"));
            assert_eq!(after.lines().count(), CHAIN as usize + 2);
            let residue: Vec<String> = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".tmp"))
                .collect();
            assert!(
                residue.is_empty(),
                "temp residue after rewrite: {residue:?}"
            );
        }

        /// K7 companion: when the atomic replace cannot even create its
        /// sibling temp (read-only directory), the append must fail loud
        /// with the on-disk original byte-identical, and the same append
        /// must complete the migration once the directory is writable
        /// again — a failed rewrite is a no-op, never a corruption.
        #[tokio::test]
        #[cfg(unix)]
        async fn failed_rewrite_leaves_the_original_file_intact() {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.jsonl");
            let contents = concat!(
                r#"{"type":"session","version":3,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
                "\n",
                r#"{"type":"message","id":"m1","parentId":null,"timestamp":"2026-05-28T07:14:00.000Z","message":{"role":"user","content":[{"type":"text","text":"one"}],"timestamp":1779952440000}}"#,
                "\n",
            );
            tokio::fs::write(&path, contents).await.unwrap();
            let storage = JsonlSessionStorage::open(&path).await.unwrap();

            // Restore the directory mode even on a panicking assertion.
            struct PermGuard(std::path::PathBuf, u32);
            impl Drop for PermGuard {
                fn drop(&mut self) {
                    let _ =
                        std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(self.1));
                }
            }
            let original = std::fs::metadata(dir.path()).unwrap().permissions().mode();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
            let guard = PermGuard(dir.path().to_path_buf(), original);

            // Root ignores directory permissions; the fence would be inert,
            // so skip rather than assert on a false setup.
            if std::fs::File::create(dir.path().join("probe")).is_ok() {
                let _ = std::fs::remove_file(dir.path().join("probe"));
                eprintln!("skipping: running with write access despite 0o500 (root?)");
                return;
            }

            let err = storage
                .append_entry(&user_message("m2", Some("m1"), "two"))
                .await
                .expect_err("rewrite into a read-only directory must fail");
            assert!(
                err.to_string().contains("jsonl.tmp")
                    || std::fs::read_to_string(&path)
                        .unwrap()
                        .contains("\"version\":3"),
                "failure names the temp write or the original survives: {err}"
            );
            drop(guard);

            // The v3 original is byte-identical, and recovery is a plain
            // retry: the same append now completes the migration with no
            // temp residue.
            assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), contents);
            storage
                .append_entry(&user_message("m2", Some("m1"), "two"))
                .await
                .unwrap();
            let migrated = tokio::fs::read_to_string(&path).await.unwrap();
            assert!(migrated.contains("\"version\":4"));
            let residue: Vec<String> = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".tmp"))
                .collect();
            assert!(
                residue.is_empty(),
                "temp residue after recovery: {residue:?}"
            );
        }

        #[tokio::test]
        async fn v4_stored_seq_mismatch_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.jsonl");
            let contents = concat!(
                r#"{"type":"session","version":4,"id":"s1","timestamp":"2026-05-28T07:13:46.608Z","cwd":"/proj"}"#,
                "\n",
                r#"{"type":"message","id":"m1","parentId":null,"seq":0,"timestamp":"2026-05-28T07:14:00.000Z","message":{"role":"user","content":[{"type":"text","text":"one"}],"timestamp":1779952440000}}"#,
                "\n",
                r#"{"type":"message","id":"m2","parentId":"m1","seq":7,"timestamp":"2026-05-28T07:14:10.000Z","message":{"role":"user","content":[{"type":"text","text":"two"}],"timestamp":1779952450000}}"#,
                "\n",
            );
            tokio::fs::write(&path, contents).await.unwrap();
            let err = match JsonlSessionStorage::open(&path).await {
                Err(e) => e,
                Ok(_) => panic!("v4 load must reject a stored seq that diverges from chain depth"),
            };
            assert!(
                err.to_string().contains("chain depth"),
                "the error explains the divergence: {err}"
            );
        }

        #[tokio::test]
        async fn branch_shares_prefix_and_stays_dense_along_active_chain() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.jsonl");
            let storage = JsonlSessionStorage::create(&path, meta()).await.unwrap();
            storage
                .append_entry(&user_message("m1", None, "one"))
                .await
                .unwrap();
            storage
                .append_entry(&user_message("m2", Some("m1"), "two"))
                .await
                .unwrap();

            // Branch: cursor back to m1, then extend with m3 (parent m1).
            storage.set_leaf_id(Some("m1")).await.unwrap();
            storage
                .append_entry(&user_message("m3", Some("m1"), "three"))
                .await
                .unwrap();

            // The active chain is m1 → m3, dense 0..1; m2 keeps its own
            // branch-local seq (1) but is off the active chain.
            assert_eq!(storage.journal_cursor().await, 1);
            let chain = storage.journal_range(0, u64::MAX).await.unwrap();
            assert_eq!(
                chain.iter().map(|r| r.entry.id()).collect::<Vec<_>>(),
                vec!["m1", "m3"]
            );
            assert_eq!(chain.iter().map(|r| r.seq).collect::<Vec<_>>(), vec![0, 1]);
        }

        #[tokio::test]
        async fn range_is_inclusive_and_clamped() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("session.jsonl");
            let storage = JsonlSessionStorage::create(&path, meta()).await.unwrap();
            for i in 0..5 {
                let parent = if i == 0 { None } else { Some(format!("m{i}")) };
                let parent = parent.as_deref();
                storage
                    .append_entry(&user_message(&format!("m{}", i + 1), parent, "x"))
                    .await
                    .unwrap();
            }
            let mid = storage.journal_range(2, 3).await.unwrap();
            assert_eq!(
                mid.iter().map(|r| r.entry.id()).collect::<Vec<_>>(),
                vec!["m3", "m4"]
            );
            // An out-of-range tail clamps to the chain end, never errors.
            let tail = storage.journal_range(9, 99).await.unwrap();
            assert!(tail.is_empty());
        }

        #[tokio::test]
        async fn append_with_unknown_parent_is_refused() {
            let dir = tempfile::tempdir().unwrap();
            let storage = JsonlSessionStorage::create(&dir.path().join("session.jsonl"), meta())
                .await
                .unwrap();
            let err = storage
                .append_entry(&user_message("m1", Some("ghost"), "one"))
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("ghost"),
                "the append names the unknown parent: {err}"
            );
        }
    }

    // ── origin / echo-retirement field (T5b, §C.2 originRpc / §F.2) ────────

    /// `append_message_with_origin` survives a disk round-trip: the pinned
    /// origin comes back on reopen, while a plain `append_message` still reads
    /// `None`.
    #[tokio::test]
    async fn append_message_with_origin_round_trips_through_disk() {
        use crate::session::Session;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let storage = JsonlSessionStorage::create(&path, meta()).await.unwrap();
        let session = Session::new(storage);

        session
            .append_message_with_origin(AgentMessage::user("echo me"), Some("rpc-42".into()))
            .await
            .unwrap();
        session
            .append_message(AgentMessage::user("no origin"))
            .await
            .unwrap();

        let before: Vec<Option<String>> = session
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                SessionTreeEntry::Message { origin, .. } => Some(origin.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            before,
            vec![Some("rpc-42".to_string()), None],
            "origin is visible before reopen"
        );

        drop(session);
        let reopened = Session::new(JsonlSessionStorage::open(&path).await.unwrap());
        let after: Vec<Option<String>> = reopened
            .storage()
            .get_entries(Default::default())
            .await
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                SessionTreeEntry::Message { origin, .. } => Some(origin.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            after,
            vec![Some("rpc-42".to_string()), None],
            "the pinned origin survives a reopen; the plain append stays None"
        );
    }

    /// Message lines written without an `origin` key — the pre-T5b v3 and v4
    /// wire forms, including a v3/v4 mixed sample — deserialize to
    /// `origin: None` (the field defaults), and the origin is only on disk when
    /// present (skip-serializing).
    #[tokio::test]
    async fn message_lines_without_origin_key_deserialize_to_none() {
        let ts = "2020-01-01T00:00:01Z";
        let header = |version: u32, id: &str| {
            format!(
                r#"{{"type":"session","version":{version},"id":"{id}","cwd":"/t","timestamp":"{ts}"}}"#
            )
        };
        // Build a message line from a real serialized `AgentMessage`, pinned in
        // the v3/v4 envelope, deliberately omitting the `origin` key (the
        // pre-T5b wire form). `message` round-trips whatever the current
        // `AgentMessage` repr is, so the sample cannot drift from the schema.
        let message_line = |seq: Option<u64>,
                            id: &str,
                            parent: Option<&str>,
                            msg: AgentMessage|
         -> String {
            let parent_json = match parent {
                Some(p) => format!(r#""{p}""#),
                None => "null".to_string(),
            };
            let seq_json = match seq {
                Some(s) => format!(r#""seq":{s},"#),
                None => String::new(),
            };
            let body = serde_json::to_string(&msg).expect("AgentMessage serializes");
            format!(
                r#"{{"type":"message",{seq_json}"id":"{id}","parentId":{parent_json},"timestamp":"{ts}","message":{body}}}"#
            )
        };

        // A v3 file: header version 3 (no per-entry seq) and a user line.
        let v3 = vec![
            header(3, "s3"),
            message_line(None, "m1", None, AgentMessage::user("old")),
        ];
        // A v4 file: header version 4 and two message lines — a v3-style user
        // line and a richer assistant line — neither carrying `origin`.
        let assistant = AgentMessage::Assistant {
            content: vec![crate::types::ContentBlock::Text {
                text: "b".into(),
                signature: None,
            }],
            model: "m".into(),
            provider: "anthropic".into(),
            api: "anthropic".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            raw_stop_reason: None,
            stop_reason: Some(crate::types::StopReason::Stop),
            usage: Box::default(),
            error_message: None,
            timestamp: chrono::Utc::now(),
        };
        let v4 = vec![
            header(4, "s4"),
            message_line(Some(0), "m1", None, AgentMessage::user("a")),
            message_line(Some(1), "m2", Some("m1"), assistant),
        ];

        let dir = tempfile::tempdir().unwrap();
        for (name, lines) in [("legacy_v3.jsonl", v3), ("legacy_v4.jsonl", v4)] {
            let path = dir.path().join(name);
            // Assert the raw sample genuinely has no origin key before writing.
            assert!(
                !lines[1..].iter().any(|l| l.contains("\"origin\"")),
                "sample {name} must not carry an origin key"
            );
            tokio::fs::write(&path, (lines.join("\n") + "\n").as_bytes())
                .await
                .unwrap();
            let storage = JsonlSessionStorage::open(&path).await.unwrap();
            let origins: Vec<Option<String>> = storage
                .get_entries(Default::default())
                .await
                .unwrap()
                .iter()
                .filter_map(|e| match e {
                    SessionTreeEntry::Message { origin, .. } => Some(origin.clone()),
                    _ => None,
                })
                .collect();
            assert!(
                !origins.is_empty(),
                "{name} should have parsed at least one message"
            );
            assert!(
                origins.iter().all(|o| o.is_none()),
                "{name} message lines without an origin key must read None: {origins:?}"
            );
        }
    }
}

#[cfg(test)]
mod deferred_probe_tests {
    use super::*;

    #[tokio::test]
    async fn deferred_session_never_touches_disk_before_assistant_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let storage = JsonlSessionStorage::create_deferred(
            &path,
            JsonlSessionMetadata {
                id: "s1".into(),
                cwd: "/t".into(),
                created_at: chrono::Utc::now(),
                parent_session_path: None,
                metadata: None,
            },
        )
        .await
        .unwrap();
        storage
            .append_entry(&SessionTreeEntry::ModelChange {
                id: "m0".into(),
                parent_id: None,
                timestamp: chrono::Utc::now(),
                provider: "p".into(),
                model_id: "m".into(),
            })
            .await
            .unwrap();
        assert!(
            !path.exists(),
            "a deferred session with only non-assistant rows must not materialize"
        );
    }
}

#[cfg(test)]
mod append_entries_tests {
    use super::*;

    fn batch_meta() -> JsonlSessionMetadata {
        JsonlSessionMetadata {
            id: "batch".into(),
            cwd: "/tmp".into(),
            created_at: chrono::Utc::now(),
            parent_session_path: None,
            metadata: None,
        }
    }

    fn chain_entry(id: &str, parent: Option<&str>) -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: id.into(),
            parent_id: parent.map(str::to_string),
            timestamp: chrono::Utc::now(),
            message: crate::types::AgentMessage::user("batch"),
            origin: None,
        }
    }

    fn rows(path: &Path) -> String {
        // Header timestamps differ across two create() instants — compare
        // everything AFTER line 0.
        let text = std::fs::read_to_string(path).unwrap();
        text.lines().skip(1).collect::<Vec<_>>().join("\n") + "\n"
    }

    #[tokio::test]
    async fn append_entries_matches_per_row_appends_row_for_row() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let a = JsonlSessionStorage::create(&dir_a.path().join("s.jsonl"), batch_meta())
            .await
            .unwrap();
        let b = JsonlSessionStorage::create(&dir_b.path().join("s.jsonl"), batch_meta())
            .await
            .unwrap();
        let chain = vec![chain_entry("e0", None), chain_entry("e1", Some("e0"))];

        for e in &chain {
            a.append_entry(e).await.unwrap();
        }
        b.append_entries(&chain).await.unwrap();

        // Bounded re-read: under extreme cross-binary load a just-awaited
        // write can lag a synchronous read on some filesystems; the retry
        // window tolerates that without weakening the equality intent.
        let (rows_a, rows_b) = {
            let pa = dir_a.path().join("s.jsonl");
            let pb = dir_b.path().join("s.jsonl");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                let pair = (rows(&pa), rows(&pb));
                if pair.0 == pair.1 || std::time::Instant::now() > deadline {
                    break pair;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        assert_eq!(
            rows_a, rows_b,
            "batched and per-row appends write identical rows (seqs included)"
        );
        assert_eq!(
            a.journal_cursor().await,
            b.journal_cursor().await,
            "the cursor lands on the same leaf"
        );
        let ra = a.journal_range(0, u64::MAX).await.unwrap();
        let rb = b.journal_range(0, u64::MAX).await.unwrap();
        assert_eq!(ra.len(), rb.len());
        assert_eq!(ra.last().map(|r| r.seq), rb.last().map(|r| r.seq));
    }

    #[tokio::test]
    async fn append_entries_rejects_the_whole_batch_atomically() {
        // Duplicate id inside the batch → nothing touches disk.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let s = JsonlSessionStorage::create(&path, batch_meta())
            .await
            .unwrap();
        let before = tokio::fs::read_to_string(&path).await.unwrap();
        let dup = vec![chain_entry("e0", None), chain_entry("e0", None)];
        let err = s.append_entries(&dup).await.unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(before, after, "a rejected batch writes nothing");

        // Unknown parent mid-batch → still nothing touches disk.
        let broken = vec![chain_entry("e0", None), chain_entry("e2", Some("nope"))];
        let err = s.append_entries(&broken).await.unwrap_err();
        assert!(err.to_string().contains("unknown parent"), "{err}");
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(before, after);
        assert_eq!(
            s.journal_cursor().await,
            0,
            "the in-memory state is untouched too"
        );
    }
}
