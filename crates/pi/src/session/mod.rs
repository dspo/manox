// Session storage and context building.
//
// A session is a tree of entries persisted as JSONL. Each entry has an id and
// parentId, forming a DAG walked leafward to reconstruct the conversation
// context. Variant `type` tags are snake_case and field names are camelCase,
// matching the TS Pi v3 on-disk schema exactly so real session files load.

pub mod jsonl;

use crate::types::{AgentMessage, Usage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// A single entry in the session tree.
///
/// Field names serialize as camelCase to match the TS Pi v3 schema. A `leaf`
/// entry records a cursor move to an older branch point: its `targetId` is the
/// entry the cursor now points at, and the leaf entry itself is never walked
/// (the cursor is `targetId`, not the leaf entry's own id).
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
        /// Materialized messages kept after the boundary — a self-contained
        /// checkpoint: a context rebuild reads them straight from the entry
        /// instead of walking the tree from `first_kept_entry_id`. Absent on
        /// older session files, where the tree walk remains the fallback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retained_tail: Option<Vec<AgentMessage>>,
        /// Token usage reported by the summarization call, when recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        /// Structured payload a summarization hook may attach.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<JsonValue>,
        /// Whether the boundary was written by a hook rather than the
        /// harness's own compaction.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
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
        /// The reasoning tier for following turns, or `"off"` to disable it.
        /// Always a string — `null` would drop the field on the wire.
        thinking_level: String,
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
    /// Flat (not nested): `summary` is the prose string, `details` carries any
    /// structured payload (e.g. files changed).
    #[serde(rename = "branch_summary", rename_all = "camelCase")]
    BranchSummary {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        from_id: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<JsonValue>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_hook: Option<bool>,
    },
    /// Extension entry whose payload the harness does not interpret.
    #[serde(rename = "custom", rename_all = "camelCase")]
    Custom {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        custom_type: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<JsonValue>,
    },
    /// An extension message whose payload the harness does not interpret.
    /// `content` is a plain string or an array of text/image blocks, matching
    /// the v3 schema.
    #[serde(rename = "custom_message", rename_all = "camelCase")]
    CustomMessage {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        custom_type: String,
        #[serde(default, deserialize_with = "crate::types::deserialize_content_blocks")]
        content: Vec<crate::types::ContentBlock>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<JsonValue>,
        #[serde(default)]
        display: bool,
    },
    /// A short human-readable label attached to a point in the tree.
    #[serde(rename = "label", rename_all = "camelCase")]
    Label {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        target_id: String,
        // TS types `label` as `string | undefined` and omits it when unset;
        // skip-on-None keeps Rust output byte-identical to a TS-written entry.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    /// Free-form session metadata (agent identity, environment, config snapshot).
    #[serde(rename = "session_info", rename_all = "camelCase")]
    SessionInfo {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// A cursor move: the leaf points at `targetId` (an older entry) for
    /// branching. Appended by `set_leaf_id`; never the walk start.
    #[serde(rename = "leaf", rename_all = "camelCase")]
    Leaf {
        id: String,
        parent_id: Option<String>,
        timestamp: DateTime<Utc>,
        target_id: Option<String>,
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
            | SessionTreeEntry::SessionInfo { id, .. }
            | SessionTreeEntry::Leaf { id, .. } => id,
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
            | SessionTreeEntry::SessionInfo { parent_id, .. }
            | SessionTreeEntry::Leaf { parent_id, .. } => parent_id.as_deref(),
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
            | SessionTreeEntry::SessionInfo { timestamp, .. }
            | SessionTreeEntry::Leaf { timestamp, .. } => *timestamp,
        }
    }

    /// The leaf cursor after appending this entry: a `leaf` entry redirects to
    /// its `targetId`; every other entry's cursor is its own id.
    pub(crate) fn leaf_cursor_after(&self) -> Option<String> {
        match self {
            SessionTreeEntry::Leaf { target_id, .. } => target_id.clone(),
            _ => Some(self.id().to_string()),
        }
    }
}

/// Trait for session storage backends.
#[async_trait::async_trait]
pub trait SessionStorage: Send + Sync {
    /// Generate a new unique entry ID.
    async fn create_entry_id(&self) -> Result<String, anyhow::Error>;

    /// Append an entry to the session and synchronously advance the leaf
    /// cursor to it.
    ///
    /// The cursor after the append is the entry's own id, except for a `leaf`
    /// entry whose cursor is its `targetId`. Implementations must update their
    /// persisted cursor within this call so a later `get_leaf_id` reflects the
    /// append even if no further write follows — callers must not pair
    /// `append_entry` with a separate `set_leaf_id` for the same cursor move.
    async fn append_entry(&self, entry: &SessionTreeEntry) -> Result<(), anyhow::Error>;

    /// Get an entry by ID.
    async fn get_entry(&self, id: &str) -> Result<Option<SessionTreeEntry>, anyhow::Error>;

    /// Get the current leaf ID (cursor position).
    async fn get_leaf_id(&self) -> Result<Option<String>, anyhow::Error>;

    /// Move the cursor to an older branch point by appending a `leaf` entry.
    /// Implementations must validate that the target id exists before
    /// persisting; the appended entry's cursor (its `targetId`) becomes the
    /// new leaf.
    async fn set_leaf_id(&self, leaf_id: Option<&str>) -> Result<(), anyhow::Error>;

    /// Get all entries, optionally filtered.
    async fn get_entries(&self) -> Result<Vec<SessionTreeEntry>, anyhow::Error>;

    /// Walk from the leaf to the root, returning the full path in
    /// chronological order. Compaction boundaries do not stop the walk —
    /// callers decide how to project them. `None` yields an empty path. An
    /// explicit `leaf_id` unknown to storage is an error, as is a parent id
    /// with no entry: a truncated path would silently drop history.
    async fn get_path(&self, leaf_id: Option<&str>)
    -> Result<Vec<SessionTreeEntry>, anyhow::Error>;
}

/// A session wraps a SessionStorage with context-building logic.
pub struct Session<S: SessionStorage> {
    storage: S,
}

/// Authorship of a persisted compaction: whether a before-compact hook
/// supplied the summary (skipping the summarization model call), and the
/// structured payload it attached. The model path passes `from_hook: false`
/// with no `details`.
#[derive(Debug, Clone, Default)]
pub struct CompactionAuthorship {
    pub details: Option<JsonValue>,
    pub from_hook: bool,
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
        Ok(id)
    }

    /// Append a `model_change` entry and return the entry ID.
    pub async fn append_model_change(
        &self,
        provider: &str,
        model_id: &str,
    ) -> Result<String, anyhow::Error> {
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;

        let entry = SessionTreeEntry::ModelChange {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now(),
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        };
        self.storage.append_entry(&entry).await?;
        Ok(id)
    }

    /// Append an `active_tools_change` entry and return the entry ID.
    pub async fn append_active_tools_change(
        &self,
        active_tool_names: &[String],
    ) -> Result<String, anyhow::Error> {
        let id = self.storage.create_entry_id().await?;
        let parent_id = self.storage.get_leaf_id().await?;

        let entry = SessionTreeEntry::ActiveToolsChange {
            id: id.clone(),
            parent_id,
            timestamp: Utc::now(),
            active_tool_names: active_tool_names.to_vec(),
        };
        self.storage.append_entry(&entry).await?;
        Ok(id)
    }

    /// Append a compaction entry and return the entry ID and timestamp.
    ///
    /// The leaf cursor moves to the entry, so later messages parent onto it.
    /// The retained tail persists with the boundary, making it a
    /// self-contained checkpoint; `first_kept_entry_id` stays as the tree-walk
    /// fallback for boundaries written without one. The returned timestamp is
    /// the boundary instant: the in-transcript summary message carries the
    /// same one, so a restored transcript matches the post-compaction one
    /// exactly.
    pub async fn append_compaction(
        &self,
        summary: &str,
        first_kept_entry_id: Option<String>,
        tokens_before: u64,
        usage: Option<Usage>,
        authorship: CompactionAuthorship,
        retained_tail: Option<Vec<AgentMessage>>,
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
            usage,
            details: authorship.details,
            from_hook: Some(authorship.from_hook),
        };
        self.storage.append_entry(&entry).await?;
        Ok((id, timestamp))
    }

    /// Timestamp of the compaction bounding the current path, if any.
    ///
    /// The boundary is path-relative: only the latest compaction reachable
    /// from the current leaf counts, never another branch's.
    pub async fn latest_compaction_timestamp(
        &self,
    ) -> Result<Option<DateTime<Utc>>, anyhow::Error> {
        let entries = self.build_context_entries().await?;
        Ok(match entries.first() {
            Some(SessionTreeEntry::Compaction { timestamp, .. }) => Some(*timestamp),
            _ => None,
        })
    }

    /// Walk from the current leaf to the root, returning every entry on the
    /// active path in chronological order — all entry types, across however
    /// many compaction boundaries the path spans.
    pub async fn get_branch(&self) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        let leaf_id = self.storage.get_leaf_id().await?;
        self.storage.get_path(leaf_id.as_deref()).await
    }

    /// Build the active, compaction-aware entry list.
    ///
    /// When the active path contains a compaction, the latest one heads the
    /// list, followed by the kept entries from its `first_kept_entry_id`
    /// onward (reconstructed from the tree, never from the boundary itself)
    /// and every entry after the boundary. Older summarized entries are
    /// omitted. Without a compaction this is the whole branch.
    pub async fn build_context_entries(&self) -> Result<Vec<SessionTreeEntry>, anyhow::Error> {
        let path = self.get_branch().await?;
        Ok(build_context_entries(path))
    }

    /// Build the session context: the messages a restored agent continues
    /// with, plus the settings the active path carries.
    pub async fn build_session_context(&self) -> Result<SessionContext, anyhow::Error> {
        let path = self.get_branch().await?;
        let (thinking_level, model, active_tool_names) = context_settings(&path);
        let entries = build_context_entries(path);
        let mut messages = Vec::new();
        let mut message_entry_ids = Vec::new();
        for entry in &entries {
            let projected = session_entry_to_context_messages(entry);
            if !projected.is_empty() {
                message_entry_ids.push(Some(entry.id().to_string()));
                // Messages materialized inside the entry (a compaction's
                // retained tail) have no entry ids of their own.
                message_entry_ids.extend((1..projected.len()).map(|_| None));
                messages.extend(projected);
            }
        }
        Ok(SessionContext {
            messages,
            message_entry_ids,
            thinking_level,
            model,
            active_tool_names,
        })
    }
}

