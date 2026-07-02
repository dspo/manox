//! Conversation view state.
//!
//! Builds a flat `ConvItem` list from `ThreadEvent` deltas for UI rendering. The
//! `Thread` holds the canonical messages; this maintains a render-oriented view:
//! thinking and body text split into separate items, and tool calls are tracked
//! by id for status/output.

use agent::{Message, ThreadEvent, ToolCallStatus};
use agent::language_model::{LanguageModelToolResult, MessageContent, Role};

/// A single renderable conversation item.
#[derive(Debug, Clone)]
pub enum ConvItem {
    User(String),
    Assistant {
        text: String,
        streaming: bool,
    },
    Reasoning {
        text: String,
        streaming: bool,
    },
    ToolCall(ToolCallItem),
    Error(String),
}

/// A tool-call item, tracking status/output by id.
#[derive(Debug, Clone)]
pub struct ToolCallItem {
    pub id: String,
    pub name: String,
    pub title: String,
    pub status: ToolCallStatus,
    pub output: String,
    pub is_error: bool,
}

#[derive(Debug, Default)]
pub struct ConversationState {
    items: Vec<ConvItem>,
}

impl ConversationState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[ConvItem] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Append a user message.
    pub fn push_user(&mut self, text: String) {
        self.items.push(ConvItem::User(text));
    }

    fn find_tool(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|item| match item {
            ConvItem::ToolCall(t) => t.id == id,
            _ => false,
        })
    }

    /// Apply a `ThreadEvent` delta (excludes `ToolCallAuthorization`, which `Workspace` handles).
    pub fn apply(&mut self, event: &ThreadEvent) {
        match event {
            ThreadEvent::AgentText(delta) => {
                let needs_new = match self.items.last() {
                    Some(ConvItem::Assistant { streaming, .. }) => !*streaming,
                    _ => true,
                };
                if needs_new {
                    self.items.push(ConvItem::Assistant {
                        text: String::new(),
                        streaming: true,
                    });
                }
                if let Some(ConvItem::Assistant { text, .. }) = self.items.last_mut() {
                    text.push_str(delta);
                }
            }
            ThreadEvent::AgentThinking(delta) => {
                let needs_new = match self.items.last() {
                    Some(ConvItem::Reasoning { streaming, .. }) => !*streaming,
                    _ => true,
                };
                if needs_new {
                    self.items.push(ConvItem::Reasoning {
                        text: String::new(),
                        streaming: true,
                    });
                }
                if let Some(ConvItem::Reasoning { text, .. }) = self.items.last_mut() {
                    text.push_str(delta);
                }
            }
            ThreadEvent::ToolCall {
                id,
                name,
                title,
                status,
            } => {
                if let Some(ix) = self.find_tool(id) {
                    if let Some(ConvItem::ToolCall(t)) = self.items.get_mut(ix) {
                        t.title = title.clone();
                        t.status = *status;
                        t.name = name.clone();
                    }
                } else {
                    self.items.push(ConvItem::ToolCall(ToolCallItem {
                        id: id.clone(),
                        name: name.clone(),
                        title: title.clone(),
                        status: *status,
                        output: String::new(),
                        is_error: false,
                    }));
                }
            }
            ThreadEvent::ToolResult {
                id,
                output,
                is_error,
            } => {
                if let Some(ix) = self.find_tool(id) {
                    if let Some(ConvItem::ToolCall(t)) = self.items.get_mut(ix) {
                        t.output = output.clone();
                        t.is_error = *is_error;
                        t.status = if *is_error {
                            ToolCallStatus::Error
                        } else {
                            ToolCallStatus::Success
                        };
                    }
                } else {
                    // No matching ToolCall item; insert directly as a result item.
                    self.items.push(ConvItem::ToolCall(ToolCallItem {
                        id: id.clone(),
                        name: String::new(),
                        title: String::new(),
                        status: if *is_error {
                            ToolCallStatus::Error
                        } else {
                            ToolCallStatus::Success
                        },
                        output: output.clone(),
                        is_error: *is_error,
                    }));
                }
            }
            ThreadEvent::ToolCallAuthorization { .. } => {
                // Handled by `Workspace` as a prompt overlay; not part of the conversation flow.
            }
            ThreadEvent::Stop(_) => {
                for item in &mut self.items {
                    match item {
                        ConvItem::Assistant { streaming, .. }
                        | ConvItem::Reasoning { streaming, .. } => {
                            *streaming = false;
                        }
                        _ => {}
                    }
                }
            }
            ThreadEvent::Error(e) => {
                self.items.push(ConvItem::Error(e.to_string()));
            }
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Rebuild view state from a `Thread`'s canonical message list (used when loading a historical thread).
    /// Tool calls pair ToolUse with ToolResult by `tool_use_id`; an unpaired side becomes its own item.
    pub fn rebuild_from_messages(messages: &[Message]) -> Self {
        let mut state = Self::new();
        for m in messages {
            match m.role {
                Role::User => {
                    // Text becomes a user bubble; ToolResult blocks pair back to the
                    // ToolCall item emitted from the preceding assistant ToolUse.
                    // ToolResults live in user messages per the Anthropic wire contract.
                    let text: String = m
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            MessageContent::Text(t)
                            | MessageContent::Thinking { text: t, .. } => Some(t.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");
                    if !text.is_empty() {
                        state.push_user(text);
                    }
                    for c in &m.content {
                        if let MessageContent::ToolResult(tr) = c {
                            pair_tool_result(&mut state, tr);
                        }
                    }
                }
                Role::Assistant => {
                    for c in &m.content {
                        match c {
                            MessageContent::Text(t) => {
                                state.items.push(ConvItem::Assistant {
                                    text: t.clone(),
                                    streaming: false,
                                });
                            }
                            MessageContent::Thinking { text, .. } => {
                                state.items.push(ConvItem::Reasoning {
                                    text: text.clone(),
                                    streaming: false,
                                });
                            }
                            MessageContent::ToolUse(tu) => {
                                state.items.push(ConvItem::ToolCall(ToolCallItem {
                                    id: tu.id.clone(),
                                    name: tu.name.to_string(),
                                    title: tu.name.to_string(),
                                    status: ToolCallStatus::Success,
                                    output: String::new(),
                                    is_error: false,
                                }));
                            }
                            MessageContent::ToolResult(tr) => {
                                // Defensive: tool results normally live in user messages,
                                // but pair them here too if they ever appear in an assistant turn.
                                pair_tool_result(&mut state, tr);
                            }
                        }
                    }
                }
                Role::System => {}
            }
        }
        state
    }
}

/// Attach a tool_result to its matching ToolCall item by id; if none exists yet,
/// emit a standalone result item. Mirrors the live `ToolResult` event handling.
fn pair_tool_result(state: &mut ConversationState, tr: &LanguageModelToolResult) {
    let status = if tr.is_error {
        ToolCallStatus::Error
    } else {
        ToolCallStatus::Success
    };
    if let Some(ix) = state.find_tool(&tr.tool_use_id) {
        if let Some(ConvItem::ToolCall(t)) = state.items.get_mut(ix) {
            t.output = tr.content.clone();
            t.is_error = tr.is_error;
            t.status = status;
            if t.name.is_empty() {
                t.name = tr.tool_name.to_string();
            }
        }
    } else {
        state.items.push(ConvItem::ToolCall(ToolCallItem {
            id: tr.tool_use_id.clone(),
            name: tr.tool_name.to_string(),
            title: String::new(),
            status,
            output: tr.content.clone(),
            is_error: tr.is_error,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::language_model::{LanguageModelToolResult, LanguageModelToolUse};
    use agent::Message;
    use std::sync::Arc;

    /// A tool_result in a user message must pair back to the ToolUse emitted in the
    /// preceding assistant message, so a reloaded historical thread shows tool output.
    #[test]
    fn rebuild_pairs_tool_result_in_user_message() {
        let messages = vec![
            Message::user("read the file".to_string()),
            Message::assistant(vec![
                MessageContent::Text("let me read it".to_string()),
                MessageContent::ToolUse(LanguageModelToolUse {
                    id: "tu_1".to_string(),
                    name: Arc::from("read_file"),
                    raw_input: String::new(),
                    input: serde_json::Value::Null,
                    is_input_complete: true,
                    thought_signature: None,
                }),
            ]),
            Message {
                role: Role::User,
                content: vec![MessageContent::ToolResult(LanguageModelToolResult {
                    tool_use_id: "tu_1".to_string(),
                    tool_name: Arc::from("read_file"),
                    is_error: false,
                    content: "file contents here".to_string(),
                })],
            },
        ];
        let state = ConversationState::rebuild_from_messages(&messages);
        let tool = state
            .items()
            .iter()
            .find_map(|i| match i {
                ConvItem::ToolCall(t) if t.id == "tu_1" => Some(t),
                _ => None,
            })
            .expect("tool call item present");
        assert_eq!(tool.output, "file contents here");
        assert_eq!(tool.status, ToolCallStatus::Success);
        assert!(!tool.is_error);
        // The pure-toolresult user message must not render an empty user bubble.
        assert!(!state.items().iter().any(|i| matches!(i, ConvItem::User(t) if t.is_empty())));
    }

    #[test]
    fn rebuild_pairs_error_tool_result() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: "tu_x".to_string(),
                tool_name: Arc::from("bash"),
                is_error: true,
                content: "boom".to_string(),
            })],
        }];
        let state = ConversationState::rebuild_from_messages(&messages);
        let tool = state
            .items()
            .iter()
            .find_map(|i| match i {
                ConvItem::ToolCall(t) if t.id == "tu_x" => Some(t),
                _ => None,
            })
            .expect("standalone result item present");
        assert_eq!(tool.output, "boom");
        assert_eq!(tool.status, ToolCallStatus::Error);
        assert!(tool.is_error);
        assert_eq!(tool.name, "bash");
    }
}
