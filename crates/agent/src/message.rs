//! Conversation message model.
//!
//! `Thread` owns `Vec<Message>` as the canonical state; `build_completion_request`
//! maps it into a `LanguageModelRequest`. Each message carries a stable `id`
//! (used as the key for per-request token usage and event linking), a `timestamp`
//! (Unix seconds), and an optional `parent_id` for future branch/fork linking.

use crate::language_model::{MessageContent, Role};
use serde::{Deserialize, Serialize};

/// UI-only metadata captured when a user message is submitted.
///
/// The model request path ignores this data; it is persisted with the message
/// so historical user turns can keep their send-time chrome stable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageUiMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// `ApprovalMode::as_i64`, stored as an integer to avoid coupling the
    /// message schema to enum names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<i64>,
    /// Set when this user message was injected mid-turn via the steer queue
    /// (drained by the turn loop), rather than starting a fresh turn. The tag
    /// is applied at drain time so it marks messages the running turn actually
    /// absorbed — letting the UI and historical replay distinguish a true steer
    /// from an ordinary follow-up turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steered: Option<bool>,
    /// Machine-generated background-task event. It remains a User-role message
    /// for provider compatibility, but the UI must not attribute it to the
    /// human user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_event: Option<bool>,
}

/// A single conversation message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Stable unique id (UUID v4). Used as the key for per-user-message token
    /// usage and for event linkage.
    pub id: String,
    /// Creation time, Unix seconds.
    pub timestamp: i64,
    /// Parent message id for branch/fork linking. Reserved: not yet wired to any
    /// branch-switch UI; linear conversations leave it `None`.
    pub parent_id: Option<String>,
    pub role: Role,
    pub content: Vec<MessageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<MessageUiMetadata>,
}

impl Message {
    pub fn user(text: String) -> Self {
        Self::new(Role::User, vec![MessageContent::Text(text)])
    }

    pub fn user_with_content(content: Vec<MessageContent>) -> Self {
        Self::new(Role::User, content)
    }

    pub fn assistant(content: Vec<MessageContent>) -> Self {
        Self::new(Role::Assistant, content)
    }

    fn new(role: Role, content: Vec<MessageContent>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now().timestamp(),
            parent_id: None,
            role,
            content,
            ui: None,
        }
    }

    /// Append a model-readable text part (Text/Thinking) to the end.
    pub fn push_text(&mut self, text: impl Into<String>) {
        self.push_content(MessageContent::Text(text.into()));
    }

    pub fn push_content(&mut self, content: MessageContent) {
        self.content.push(content);
    }
}