/// The projected session state an agent is restored from.
pub struct SessionContext {
    /// Messages projected from the compaction-aware entry list.
    pub messages: Vec<AgentMessage>,
    /// The entry that produced each message, parallel to `messages`. Drives
    /// `first_kept_entry_id` resolution when the next compaction cuts the
    /// restored transcript.
    pub message_entry_ids: Vec<Option<String>>,
    /// The reasoning tier for following turns; `None` when the path left it
    /// at (or never changed it from) `"off"`.
    pub thinking_level: Option<String>,
    /// The model the session was last driven with — from the latest
    /// `model_change` entry, or the latest assistant message's own identity.
    pub model: Option<SessionModelRef>,
    /// The active tool subset from the latest `active_tools_change` entry;
    /// `None` when the path never narrowed the mounted set.
    pub active_tool_names: Option<Vec<String>>,
}

/// A model reference carried by the session path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionModelRef {
    pub provider: String,
    pub model_id: String,
}

/// The compaction-aware projection over an active path: latest compaction
/// first, then everything after the boundary. A boundary carrying a
/// `retained_tail` is a self-contained checkpoint — the kept segment is not
/// walked; without one, the kept segment is reconstructed from
/// `first_kept_entry_id`.
fn build_context_entries(path: Vec<SessionTreeEntry>) -> Vec<SessionTreeEntry> {
    let Some(compaction_idx) = path
        .iter()
        .rposition(|e| matches!(e, SessionTreeEntry::Compaction { .. }))
    else {
        return path;
    };
    let SessionTreeEntry::Compaction {
        first_kept_entry_id,
        retained_tail,
        ..
    } = &path[compaction_idx]
    else {
        unreachable!("rposition matched a compaction");
    };

    let mut context_entries = vec![path[compaction_idx].clone()];
    // A `first_kept_entry_id` absent from the path keeps nothing — the same
    // outcome an undefined id produces in a hand-edited TS session file.
    if retained_tail.is_none() {
        let mut found_first_kept = false;
        for entry in &path[..compaction_idx] {
            if Some(entry.id()) == first_kept_entry_id.as_deref() {
                found_first_kept = true;
            }
            if found_first_kept {
                context_entries.push(entry.clone());
            }
        }
    }
    context_entries.extend_from_slice(&path[compaction_idx + 1..]);
    context_entries
}

