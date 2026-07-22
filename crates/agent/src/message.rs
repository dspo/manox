//! Conversation message model.
//!
//! `Thread` owns `Vec<Message>` as the canonical state; `build_completion_request`
//! maps it into a `LanguageModelRequest`. Each message carries a stable `id`
//! (used as the key for per-request token usage and event linking), a `timestamp`
//! (Unix seconds), and an optional `parent_id` for future branch/fork linking.

use crate::language_model::{MessageContent, Role};
use serde::{Deserialize, Serialize};

/// Stable origin of a persisted message. Internal Goal directives are model
/// visible but hidden from every user-facing reconstruction path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageProvenance {
    User,
    Assistant,
    Tool,
    GoalContinuation,
    GoalObjectiveUpdate,
}

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
    pub provenance: MessageProvenance,
    pub role: Role,
    pub content: Vec<MessageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<MessageUiMetadata>,
}

impl Message {
    pub fn user(text: String) -> Self {
        Self::new(
            Role::User,
            MessageProvenance::User,
            vec![MessageContent::Text(text)],
        )
    }

    pub fn user_with_content(content: Vec<MessageContent>) -> Self {
        let provenance = if content
            .iter()
            .any(|part| matches!(part, MessageContent::ToolResult(_)))
        {
            MessageProvenance::Tool
        } else {
            MessageProvenance::User
        };
        Self::new(Role::User, provenance, content)
    }

    pub fn assistant(content: Vec<MessageContent>) -> Self {
        Self::new(Role::Assistant, MessageProvenance::Assistant, content)
    }

    pub fn goal_continuation(text: String) -> Self {
        Self::new(
            Role::User,
            MessageProvenance::GoalContinuation,
            vec![MessageContent::Text(text)],
        )
    }

    pub fn goal_objective_update(text: String) -> Self {
        Self::new(
            Role::User,
            MessageProvenance::GoalObjectiveUpdate,
            vec![MessageContent::Text(text)],
        )
    }

    pub fn is_hidden_from_ui(&self) -> bool {
        matches!(
            self.provenance,
            MessageProvenance::GoalContinuation | MessageProvenance::GoalObjectiveUpdate
        )
    }

    fn new(role: Role, provenance: MessageProvenance, content: Vec<MessageContent>) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: chrono::Utc::now().timestamp(),
            parent_id: None,
            provenance,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_is_required_in_persisted_messages() {
        let value = serde_json::json!({
            "id": "m1",
            "timestamp": 0,
            "parent_id": null,
            "role": "user",
            "content": [],
        });
        assert!(serde_json::from_value::<Message>(value).is_err());
    }

    #[test]
    fn goal_messages_are_model_visible_but_ui_hidden() {
        let message = Message::goal_continuation("continue".into());
        assert!(message.is_hidden_from_ui());
        assert_eq!(message.role, Role::User);
        assert_eq!(
            message.content,
            vec![MessageContent::Text("continue".into())]
        );
    }
}
