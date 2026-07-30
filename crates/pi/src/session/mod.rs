// Session storage and context building.
//
// A session is a tree of entries (messages, compaction summaries, model
// changes, etc.) persisted as JSONL. Each entry has an id and parentId,
// forming a DAG that can be walked to reconstruct the conversation context.

pub mod jsonl;

use crate::types::AgentMessage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// A single entry in the session tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionTreeEntry {
    #[serde(rename = "message")]
    Message {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        message: AgentMessage,
    },
    #[serde(rename = "compaction")]
    Compaction {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        summary: String,
        #[serde(rename = "firstKeptEntryId")]
        first_kept_entry_id: Option<String>,
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        /// The messages kept intact across the compaction, stored verbatim
        /// so a rebuilt context needs no walk past the boundary.
        #[serde(rename = "retainedTail", default)]
        retained_tail: Vec<AgentMessage>,
    },
    #[serde(rename = "leaf")]
    Leaf {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        #[serde(rename = "targetId")]
        target_id: String,
    },
    #[serde(rename = "modelChange")]
    ModelChange {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        provider: String,
        #[serde(rename = "modelId")]
        model_id: String,
    },
    #[serde(rename = "custom")]
    Custom {
        id: String,
        #[serde(rename = "parentId")]
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        #[serde(rename = "customType")]
        custom_type: String,
        data: Option<JsonValue>,
    },
}

impl SessionTreeEntry {
    pub fn id(&self) -> &str {
        match self {
            SessionTreeEntry::Message { id, .. }
            | SessionTreeEntry::Compaction { id, .. }
            | SessionTreeEntry::Leaf { id, .. }
            | SessionTreeEntry::ModelChange { id, .. }
            | SessionTreeEntry::Custom { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            SessionTreeEntry::Message { parent_id, .. }
            | SessionTreeEntry::Compaction { parent_id, .. }
            | SessionTreeEntry::Leaf { parent_id, .. }
            | SessionTreeEntry::ModelChange { parent_id, .. }
            | SessionTreeEntry::Custom { parent_id, .. } => parent_id.as_deref(),
        }
    }

    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            SessionTreeEntry::Message { timestamp, .. }
            | SessionTreeEntry::Compaction { timestamp, .. }
            | SessionTreeEntry::Leaf { timestamp, .. }
            | SessionTreeEntry::ModelChange { timestamp, .. }
            | SessionTreeEntry::Custom { timestamp, .. } => *timestamp,
        }
    }
}

/// Trait for session storage backends.
#[async_trait::async_trait]
pub trait SessionStorage: Send + Sync {
    /// Generate a new unique entry ID.
    async fn create_entry_id(&self) -> Result<String, anyhow::Error>;

    /// Append an entry to the session.
    async fn append_entry(&self, entry: &SessionTreeEntry) -> Result<(), anyhow::Error>;

    /// Get an entry by ID.
    async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, anyhow::Error>;

    /// Get the current leaf ID (cursor position).
    async fn get_leaf_id(&self) -> Result<Option<String>, anyhow::Error>;

    /// Set the current leaf ID.
    async fn set_leaf_id(&self, leaf_id: Option<&str>) -> Result<(), anyhow::Error>;

    /// Get all entries, optionally filtered.
    async fn get_entries(&self) -> Result<Vec<SessionTreeEntry>, anyhow::Error>;

    /// Walk from the leaf to root (or last compaction), returning entries
    /// in chronological order.
    async fn get_path_to_root_or_compaction(
        &self,
        leaf_id: Option<&str>,
    ) -> Result<Vec<SessionTreeEntry>, anyhow::Error>;
}

/// A session wraps a SessionStorage with context-building logic.
pub struct Session<S: SessionStorage> {
    storage: S,
}

impl<S: SessionStorage> Session<S> {
    pub fn new(storage: S) -> Self {
        Session { storage }
    }

    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// Append a message entry and return the entry ID.
    pub async fn append_message(&self, message: AgentMessage) -> Result<String, anyhow::Error> {
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;

        let entry = SessionTreeEntry::Message {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now(),
            message,
        };
        self.storage.append_entry(&entry).await?;
        self.storage.set_leaf_id(Some(&id)).await?;
        Ok(id)
    }

    /// Append a compaction entry and return the entry ID and timestamp.
    ///
    /// The leaf cursor moves to the entry, so later messages parent onto it
    /// and a context rebuild stops at this boundary. The returned timestamp
    /// is the boundary instant: the in-transcript summary message carries
    /// the same one, so a restored transcript matches the post-compaction
    /// one exactly.
    pub async fn append_compaction(
        &self,
        summary: &str,
        first_kept_entry_id: Option<String>,
        tokens_before: u64,
        retained_tail: Vec<AgentMessage>,
    ) -> Result<(String, DateTime<Utc>), anyhow::Error> {
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;
        let timestamp = Utc::now();

        let entry = SessionTreeEntry::Compaction {
            id: id.clone(),
            parent_id,
            timestamp,
            summary: summary.to_string(),
            first_kept_entry_id,
            tokens_before,
            retained_tail,
        };
        self.storage.append_entry(&entry).await?;
        self.storage.set_leaf_id(Some(&id)).await?;
        Ok((id, timestamp))
    }

    /// Timestamp of the compaction bounding the current path, if any.
    ///
    /// The boundary is path-relative: only the latest compaction reachable
    /// from the current leaf counts, never another branch's.
    pub async fn latest_compaction_timestamp(
        &self,
    ) -> Result<Option<DateTime<Utc>>, anyhow::Error> {
        let path = self.build_context().await?;
        Ok(match path.first() {
            Some(SessionTreeEntry::Compaction { timestamp, .. }) => Some(*timestamp),
            _ => None,
        })
    }

    /// Build the context entries by walking the tree from leaf to root.
    pub async fn build_context(&self) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        let leaf_id = self.storage.get_leaf_id().await?;
        self.storage
            .get_path_to_root_or_compaction(leaf_id.as_deref())
            .await
    }
}
