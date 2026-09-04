// Append-only JSONL session storage (format version 3).
//
// Layout of a session file (the caller picks the path — typically a
// `timestamp_sessionId.jsonl` under a per-cwd directory, matching the TS Pi
// repo naming):
//   line 0 — a session header: `{"type":"session","version":3,"id":..,"timestamp":..,"cwd":..,"parentSession"?:..,"metadata"?:..}`.
//   line 1.. — session-tree entries, appended in occurrence order. A `leaf`
//              entry records a cursor move to an older branch point
//              (`targetId`); any other entry implicitly makes itself the
//              cursor. The leaf cursor is `targetId` for a trailing leaf
//              entry, otherwise the last entry's id. The file is strictly
//              append-only: no field is ever rewritten.
//
// `open` takes the exact file path. A missing file is created with `metadata`
// as its header; an existing file must begin with a valid v3 session header,
// otherwise this errors rather than guessing at a repair.

use std::path::{Path, PathBuf};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
    /// Path of the session this one forked from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_path: Option<String>,
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
    parent_session: Option<String>,
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
    /// under `append_lock` (L4); parents always precede children in file
    /// order, so depths are computed incrementally on load.
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
        })
    }

    /// Open an existing session file at `path`.
    ///
    /// The file must exist and begin with a valid v3 session header; otherwise
    /// this errors rather than guessing at a repair. Unlike [`Self::create`], a
    /// missing or mis-typed path surfaces as an error so a recovery path can
    /// never silently materialize an empty session.
    pub async fn open(path: &Path) -> Result<Self, anyhow::Error> {
        Self::load(path).await
    }

    async fn load(path: &Path) -> Result<Self, anyhow::Error> {
        let file = File::open(path).await?;
        let reader = BufReader::new(file);
        let mut lines = reader.lines();
        let header_line = lines
            .next_line()
            .await?
            .ok_or_else(|| anyhow::anyhow!("session file is empty (no header line)"))?;
        if header_line.trim().is_empty() {
            anyhow::bail!("session file header is blank");
        }
        let header: JsonValue = serde_json::from_str(&header_line)
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

        let mut entries = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();
        let mut seq_index = std::collections::HashMap::new();
        let v4 = header.version >= FORMAT_VERSION;
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let value: JsonValue = serde_json::from_str(&line)?;
            // Wire-level structural checks before deserializing: a missing
            // required field must not be silently read as `null` (TS
            // `parseEntryLine` treats a missing `parentId`/`targetId` as an
            // invalid entry).
            validate_entry_wire(&value)?;
            let entry: SessionTreeEntry = serde_json::from_value(value.clone())?;
            // A duplicate id would make the walk index silently overwrite one
            // entry with the other — reject the file instead of restoring a
            // wrong ancestry.
            if entry.id().is_empty() {
                anyhow::bail!("session file contains an entry with an empty id");
            }
            if !seen_ids.insert(entry.id().to_string()) {
                anyhow::bail!("duplicate entry id {} in session file", entry.id());
            }
            // §C.1 seq: chain depth computed incrementally (parents always
            // precede children in an append-only file). A v4 line must carry
            // the same value — a mismatch is corruption, not a renumbering.
            let depth = match entry.parent_id() {
                None => 0u64,
                Some(parent) => match seq_index.get(parent) {
                    Some(parent_depth) => parent_depth + 1,
                    // A parent that never appeared earlier in the file means
                    // the chain is broken; get_path would fail the same way.
                    None => anyhow::bail!(
                        "session entry {} references unknown parent {parent}",
                        entry.id()
                    ),
                },
            };
            if v4 {
                match value.get("seq").and_then(JsonValue::as_u64) {
                    Some(seq) if seq == depth => {}
                    Some(seq) => anyhow::bail!(
                        "session entry {} carries seq {seq} but its chain depth is {depth}",
                        entry.id()
                    ),
                    None => {
                        anyhow::bail!("v4 session entry {} is missing its seq field", entry.id())
                    }
                }
            }
            seq_index.insert(entry.id().to_string(), depth);
            entries.push(entry);
        }
        // The cursor follows the last entry: a trailing `leaf` entry
        // redirects to its `targetId`, otherwise the last entry's own id.
        let leaf_id = entries.last().and_then(SessionTreeEntry::leaf_cursor_after);
        let (journal_tx, _) = broadcast::channel(JOURNAL_BROADCAST_CAPACITY);

        Ok(JsonlSessionStorage {
            jsonl_path: path.to_path_buf(),
            entries: Mutex::new(entries),
            leaf_id: Mutex::new(leaf_id),
            seq_index: Mutex::new(seq_index),
            file_version: Mutex::new(header.version),
            journal_tx,
            append_lock: Mutex::new(()),
            metadata,
            deferred: Mutex::new(false),
        })
    }

    async fn append_line(&self, line: &str) -> Result<(), anyhow::Error> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.jsonl_path)
            .await?;
        file.write_all(line.as_bytes()).await?;
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
    async fn append_entry_locked(&self, entry: &SessionTreeEntry) -> Result<(), anyhow::Error> {
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
        // Single stamp point: parent must already be indexed (append-only
        // files guarantee parents precede children).
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
        let line = v4_line(entry, seq)?;
        // A deferred session materializes on the first assistant message: the
        // header plus every buffered entry are written in one shot, so the
        // on-disk order matches the in-memory index (TS `_persist`).
        let is_assistant = matches!(
            entry,
            SessionTreeEntry::Message {
                message: crate::types::AgentMessage::Assistant { .. },
                ..
            }
        );
        let mut materialized = false;
        if *self.deferred.lock().await {
            if is_assistant {
                self.rewrite_file_v4_locked(Some(&line)).await?;
                *self.deferred.lock().await = false;
                materialized = true;
            }
        } else if *self.file_version.lock().await < FORMAT_VERSION {
            // Lazy v3 → v4 migration: rewrite the whole file with stamped
            // lines, appending the new one in the same write.
            self.rewrite_file_v4_locked(Some(&line)).await?;
            materialized = true;
        }
        if !materialized {
            self.append_line(&line).await?;
        }
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
    async fn rewrite_file_v4_locked(&self, extra_line: Option<&str>) -> Result<(), anyhow::Error> {
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
        tokio::fs::write(&self.jsonl_path, content).await?;
        *self.file_version.lock().await = FORMAT_VERSION;
        Ok(())
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
            Some(id) => self.seq_index.lock().await.get(&id).copied().unwrap_or(0),
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
        self.append_entry_locked(entry).await
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
        self.append_entry_locked(&entry).await
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
            parent_session_path: Some("/sessions/parent.jsonl".into()),
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
            Some("/sessions/parent.jsonl")
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
