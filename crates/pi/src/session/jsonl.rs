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
use tokio::sync::Mutex;

use crate::session::{SessionStorage, SessionTreeEntry};
use serde_json::Value as JsonValue;

/// Current on-disk session format version.
const FORMAT_VERSION: u32 = 3;

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
    /// Metadata read from the header (file is authoritative on reopen).
    pub metadata: JsonlSessionMetadata,
}

impl JsonlSessionStorage {
    /// Create a new session file at `path`, writing `metadata` as the header.
    ///
    /// The path is the exact file location — the caller owns the naming scheme
    /// (the TS Pi repo writes `timestamp_sessionId.jsonl` under a per-cwd
    /// directory). A missing parent directory is created. This errors if the
    /// file already exists; reopen an existing file with [`open`].
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
        let line = serde_json::to_string(&header)? + "\n";
        tokio::fs::write(path, line).await?;
        Ok(JsonlSessionStorage {
            jsonl_path: path.to_path_buf(),
            entries: Mutex::new(Vec::new()),
            leaf_id: Mutex::new(None),
            metadata,
        })
    }

    /// Open an existing session file at `path`.
    ///
    /// The file must exist and begin with a valid v3 session header; otherwise
    /// this errors rather than guessing at a repair. Unlike [`create`], a
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
        let header: SessionHeader = serde_json::from_str(&header_line)
            .map_err(|e| anyhow::anyhow!("invalid session header: {e}"))?;
        if header.type_tag != "session" {
            anyhow::bail!(
                "session file first line is not a session header (type=\"{}\")",
                header.type_tag
            );
        }
        if header.version != FORMAT_VERSION {
            anyhow::bail!(
                "session file version {} is unsupported (expected {})",
                header.version,
                FORMAT_VERSION
            );
        }

        let metadata = JsonlSessionMetadata {
            id: header.id,
            cwd: header.cwd,
            created_at: header.timestamp,
            parent_session_path: header.parent_session,
            metadata: header.metadata,
        };

        let mut entries = Vec::new();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let entry: SessionTreeEntry = serde_json::from_str(&line)?;
            entries.push(entry);
        }
        // The cursor follows the last entry: a trailing `leaf` entry
        // redirects to its `targetId`, otherwise the last entry's own id.
        let leaf_id = entries.last().and_then(SessionTreeEntry::leaf_cursor_after);

        Ok(JsonlSessionStorage {
            jsonl_path: path.to_path_buf(),
            entries: Mutex::new(entries),
            leaf_id: Mutex::new(leaf_id),
            metadata,
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
}

#[async_trait::async_trait]
impl SessionStorage for JsonlSessionStorage {
    async fn create_entry_id(&self) -> Result<String, anyhow::Error> {
        Ok(uuid::Uuid::new_v4().to_string())
    }

    async fn append_entry(&self, entry: &SessionTreeEntry) -> Result<(), anyhow::Error> {
        let line = serde_json::to_string(entry)? + "\n";
        self.append_line(&line).await?;
        // Index the entry before moving the cursor, mirroring TS Pi's order:
        // a concurrent `get_leaf_id` must never see a cursor whose target is
        // absent from the index, which would read as session corruption.
        self.entries.lock().await.push(entry.clone());
        // The cursor follows this entry: a `leaf` entry redirects to its
        // `targetId`, otherwise the entry becomes the cursor itself.
        *self.leaf_id.lock().await = entry.leaf_cursor_after();
        Ok(())
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
        self.append_entry(&entry).await
    }

    async fn get_entries(&self) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        Ok(self.entries.lock().await.clone())
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
        };
        storage.append_entry(&entry).await.unwrap();

        let fetched = storage.get_entry("test-1").await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().id(), "test-1");

        let all = storage.get_entries().await.unwrap();
        assert_eq!(all.len(), 1);
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
            };
            storage.append_entry(&msg).await.unwrap();
            // No `leaf` entry exists; the cursor is the last appended entry.
        }

        let storage = JsonlSessionStorage::open(&path).await.unwrap();
        let entries = storage.get_entries().await.unwrap();
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
        assert!(header_line.contains("\"version\":3"));
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
        assert!(err.contains("version 2 is unsupported"), "{err}");
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
        };
        let child = SessionTreeEntry::Message {
            id: "child".into(),
            parent_id: Some("root".into()),
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("child"),
        };
        let leaf = SessionTreeEntry::Message {
            id: "leaf".into(),
            parent_id: Some("child".into()),
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("leaf"),
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
        };

        let root = SessionTreeEntry::Message {
            id: "root".into(),
            parent_id: None,
            timestamp: base,
            message: AgentMessage::user("root"),
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
        };
        let child = SessionTreeEntry::Message {
            id: "child".into(),
            parent_id: Some("root".into()),
            timestamp: chrono::Utc::now(),
            message: AgentMessage::user("child"),
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

        let entries = storage.get_entries().await.unwrap();
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
        let entries = storage.get_entries().await.unwrap();
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

        let entries = storage.get_entries().await.unwrap();
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

        let entries = session.storage().get_entries().await.unwrap();
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
        assert_eq!(reopened.get_entries().await.unwrap().len(), 3);

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

    /// A walk that cannot complete loudly fails: an explicit leaf unknown to
    /// storage, or a parent id with no entry, never yields a truncated path.
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
        let storage = JsonlSessionStorage::open(&path).await.unwrap();

        // Unknown explicit leaf.
        let err = storage.get_path(Some("no-such-entry")).await.unwrap_err();
        assert!(
            err.to_string().contains("no-such-entry"),
            "the error names the missing leaf: {err}"
        );

        // A parent id with no entry breaks the chain mid-walk.
        let err = storage.get_path(Some("m2")).await.unwrap_err();
        assert!(
            err.to_string().contains("ghost"),
            "the error names the missing parent: {err}"
        );

        // A well-formed chain still walks.
        let path = storage.get_path(Some("m1")).await.unwrap();
        assert_eq!(path.len(), 1);
    }
}
