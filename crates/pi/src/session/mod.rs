// Session storage and context building.
//
// A session is a tree of entries persisted as JSONL. Each entry has an id and
// parentId, forming a DAG walked leafward to reconstruct the conversation
// context. Variant `type` tags are snake_case and field names are camelCase,
// matching the TS Pi v3 on-disk schema exactly so real session files load.

pub mod jsonl;

use crate::compaction::branch_summarization::BranchSummary;
use crate::types::{AgentMessage, Usage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// A single entry in the session tree.
///
/// Field names serialize as camelCase to match the TS Pi v3 schema. There is
/// no `leaf` entry: every appended entry implicitly becomes the leaf cursor,
/// so the cursor is the id of the last appended entry (or an in-memory
/// override set by `set_leaf_id` for branching).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SessionTreeEntry {
    #[serde(rename = "message", rename_all = "camelCase")]
    Message {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        message: AgentMessage,
    },
    #[serde(rename = "compaction", rename_all = "camelCase")]
    Compaction {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        summary: String,
        first_kept_entry_id: Option<String>,
        tokens_before: u64,
        /// Token usage reported by the summarization call, when recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// The messages kept intact across the compaction, stored verbatim
        /// so a rebuilt context needs no walk past the boundary.
        #[serde(default)]
        retained_tail: Vec<AgentMessage>,
    },
    /// A change of model for the following turns.
    #[serde(rename = "model_change", rename_all = "camelCase")]
    ModelChange {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        provider: String,
        model_id: String,
    },
    /// A change in reasoning depth for the following turns.
    #[serde(rename = "thinking_level_change", rename_all = "camelCase")]
    ThinkingLevelChange {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        /// `None` disables reasoning; a tier string enables it at that depth.
        thinking_level: Option<String>,
    },
    /// A change in the set of tools mounted for the following turns.
    #[serde(rename = "active_tools_change", rename_all = "camelCase")]
    ActiveToolsChange {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        active_tool_names: Vec<String>,
    },
    /// A conversation- or branch-level summary produced by branch summarization.
    #[serde(rename = "branch_summary", rename_all = "camelCase")]
    BranchSummary {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        summary: BranchSummary,
    },
    /// Extension entry whose payload the harness does not interpret. `data` is
    /// either a plain string or a JSON object, matching the TS Pi v3 schema.
    #[serde(rename = "custom", rename_all = "camelCase")]
    Custom {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        custom_type: String,
        data: JsonValue,
    },
    /// An extension message whose payload the harness does not interpret.
    #[serde(rename = "custom_message", rename_all = "camelCase")]
    CustomMessage {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        custom_type: String,
        /// The message body as a plain string, per the v3 schema.
        content: String,
        /// Whether the UI should display this message.
        #[serde(default)]
        display: bool,
    },
    /// A short human-readable label attached to a point in the tree.
    #[serde(rename = "label", rename_all = "camelCase")]
    Label {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        text: String,
    },
    /// Free-form session metadata (agent identity, environment, config snapshot).
    #[serde(rename = "session_info", rename_all = "camelCase")]
    SessionInfo {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        name: String,
    },
}

impl SessionTreeEntry {
    pub fn id(&self) -> &str {
        match self {
            SessionTreeEntry::Message { id, .. }
            | SessionTreeEntry::Compaction { id, .. }
            | SessionTreeEntry::ModelChange { id, .. }
            | SessionTreeEntry::ThinkingLevelChange { id, .. }
            | SessionTreeEntry::ActiveToolsChange { id, .. }
            | SessionTreeEntry::BranchSummary { id, .. }
            | SessionTreeEntry::CustomMessage { id, .. }
            | SessionTreeEntry::Custom { id, .. }
            | SessionTreeEntry::Label { id, .. }
            | SessionTreeEntry::SessionInfo { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            SessionTreeEntry::Message { parent_id, .. }
            | SessionTreeEntry::Compaction { parent_id, .. }
            | SessionTreeEntry::ModelChange { parent_id, .. }
            | SessionTreeEntry::ThinkingLevelChange { parent_id, .. }
            | SessionTreeEntry::ActiveToolsChange { parent_id, .. }
            | SessionTreeEntry::BranchSummary { parent_id, .. }
            | SessionTreeEntry::CustomMessage { parent_id, .. }
            | SessionTreeEntry::Custom { parent_id, .. }
            | SessionTreeEntry::Label { parent_id, .. }
            | SessionTreeEntry::SessionInfo { parent_id, .. } => parent_id.as_deref(),
        }
    }

    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            SessionTreeEntry::Message { timestamp, .. }
            | SessionTreeEntry::Compaction { timestamp, .. }
            | SessionTreeEntry::ModelChange { timestamp, .. }
            | SessionTreeEntry::ThinkingLevelChange { timestamp, .. }
            | SessionTreeEntry::ActiveToolsChange { timestamp, .. }
            | SessionTreeEntry::BranchSummary { timestamp, .. }
            | SessionTreeEntry::CustomMessage { timestamp, .. }
            | SessionTreeEntry::Custom { timestamp, .. }
            | SessionTreeEntry::Label { timestamp, .. }
            | SessionTreeEntry::SessionInfo { timestamp, .. } => *timestamp,
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
        usage: Option<Usage>,
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
            usage,
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