/// The settings the active path carries: the reasoning tier from the latest
/// `thinking_level_change`, the model from the latest `model_change` (an
/// assistant message's own identity is a fresher witness than an older
/// `model_change`, matching the TS projection), and the active tool subset
/// from the latest `active_tools_change`.
fn context_settings(
    path: &[SessionTreeEntry],
) -> (Option<String>, Option<SessionModelRef>, Option<Vec<String>>) {
    let mut thinking_level = None;
    let mut model = None;
    let mut active_tool_names = None;
    for entry in path {
        match entry {
            SessionTreeEntry::ThinkingLevelChange {
                thinking_level: l, ..
            } => {
                thinking_level = (l != "off").then(|| l.clone());
            }
            SessionTreeEntry::ModelChange {
                provider, model_id, ..
            } => {
                model = Some(SessionModelRef {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                });
            }
            SessionTreeEntry::Message {
                message:
                    AgentMessage::Assistant {
                        provider, model: m, ..
                    },
                ..
            } => {
                model = Some(SessionModelRef {
                    provider: provider.clone(),
                    model_id: m.clone(),
                });
            }
            SessionTreeEntry::ActiveToolsChange {
                active_tool_names: names,
                ..
            } => {
                active_tool_names = Some(names.clone());
            }
            _ => {}
        }
    }
    (thinking_level, model, active_tool_names)
}

