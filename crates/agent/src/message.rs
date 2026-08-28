//! Conversation message model.
//!
//! `Thread` owns `Vec<Message>` as the canonical state; `build_completion_request`
//! maps it into a `LanguageModelRequest`. Each message carries a stable `id`
//! (used as the key for per-request token usage and event linking), a `timestamp`
//! (Unix seconds), and an optional `parent_id` for future branch/fork linking.

use crate::language_model::{MessageContent, Role};
use serde::{Deserialize, Serialize};

/// Stable origin of a persisted message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageProvenance {
    User,
    Assistant,
    Tool,
}

/// Originating agent of a user-role message the human did not type
/// (harness-seeded turns, team peer deliveries, member opening tasks).
///
/// The value is the agent's routing identity; the UI resolves it to a
/// display name at render time. New agents (built-in, user-authored,
/// plugin) flow through `Agent` verbatim — no per-agent code paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageAuthor {
    /// The session's main agent (Captain).
    #[default]
    Lead,
    /// A named agent: team member name or agent-manifest name.
    Agent(String),
}

impl MessageAuthor {
    /// Routing identity: `"lead"` for the main agent, the manifest /
    /// member name otherwise. The session sidecar persists this string.
    pub fn routing(&self) -> &str {
        match self {
            MessageAuthor::Lead => crate::team::LEADER_NAME,
            MessageAuthor::Agent(name) => name,
        }
    }

    /// Inverse of [`MessageAuthor::routing`].
    pub fn from_routing(name: &str) -> Self {
        if name == crate::team::LEADER_NAME {
            MessageAuthor::Lead
        } else {
            MessageAuthor::Agent(name.to_string())
        }
    }
}

/// UI-only metadata captured when a user message is submitted.
///
/// The model request path ignores this data; it is persisted with the message
/// so historical user turns can keep their send-time chrome stable.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MessageUiMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// `PermissionMode::as_i64`, stored as an integer to avoid coupling the
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
    /// The agent that authored this user-role message; absent = human
    /// input. The model request path never reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<MessageAuthor>,
    /// This user message entered the session via team peer delivery
    /// (`SendMessage`); the reload path rebuilds it as a team bubble.
    #[serde(default, skip_serializing_if = "is_false")]
    pub peer: bool,
    /// UI-only display form of this user message — e.g. the compact
    /// `/name args` invocation for a registry slash turn whose model-facing
    /// text is the expanded macro/skill body. When set, the conversation
    /// bubble renders this instead of the model-facing text, live and after
    /// reload alike. The model request path never reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_text: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
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
    fn user_with_content_sets_tool_provenance() {
        let tool = Message::user_with_content(vec![MessageContent::ToolResult(
            crate::language_model::LanguageModelToolResult {
                tool_use_id: "t1".into(),
                tool_name: "Read".into(),
                is_error: false,
                content: "x".into(),
            },
        )]);
        assert_eq!(tool.provenance, MessageProvenance::Tool);
        let plain = Message::user_with_content(vec![MessageContent::Text("hi".into())]);
        assert_eq!(plain.provenance, MessageProvenance::User);
    }

    #[test]
    fn ui_metadata_display_text_round_trips_and_skips_when_absent() {
        let with_display = MessageUiMetadata {
            display_text: Some("/gitwork:deliver fast".into()),
            ..Default::default()
        };
        let value = serde_json::to_value(&with_display).unwrap();
        assert_eq!(value["display_text"], "/gitwork:deliver fast");
        let back: MessageUiMetadata = serde_json::from_value(value).unwrap();
        assert_eq!(back.display_text.as_deref(), Some("/gitwork:deliver fast"));

        // Absent display_text stays out of the persisted form entirely.
        let plain = MessageUiMetadata::default();
        let value = serde_json::to_value(&plain).unwrap();
        assert!(value.get("display_text").is_none());
        let back: MessageUiMetadata = serde_json::from_value(value).unwrap();
        assert_eq!(back.display_text, None);
    }

    #[test]
    fn ui_metadata_author_round_trips_and_skips_when_absent() {
        let lead = MessageUiMetadata {
            author: Some(MessageAuthor::Lead),
            ..Default::default()
        };
        let value = serde_json::to_value(&lead).unwrap();
        assert_eq!(value["author"], "lead");

        let named = MessageUiMetadata {
            author: Some(MessageAuthor::Agent("Sailor".into())),
            peer: true,
            ..Default::default()
        };
        let value = serde_json::to_value(&named).unwrap();
        assert_eq!(value["author"], serde_json::json!({ "agent": "Sailor" }));
        assert_eq!(value["peer"], true);
        let back: MessageUiMetadata = serde_json::from_value(value).unwrap();
        assert_eq!(back.author, Some(MessageAuthor::Agent("Sailor".into())));
        assert!(back.peer);

        // Human turns carry no attribution in the persisted form.
        let plain = MessageUiMetadata::default();
        let value = serde_json::to_value(&plain).unwrap();
        assert!(value.get("author").is_none() && value.get("peer").is_none());
    }

    #[test]
    fn author_routing_round_trips_through_lead_sentinel() {
        assert_eq!(MessageAuthor::Lead.routing(), "lead");
        assert_eq!(MessageAuthor::Agent("Sailor".into()).routing(), "Sailor");
        assert_eq!(MessageAuthor::from_routing("lead"), MessageAuthor::Lead);
        assert_eq!(
            MessageAuthor::from_routing("Sailor"),
            MessageAuthor::Agent("Sailor".into())
        );
    }
}
