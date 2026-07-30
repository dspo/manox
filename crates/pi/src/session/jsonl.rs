// JSONL-based session storage.
//
// Each line in the file is a complete JSON object representing a single
// SessionTreeEntry. The file is append-only; new entries are written at
// the end. A sidecar metadata file tracks the session id, cwd, and leaf
// cursor position.

use std::path::{Path, PathBuf};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;

use crate::session::{SessionStorage, SessionTreeEntry};

/// JSONL session storage backed by a file.
pub struct JsonlSessionStorage {
    /// Path to the JSONL data file.
    jsonl_path: PathBuf,
    /// Path to the sidecar metadata file.
    meta_path: PathBuf,
    /// In-memory cache of entries (loaded on open, updated on append).
    entries: Mutex<Vec<SessionTreeEntry>>,
    /// Current leaf entry ID.
    leaf_id: Mutex<Option<String>>,
    /// Session metadata.
    pub metadata: JsonlSessionMetadata,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JsonlSessionMetadata {
    pub id: String,
    pub cwd: String,
    #[serde(default = "chrono::Utc::now")]
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl JsonlSessionStorage {
    /// Open or create a session in the given directory.
    pub async fn open(dir: &Path, metadata: JsonlSessionMetadata) -> Result<Self, anyhow::Error> {
        tokio::fs::create_dir_all(dir).await?;

        let jsonl_path = dir.join("session.jsonl");
        let meta_path = dir.join("session.meta.json");

        let (entries, leaf_id) = if jsonl_path.exists() {
            let entries = Self::load_entries(&jsonl_path).await?;
            let leaf_id = Self::load_leaf_id(&meta_path).await?;
            (entries, leaf_id)
        } else {
            // Write initial metadata.
            let meta_json = serde_json::to_string_pretty(&metadata)?;
            tokio::fs::write(&meta_path, meta_json).await?;
            (Vec::new(), None)
        };

        Ok(JsonlSessionStorage {
            jsonl_path,
            meta_path,
            entries: Mutex::new(entries),
            leaf_id: Mutex::new(leaf_id),
            metadata,
        })
    }

    async fn load_entries(path: &Path) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        let file = File::open(path).await?;
        let reader = BufReader::new(file);
        let mut entries = Vec::new();
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let entry: SessionTreeEntry = serde_json::from_str(&line)?;
            entries.push(entry);
        }
        Ok(entries)
    }

    async fn load_leaf_id(meta_path: &Path) -> Result<Option<String>, anyhow::Error> {
        if !meta_path.exists() {
            return Ok(None);
        }
        let content = tokio::fs::read_to_string(meta_path).await?;
        #[derive(serde::Deserialize)]
        struct MetaFile {
            #[serde(default)]
            leaf_id: Option<String>,
        }
        let meta: MetaFile = serde_json::from_str(&content)?;
        Ok(meta.leaf_id)
    }

    async fn save_leaf_id(&self) -> Result<(), anyhow::Error> {
        let leaf_id = self.leaf_id.lock().await;
        #[derive(serde::Serialize)]
        struct MetaFile {
            id: String,
            cwd: String,
            created_at: chrono::DateTime<chrono::Utc>,
            leaf_id: Option<String>,
        }
        let meta = MetaFile {
            id: self.metadata.id.clone(),
            cwd: self.metadata.cwd.clone(),
            created_at: self.metadata.created_at,
            leaf_id: leaf_id.clone(),
        };
        let json = serde_json::to_string_pretty(&meta)?;
        tokio::fs::write(&self.meta_path, json).await?;
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

        // Append to the JSONL file.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.jsonl_path)
            .await?;
        file.write_all(line.as_bytes()).await?;

        // Update in-memory cache.
        self.entries.lock().await.push(entry.clone());

        Ok(())
    }

    async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, anyhow::Error> {
        let entries = self.entries.lock().await;
        Ok(entries.iter().find(|e| e.id() == id).cloned())
    }

    async fn get_leaf_id(&self) -> Result<Option<String>, anyhow::Error> {
        Ok(self.leaf_id.lock().await.clone())
    }

    async fn set_leaf_id(&self, leaf_id: Option<&str>) -> Result<(), anyhow::Error> {
        *self.leaf_id.lock().await = leaf_id.map(|s| s.to_string());
        self.save_leaf_id().await?;
        Ok(())
    }

