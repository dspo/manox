//! Conversation view state.
//!
//! Builds a flat `ConvItem` list from `ThreadEvent` deltas for UI rendering. The
//! `Thread` holds the canonical messages; this maintains a render-oriented view:
//! thinking and body text split into separate items, and tool calls are tracked
//! by id for status/output.

use agent::language_model::{LanguageModelToolResult, MessageContent, Role};
use agent::tools::agent::{agent_final_text, agent_sub_messages};
use agent::{Message, ThreadEvent, ToolCallStatus};

/// A single renderable conversation item.
#[derive(Debug, Clone)]
pub enum ConvItem {
    User(String),
    Assistant { text: String, streaming: bool },
    Reasoning { text: String, streaming: bool },
    ToolCall(ToolCallItem),
    AgentTask(AgentTaskItem),
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
    /// True while live `ToolOutput` chunks are still streaming in; flipped to
    /// false once the final `ToolResult` lands the canonical output.
    pub streaming: bool,
}

/// A sub-agent (`agent` tool) invocation. The child `Thread`'s streamed text
/// accumulates in `sub_text` for the collapsed live tail; the full child
/// conversation lands in `sub_messages` (via the parent's snapshot) for the
/// expandable panel. `final_text` is what the parent model received as the
/// tool result.
#[derive(Debug, Clone)]
pub struct AgentTaskItem {
    pub id: String,
    pub title: String,
    pub status: ToolCallStatus,
    pub streaming: bool,
    pub sub_text: String,
    pub sub_messages: Vec<Message>,
    pub final_text: String,
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

    fn find_agent_task(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|item| match item {
            ConvItem::AgentTask(t) => t.id == id,
            _ => false,
        })
    }

    /// Feed the child `Thread`'s full message list into the matching agent task,
    /// populating the expandable sub-conversation panel. Called by `Workspace`
    /// after the parent's `subagent_snapshots` is updated.
    pub fn set_agent_sub_messages(&mut self, id: &str, messages: Vec<Message>) {
        if let Some(ix) = self.find_agent_task(id)
            && let Some(ConvItem::AgentTask(t)) = self.items.get_mut(ix)
        {
            t.sub_messages = messages;
        }
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
                if name == "agent" {
                    // Sub-agent invocations render as AgentTask cards, not plain
                    // tool calls, so they get the expandable sub-conversation panel.
                    if let Some(ix) = self.find_agent_task(id) {
                        if let Some(ConvItem::AgentTask(t)) = self.items.get_mut(ix) {
                            t.title = title.clone();
                            t.status = *status;
                        }
                    } else {
                        self.items.push(ConvItem::AgentTask(AgentTaskItem {
                            id: id.clone(),
                            title: title.clone(),
                            status: *status,
                            streaming: matches!(*status, ToolCallStatus::Running),
                            sub_text: String::new(),
                            sub_messages: Vec::new(),
                            final_text: String::new(),
                            is_error: false,
                        }));
                    }
                } else if let Some(ix) = self.find_tool(id) {
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
                        streaming: matches!(*status, ToolCallStatus::Running),
                    }));
                }
            }
            ThreadEvent::ToolOutput { id, chunk } => {
                if let Some(ix) = self.find_agent_task(id) {
                    if let Some(ConvItem::AgentTask(t)) = self.items.get_mut(ix) {
                        t.sub_text.push_str(chunk);
                        t.streaming = true;
                    }
                } else if let Some(ix) = self.find_tool(id)
                    && let Some(ConvItem::ToolCall(t)) = self.items.get_mut(ix)
                {
                    t.output.push_str(chunk);
                    t.streaming = true;
                }
            }
            ThreadEvent::ToolResult {
                id,
                output,
                is_error,
            } => {
                let status = if *is_error {
                    ToolCallStatus::Error
                } else {
                    ToolCallStatus::Success
                };
                if let Some(ix) = self.find_agent_task(id) {
                    if let Some(ConvItem::AgentTask(t)) = self.items.get_mut(ix) {
                        // The live event carries the JSON envelope; extract the
                        // final text for the collapsed view. `sub_messages` is
                        // filled separately from the in-memory snapshot by the
                        // workspace, so don't touch it here.
                        t.final_text = agent_final_text(output);
                        t.is_error = *is_error;
                        t.streaming = false;
                        t.status = status;
                    }
                } else if let Some(ix) = self.find_tool(id) {
                    if let Some(ConvItem::ToolCall(t)) = self.items.get_mut(ix) {
                        t.output = output.clone();
                        t.is_error = *is_error;
                        t.streaming = false;
                        t.status = status;
                    }
                } else {
                    // No matching ToolCall item; insert directly as a result item.
                    self.items.push(ConvItem::ToolCall(ToolCallItem {
                        id: id.clone(),
                        name: String::new(),
                        title: String::new(),
                        status,
                        output: output.clone(),
                        is_error: *is_error,
                        streaming: false,
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
                        ConvItem::ToolCall(t) => t.streaming = false,
                        ConvItem::AgentTask(t) => t.streaming = false,
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
                    let text: String =
                        m.content
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
                                if tu.name.as_ref() == "agent" {
                                    let title =
                                        agent::thread::tool_title(tu.name.as_ref(), &tu.input);
                                    state.items.push(ConvItem::AgentTask(AgentTaskItem {
                                        id: tu.id.clone(),
                                        title,
                                        status: ToolCallStatus::Success,
                                        streaming: false,
                                        sub_text: String::new(),
                                        sub_messages: Vec::new(),
                                        final_text: String::new(),
                                        is_error: false,
                                    }));
                                } else {
                                    state.items.push(ConvItem::ToolCall(ToolCallItem {
                                        id: tu.id.clone(),
                                        name: tu.name.to_string(),
                                        title: tu.name.to_string(),
                                        status: ToolCallStatus::Success,
                                        output: String::new(),
                                        is_error: false,
                                        streaming: false,
                                    }));
                                }
                            }
                            MessageContent::ToolResult(tr) => {
                                // Defensive: tool results normally live in user messages,
                                // but pair them here too if they ever appear in an assistant turn.
                                pair_tool_result(&mut state, tr);
                            }
                            MessageContent::Image { .. } => {}
                        }
                    }
                }
                Role::System => {}
            }
        }
        state
    }
}