/// Project one session entry into context messages. Display/state entries
/// (model/thinking/tool changes, labels, custom data, the cursor) produce
/// nothing.
pub fn session_entry_to_context_messages(entry: &SessionTreeEntry) -> Vec<AgentMessage> {
    match entry {
        SessionTreeEntry::Message { message, .. } => vec![message.clone()],
        SessionTreeEntry::CustomMessage {
            custom_type,
            content,
            details,
            display,
            timestamp,
            ..
        } => vec![AgentMessage::Custom {
            custom_type: custom_type.clone(),
            content: content.clone(),
            details: details.clone(),
            display: *display,
            timestamp: *timestamp,
        }],
        SessionTreeEntry::BranchSummary {
            summary, timestamp, ..
        } if !summary.is_empty() => vec![branch_summary_message(summary, *timestamp)],
        SessionTreeEntry::Compaction {
            summary,
            timestamp,
            retained_tail,
            ..
        } => {
            // The summary carrier heads the projection; a persisted tail
            // follows it, exactly as it did in the post-compaction transcript.
            let mut messages = Vec::with_capacity(1 + retained_tail.as_ref().map_or(0, Vec::len));
            messages.push(compaction_summary_message(summary, *timestamp));
            if let Some(tail) = retained_tail {
                messages.extend(tail.iter().cloned());
            }
            messages
        }
        _ => Vec::new(),
    }
}

