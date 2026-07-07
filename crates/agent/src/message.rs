//! Conversation message model.
//!
//! `Thread` owns `Vec<Message>` as the canonical state; `build_completion_request`
//! maps it into a `LanguageModelRequest`. Each message carries a stable `id`
//! (used as the key for per-request token usage and event linking), a `timestamp`
//! (Unix seconds), and an optional `parent_id` for future branch/fork linking.

use crate::language_model::{MessageContent, Role};
use serde::{Deserialize, Serialize};

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