/// Attach a tool_result to its matching item by id. Sub-agent results land in
/// `AgentTaskItem::final_text`; ordinary tool results land in `ToolCallItem::output`.
/// If no match exists, emit a standalone ToolCall result item.
fn pair_tool_result(state: &mut ConversationState, tr: &LanguageModelToolResult) {
    let status = if tr.is_error {
        ToolCallStatus::Error
    } else {
        ToolCallStatus::Success
    };
    if let Some(ix) = state.find_agent_task(&tr.tool_use_id) {
        if let Some(ConvItem::AgentTask(t)) = state.items.get_mut(ix) {
            // On reload the in-memory snapshot map is empty, so restore the
            // sub-conversation from the persisted JSON envelope.
            t.final_text = agent_final_text(&tr.content);
            t.sub_messages = agent_sub_messages(&tr.content).unwrap_or_default();
            t.is_error = tr.is_error;
            t.status = status;
        }
        return;
    }
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
            streaming: false,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::Message;
    use agent::language_model::{LanguageModelToolResult, LanguageModelToolUse};
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
        assert!(
            !state
                .items()
                .iter()
                .any(|i| matches!(i, ConvItem::User(t) if t.is_empty()))
        );
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

    /// A reloaded `agent` tool call must restore both its final text and the
    /// sub-conversation from the persisted JSON envelope (the in-memory snapshot
    /// map is empty after restart, so the envelope is the only source).
    #[test]
    fn rebuild_restores_agent_sub_messages_from_envelope() {
        let sub_messages = vec![
            Message::user("research the foo module".to_string()),
            Message::assistant(vec![MessageContent::Text("found 3 files".to_string())]),
        ];
        let envelope = serde_json::json!({
            "final": "found 3 files",
            "messages": sub_messages,
        })
        .to_string();
        let messages = vec![
            Message::assistant(vec![MessageContent::ToolUse(LanguageModelToolUse {
                id: "tu_agent".to_string(),
                name: Arc::from("agent"),
                raw_input: String::new(),
                input: serde_json::json!({"subagent_type": "researcher", "prompt": "research foo"}),
                is_input_complete: true,
                thought_signature: None,
            })]),
            Message {
                role: Role::User,
                content: vec![MessageContent::ToolResult(LanguageModelToolResult {
                    tool_use_id: "tu_agent".to_string(),
                    tool_name: Arc::from("agent"),
                    is_error: false,
                    content: envelope,
                })],
            },
        ];
        let state = ConversationState::rebuild_from_messages(&messages);
        let task = state
            .items()
            .iter()
            .find_map(|i| match i {
                ConvItem::AgentTask(t) if t.id == "tu_agent" => Some(t),
                _ => None,
            })
            .expect("agent task item present");
        assert_eq!(task.final_text, "found 3 files");
        assert_eq!(task.sub_messages.len(), 2);
        assert_eq!(task.sub_messages[1].content.len(), 1);
    }

    /// A legacy `agent` tool result (plain text, no JSON envelope) must still
    /// render its final text without panicking.
    #[test]
    fn agent_final_text_falls_back_for_legacy_content() {
        assert_eq!(
            agent_final_text("just a plain summary"),
            "just a plain summary"
        );
        assert_eq!(agent_final_text("not json { at all"), "not json { at all");
        assert!(agent_sub_messages("plain text").is_none());
    }
}
