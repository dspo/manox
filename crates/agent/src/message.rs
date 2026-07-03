//! Conversation message model.
//!
//! `Thread` owns `Vec<Message>` as the canonical state; `build_completion_request`
//! maps it into a `LanguageModelRequest`. The first version carries only text,
//! thinking, and tool content (reusing `MessageContent`).

use crate::language_model::{MessageContent, Role};

/// A single conversation message.
#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub content: Vec<MessageContent>,
}

impl Message {
    pub fn user(text: String) -> Self {
        Self {
            role: Role::User,
            content: vec![MessageContent::Text(text)],
        }
    }

    pub fn user_with_content(content: Vec<MessageContent>) -> Self {
        Self {
            role: Role::User,
            content,
        }
    }

    pub fn assistant(content: Vec<MessageContent>) -> Self {
        Self {
            role: Role::Assistant,
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