/// The in-transcript carrier for a compaction summary: a tagged user
/// message. Kept symmetric between compaction and restore so the summary
/// reads identically whether it was just written or rebuilt from storage.
pub fn compaction_summary_message(summary: &str, timestamp: DateTime<Utc>) -> AgentMessage {
    AgentMessage::User {
        content: vec![crate::types::ContentBlock::Text {
            text: format!("{COMPACTION_SUMMARY_PREFIX}{summary}{COMPACTION_SUMMARY_SUFFIX}"),
            signature: None,
        }],
        timestamp,
    }
}

/// The in-transcript carrier for a branch summary: a tagged user message.
pub fn branch_summary_message(summary: &str, timestamp: DateTime<Utc>) -> AgentMessage {
    AgentMessage::User {
        content: vec![crate::types::ContentBlock::Text {
            text: format!("{BRANCH_SUMMARY_PREFIX}{summary}{BRANCH_SUMMARY_SUFFIX}"),
            signature: None,
        }],
        timestamp,
    }
}

/// Tag wrapping a compaction summary in the transcript.
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";

/// Tag wrapping a branch summary in the transcript.
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ContentBlock;

    fn message(id: &str, parent: Option<&str>, text: &str) -> SessionTreeEntry {
        SessionTreeEntry::Message {
            id: id.into(),
            parent_id: parent.map(Into::into),
            timestamp: Utc::now(),
            message: AgentMessage::user(text),
        }
    }

    fn compaction(
        id: &str,
        parent: Option<&str>,
        first_kept_entry_id: Option<&str>,
    ) -> SessionTreeEntry {
        SessionTreeEntry::Compaction {
            id: id.into(),
            parent_id: parent.map(Into::into),
            timestamp: Utc::now(),
            summary: format!("summary-{id}"),
            first_kept_entry_id: first_kept_entry_id.map(Into::into),
            tokens_before: 0,
            retained_tail: None,
            usage: None,
            details: None,
            from_hook: None,
        }
    }

    fn ids(entries: &[SessionTreeEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.id()).collect()
    }

    #[test]
    fn context_entries_reconstruct_the_kept_segment_from_the_tree() {
        // m1 m2 m3 [comp keeps from m2] m4: the context is the boundary, the
        // kept entries m2..m3 walked out of the pre-boundary path, then m4.
        let path = vec![
            message("m1", None, "one"),
            message("m2", Some("m1"), "two"),
            message("m3", Some("m2"), "three"),
            compaction("c1", Some("m3"), Some("m2")),
            message("m4", Some("c1"), "four"),
        ];
        let entries = build_context_entries(path);
        assert_eq!(ids(&entries), ["c1", "m2", "m3", "m4"]);
    }

    #[test]
    fn context_entries_last_compaction_wins() {
        // m1 [c1 keeps m1] m2 [c2 keeps nothing-known] m3: only the latest
        // boundary heads the context; the older one is summarized away.
        let path = vec![
            message("m1", None, "one"),
            compaction("c1", Some("m1"), Some("m1")),
            message("m2", Some("c1"), "two"),
            compaction("c2", Some("m2"), Some("m1")),
            message("m3", Some("c2"), "three"),
        ];
        let entries = build_context_entries(path);
        // c2's first_kept (m1) precedes c2 on the path, so the scan finds it
        // and keeps m1, c1, m2 — everything from m1 up to the boundary.
        assert_eq!(ids(&entries), ["c2", "m1", "c1", "m2", "m3"]);
    }

    #[test]
    fn context_entries_unknown_first_kept_keeps_nothing() {
        // A first_kept_entry_id that never appears on the path keeps no
        // pre-boundary entry at all.
        let path = vec![
            message("m1", None, "one"),
            message("m2", Some("m1"), "two"),
            compaction("c1", Some("m2"), Some("ghost")),
            message("m3", Some("c1"), "three"),
        ];
        let entries = build_context_entries(path);
        assert_eq!(ids(&entries), ["c1", "m3"]);
    }

    #[test]
    fn context_settings_take_the_latest_witness() {
        let assistant = |id: &str, parent: &str| SessionTreeEntry::Message {
            id: id.into(),
            parent_id: Some(parent.into()),
            timestamp: Utc::now(),
            message: AgentMessage::Assistant {
                content: vec![],
                model: "claude-opus".into(),
                provider: "anthropic".into(),
                api: "anthropic".into(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                stop_reason: None,
                usage: Box::default(),
                error_message: None,
                timestamp: Utc::now(),
            },
        };
        let path = vec![
            SessionTreeEntry::ModelChange {
                id: "mc".into(),
                parent_id: None,
                timestamp: Utc::now(),
                provider: "openai".into(),
                model_id: "gpt-5".into(),
            },
            SessionTreeEntry::ThinkingLevelChange {
                id: "t1".into(),
                parent_id: Some("mc".into()),
                timestamp: Utc::now(),
                thinking_level: "medium".into(),
            },
            assistant("a1", "t1"),
            SessionTreeEntry::ActiveToolsChange {
                id: "at".into(),
                parent_id: Some("a1".into()),
                timestamp: Utc::now(),
                active_tool_names: vec!["read".into(), "bash".into()],
            },
            SessionTreeEntry::ThinkingLevelChange {
                id: "t2".into(),
                parent_id: Some("at".into()),
                timestamp: Utc::now(),
                thinking_level: "off".into(),
            },
        ];
        let (thinking_level, model, active_tool_names) = context_settings(&path);
        // The trailing "off" change resets the tier to the provider default.
        assert_eq!(thinking_level, None);
        // An assistant message is a fresher model witness than an older
        // model_change.
        assert_eq!(
            model,
            Some(SessionModelRef {
                provider: "anthropic".into(),
                model_id: "claude-opus".into(),
            })
        );
        // The latest active_tools_change carries the narrowed subset.
        assert_eq!(
            active_tool_names,
            Some(vec!["read".to_string(), "bash".to_string()])
        );
    }

    #[test]
    fn entry_projection_covers_every_variant() {
        let ts = Utc::now();
        let custom_message = SessionTreeEntry::CustomMessage {
            id: "cm".into(),
            parent_id: None,
            timestamp: ts,
            custom_type: "notice".into(),
            content: vec![ContentBlock::Text {
                text: "heads up".into(),
                signature: None,
            }],
            details: Some(serde_json::json!({"level": 1})),
            display: true,
        };
        let projected = session_entry_to_context_messages(&custom_message);
        assert_eq!(projected.len(), 1);
        match &projected[0] {
            AgentMessage::Custom {
                custom_type,
                display,
                details,
                ..
            } => {
                assert_eq!(custom_type, "notice");
                assert!(display);
                assert_eq!(details, &Some(serde_json::json!({"level": 1})));
            }
            other => panic!("expected Custom, got {other:?}"),
        }

        let branch_summary = SessionTreeEntry::BranchSummary {
            id: "bs".into(),
            parent_id: None,
            timestamp: ts,
            from_id: "x".into(),
            summary: "came back".into(),
            details: None,
            usage: None,
            from_hook: None,
        };
        let projected = session_entry_to_context_messages(&branch_summary);
        assert_eq!(projected.len(), 1);
        match &projected[0] {
            AgentMessage::User { content, .. } => match &content[0] {
                ContentBlock::Text { text, .. } => assert_eq!(
                    text,
                    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\ncame back</summary>"
                ),
                other => panic!("expected text, got {other:?}"),
            },
            other => panic!("expected User, got {other:?}"),
        }

        // An empty branch summary produces nothing.
        let empty_summary = SessionTreeEntry::BranchSummary {
            id: "bs2".into(),
            parent_id: None,
            timestamp: ts,
            from_id: "x".into(),
            summary: String::new(),
            details: None,
            usage: None,
            from_hook: None,
        };
        assert!(session_entry_to_context_messages(&empty_summary).is_empty());

        // Display/state entries never enter the context.
        for entry in [
            SessionTreeEntry::ModelChange {
                id: "mc".into(),
                parent_id: None,
                timestamp: ts,
                provider: "p".into(),
                model_id: "m".into(),
            },
            SessionTreeEntry::ThinkingLevelChange {
                id: "tl".into(),
                parent_id: None,
                timestamp: ts,
                thinking_level: "high".into(),
            },
            SessionTreeEntry::ActiveToolsChange {
                id: "at".into(),
                parent_id: None,
                timestamp: ts,
                active_tool_names: vec![],
            },
            SessionTreeEntry::Custom {
                id: "cu".into(),
                parent_id: None,
                timestamp: ts,
                custom_type: "state".into(),
                data: None,
            },
            SessionTreeEntry::Label {
                id: "la".into(),
                parent_id: None,
                timestamp: ts,
                target_id: "x".into(),
                label: None,
            },
            SessionTreeEntry::SessionInfo {
                id: "si".into(),
                parent_id: None,
                timestamp: ts,
                name: None,
            },
            SessionTreeEntry::Leaf {
                id: "le".into(),
                parent_id: None,
                timestamp: ts,
                target_id: None,
            },
        ] {
            assert!(
                session_entry_to_context_messages(&entry).is_empty(),
                "{entry:?} must not project into the context"
            );
        }
    }

    #[test]
    fn compaction_projection_uses_the_summary_tags() {
        let entry = compaction("c1", None, None);
        let projected = session_entry_to_context_messages(&entry);
        assert_eq!(projected.len(), 1);
        match &projected[0] {
            AgentMessage::User { content, timestamp } => {
                assert_eq!(*timestamp, entry.timestamp());
                match &content[0] {
                    ContentBlock::Text { text, .. } => assert_eq!(
                        text,
                        "The conversation history before this point was compacted into the following summary:\n\n<summary>\nsummary-c1\n</summary>"
                    ),
                    other => panic!("expected text, got {other:?}"),
                }
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn compaction_with_retained_tail_projects_summary_then_tail() {
        let mut entry = compaction("c1", None, Some("m2"));
        let SessionTreeEntry::Compaction { retained_tail, .. } = &mut entry else {
            unreachable!()
        };
        *retained_tail = Some(vec![
            AgentMessage::user("kept one"),
            AgentMessage::user("kept two"),
        ]);

        let projected = session_entry_to_context_messages(&entry);
        assert_eq!(projected.len(), 3);
        let AgentMessage::User { content, .. } = &projected[1] else {
            panic!("tail message must project verbatim")
        };
        assert!(matches!(&content[0], ContentBlock::Text { text, .. } if text == "kept one"));
    }

    #[test]
    fn context_entries_retained_tail_skips_the_kept_walk() {
        // m1 m2 [c1 keeps m2 AND carries a materialized tail] m3: the tail
        // makes c1 self-contained — m2 is not walked out of the tree even
        // though first_kept_entry_id names it.
        let mut boundary = compaction("c1", Some("m2"), Some("m2"));
        let SessionTreeEntry::Compaction { retained_tail, .. } = &mut boundary else {
            unreachable!()
        };
        *retained_tail = Some(vec![AgentMessage::user("kept")]);
        let path = vec![
            message("m1", None, "one"),
            message("m2", Some("m1"), "two"),
            boundary,
            message("m3", Some("c1"), "three"),
        ];
        let entries = build_context_entries(path);
        assert_eq!(ids(&entries), ["c1", "m3"]);
    }
}