    async fn get_entries(&self) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        Ok(self.entries.lock().await.clone())
    }

    async fn get_path_to_root_or_compaction(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        let target_id = match leaf_id {
            Some(id) => id.to_string(),
            None => match self.get_leaf_id().await? {
                Some(id) => id,
                None => return Ok(Vec::new()),
            },
        };

        let entries = self.entries.lock().await;

        // Build an index: id → entry.
        let mut index: std::collections::HashMap<&str, &SessionTreeEntry> =
            entries.iter().map(|e| (e.id(), e)).collect();

        // Walk from the target leaf to root, stopping at compaction entries.
        let mut path: Vec<&SessionTreeEntry> = Vec::new();
        let mut current_id: Option<&str> = Some(&target_id);

        while let Some(id) = current_id {
            let entry = match index.remove(id) {
                Some(e) => e,
                None => break,
            };
            let is_compaction = matches!(entry, SessionTreeEntry::Compaction { .. });
            current_id = entry.parent_id();
            path.push(entry);

            if is_compaction {
                break;
            }
        }

        // Reverse to chronological order.
        path.reverse();
        Ok(path.into_iter().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AgentMessage;

    #[tokio::test]
    async fn test_jsonl_append_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let meta = JsonlSessionMetadata {
            id: uuid::Uuid::new_v4().to_string(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
        };

        let storage = JsonlSessionStorage::open(dir.path(), meta).await.unwrap();

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
        let meta = JsonlSessionMetadata {
            id: uuid::Uuid::new_v4().to_string(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
        };

        let storage = JsonlSessionStorage::open(dir.path(), meta).await.unwrap();

        assert!(storage.get_leaf_id().await.unwrap().is_none());

        storage.set_leaf_id(Some("entry-42")).await.unwrap();
        assert_eq!(storage.get_leaf_id().await.unwrap(), Some("entry-42".into()));

        storage.set_leaf_id(None).await.unwrap();
        assert!(storage.get_leaf_id().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_path_to_root() {
        let dir = tempfile::tempdir().unwrap();
        let meta = JsonlSessionMetadata {
            id: uuid::Uuid::new_v4().to_string(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
        };

        let storage = JsonlSessionStorage::open(dir.path(), meta).await.unwrap();

        // Build a chain: root -> child -> leaf
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

        let path = storage
            .get_path_to_root_or_compaction(Some("leaf"))
            .await
            .unwrap();

        assert_eq!(path.len(), 3);
        assert_eq!(path[0].id(), "root");
        assert_eq!(path[1].id(), "child");
        assert_eq!(path[2].id(), "leaf");
    }

    #[tokio::test]
    async fn test_path_stops_at_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let meta = JsonlSessionMetadata {
            id: uuid::Uuid::new_v4().to_string(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
        };

        let storage = JsonlSessionStorage::open(dir.path(), meta).await.unwrap();

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
            retained_tail: vec![AgentMessage::user("kept")],
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

        let path = storage
            .get_path_to_root_or_compaction(None)
            .await
            .unwrap();

        // Should include compaction and post, but NOT pre (stopped at compaction).
        assert_eq!(path.len(), 2);
        assert_eq!(path[0].id(), "comp");
        assert_eq!(path[1].id(), "post");
    }

    #[tokio::test]
    async fn test_compaction_boundary_is_path_relative() {
        use crate::session::Session;

        let dir = tempfile::tempdir().unwrap();
        let meta = JsonlSessionMetadata {
            id: uuid::Uuid::new_v4().to_string(),
            cwd: "/test".into(),
            created_at: chrono::Utc::now(),
        };

        let storage = JsonlSessionStorage::open(dir.path(), meta).await.unwrap();
        let base = chrono::Utc::now();
        let compaction = |id: &str, parent: &str, secs: i64| SessionTreeEntry::Compaction {
            id: id.into(),
            parent_id: Some(parent.into()),
            timestamp: base + chrono::Duration::seconds(secs),
            summary: id.into(),
            first_kept_entry_id: None,
            tokens_before: 0,
            retained_tail: Vec::new(),
        };
        let message = |id: &str, parent: &str, secs: i64| SessionTreeEntry::Message {
            id: id.into(),
            parent_id: Some(parent.into()),
            timestamp: base + chrono::Duration::seconds(secs),
            message: AgentMessage::user(id),
        };

        // Two branches off the root: A compacts early, B compacts late.
        let root = SessionTreeEntry::Message {
            id: "root".into(),
            parent_id: None,
            timestamp: base,
            message: AgentMessage::user("root"),
        };
        storage.append_entry(&root).await.unwrap();
        storage.append_entry(&compaction("compA", "root", 1)).await.unwrap();
        storage.append_entry(&message("postA", "compA", 2)).await.unwrap();
        storage.append_entry(&compaction("compB", "root", 3)).await.unwrap();

        let session = Session::new(storage);

        // Leaf on branch A: the boundary is compA, never the newer compB.
        session.storage().set_leaf_id(Some("postA")).await.unwrap();
        let ts = session.latest_compaction_timestamp().await.unwrap();
        assert_eq!(ts, Some(base + chrono::Duration::seconds(1)));

        // Leaf on branch B: the boundary is compB.
        session.storage().set_leaf_id(Some("compB")).await.unwrap();
        let ts = session.latest_compaction_timestamp().await.unwrap();
        assert_eq!(ts, Some(base + chrono::Duration::seconds(3)));

        // Leaf on the root: no compaction on this path at all.
        session.storage().set_leaf_id(Some("root")).await.unwrap();
        let ts = session.latest_compaction_timestamp().await.unwrap();
        assert_eq!(ts, None);
    }
}