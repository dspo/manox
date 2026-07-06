//! Conversation view state.
//!
//! A gpui `Entity` holding one `Entity<MessageItem>` per conversation item.
//! `Thread` holds the canonical messages; this maintains a render-oriented
//! view: thinking and body text split into separate items, and tool calls are
//! tracked by id for status/output. Each item lives in its own `Entity` so a
//! streaming delta notifies (and re-renders) only that item, leaving already-
//! finished items' markdown untouched.

use agent::{Message, ThreadEvent, TokenUsage, ToolCallStatus};
use gpui::{App, AppContext as _, Entity, WeakEntity};

use crate::Workspace;
use crate::views::message::{MessageItem, build_items};

/// A single renderable conversation item.
#[derive(Debug, Clone)]
pub enum ConvItem {
    User(String),
    Assistant {
        text: String,
        streaming: bool,
        /// Per-turn token usage (input/output/cache) for the user message that
        /// preceded this assistant reply. Populated on turn `Stop`; `None`
        /// while streaming or when the provider didn't report usage.
        token_usage: Option<TokenUsage>,
    },
    Reasoning {
        text: String,
        streaming: bool,
        collapsed: bool,
        user_toggled: bool,
    },
    ToolCall(ToolCallItem),
    AgentTask(AgentTaskItem),
    /// A runtime error from the agent (red danger styling).
    Error(String),
    /// An ephemeral system notice — status changes, slash-command acks, etc.
    /// Rendered with neutral tones, not danger colors.
    Notice(String),
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
    /// True ⇒ body hidden. Auto-flipped to true on terminal status (Success /
    /// Error / Denied) unless `user_toggled` is set, so a completed tool call
    /// collapses back to a single-line card. While `streaming` is true the
    /// body is always shown regardless of this flag.
    pub collapsed: bool,
    /// Becomes true the first time the user clicks the card header. Once
    /// set, the auto-collapse logic stops touching `collapsed` so the user's
    /// manual choice survives subsequent status transitions within the same
    /// tool call.
    pub user_toggled: bool,
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

/// What `apply` did to the item list, so the caller can keep the `ListState`
/// in sync (splice on append, remeasure on in-place mutation).
pub enum ApplyOutcome {
    /// No item touched (e.g. `ToolCallAuthorization`).
    None,
    /// An existing item's content changed at `index`; remeasure that item.
    Remeasure(usize),
    /// A new item was appended at the end; splice the list count up.
    Appended,
    /// Every item may have changed (terminal `Stop`); remeasure all.
    All,
}

#[derive(Debug, Default)]
pub struct ConversationState {
    items: Vec<Entity<MessageItem>>,
}

impl ConversationState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn items(&self) -> &[Entity<MessageItem>] {
        &self.items
    }

    /// True when the conversation has no substantive items (user, assistant,
    /// reasoning, tool call, or agent task). Notice-only items (error cards
    /// used for slash-command acknowledgements and mode switches) don't count
    /// so toggling YOLO on the empty first screen doesn't prematurely leave
    /// the hero layout.
    pub fn is_empty(&self, cx: &App) -> bool {
        self.items
            .iter()
            .all(|e| matches!(e.read(cx).kind(), ConvItem::Error(_) | ConvItem::Notice(_)))
    }

    /// Append a user message.
    pub fn push_user(
        &mut self,
        text: String,
        role: &str,
        weak: WeakEntity<Workspace>,
        cx: &mut App,
    ) {
        let id = self.items.len();
        self.items
            .push(cx.new(|_| MessageItem::new(ConvItem::User(text), role.to_string(), id, weak)));
    }

    /// Append a system-styled notice. Does not touch the canonical `Thread`
    /// messages — UI-only, for slash-command acknowledgements and similar
    /// ephemeral notices.
    pub fn push_notice(&mut self, text: String, weak: WeakEntity<Workspace>, cx: &mut App) {
        let id = self.items.len();
        self.items
            .push(cx.new(|_| MessageItem::new(ConvItem::Notice(text), String::new(), id, weak)));
    }

    pub fn find_tool(&self, id: &str, cx: &App) -> Option<usize> {
        self.items
            .iter()
            .position(|e| matches!(e.read(cx).kind(), ConvItem::ToolCall(t) if t.id == id))
    }

    fn find_agent_task(&self, id: &str, cx: &App) -> Option<usize> {
        self.items
            .iter()
            .position(|e| matches!(e.read(cx).kind(), ConvItem::AgentTask(t) if t.id == id))
    }

    /// Feed the child `Thread`'s full message list into the matching agent task,
    /// populating the expandable sub-conversation panel. Returns the item index
    /// so the caller can remeasure it; `None` if no matching task was found.
    pub fn set_agent_sub_messages(
        &mut self,
        id: &str,
        messages: Vec<Message>,
        cx: &mut App,
    ) -> Option<usize> {
        let ix = self.find_agent_task(id, cx)?;
        self.items[ix].update(cx, |item, cx| {
            if let ConvItem::AgentTask(t) = item.kind_mut() {
                t.sub_messages = messages;
            }
            cx.notify();
        });
        Some(ix)
    }

    /// Apply a `ThreadEvent` delta (excludes `ToolCallAuthorization`, which `Workspace` handles).
    /// `last_request_usage` is the token usage for the turn's last user message;
    /// consumed only on `Stop` to label the just-finished assistant reply.
    pub fn apply(
        &mut self,
        event: &ThreadEvent,
        role: &str,
        last_request_usage: Option<TokenUsage>,
        weak: WeakEntity<Workspace>,
        cx: &mut App,
    ) -> ApplyOutcome {
        match event {
            // Backfill plan text into the matching exit_plan_mode ToolCall
            // item so it renders as markdown in the chat view. The ToolCall
            // event (PendingApproval) always arrives before PlanProposed.
            ThreadEvent::PlanProposed { id, plan_text } => {
                if let Some(ix) = self.find_tool(id, cx) {
                    self.items[ix].update(cx, |item, cx| {
                        if let ConvItem::ToolCall(t) = item.kind_mut() {
                            t.output = plan_text.clone();
                        }
                        cx.notify();
                    });
                    ApplyOutcome::Remeasure(ix)
                } else {
                    ApplyOutcome::None
                }
            }
            // Token usage + model changes are surfaced elsewhere (sidebar /
            // assistant footer / model-history overlay). No conversation item.
            ThreadEvent::TokenUsageUpdated(_) | ThreadEvent::ModelChanged { .. } => {
                ApplyOutcome::None
            }
            // `TurnStarted` is a UI-only signal routed to `ThreadStore` by the
            // workspace to light the sidebar running indicator; it carries no
            // conversation content.
            ThreadEvent::TurnStarted => ApplyOutcome::None,
            ThreadEvent::AgentText(delta) => {
                let needs_new = match self.items.last() {
                    Some(e) => !matches!(
                        e.read(cx).kind(),
                        ConvItem::Assistant {
                            streaming: true,
                            ..
                        }
                    ),
                    None => true,
                };
                if needs_new {
                    let id = self.items.len();
                    self.items.push(cx.new(|_| {
                        MessageItem::new(
                            ConvItem::Assistant {
                                text: delta.clone(),
                                streaming: true,
                                token_usage: None,
                            },
                            role.to_string(),
                            id,
                            weak,
                        )
                    }));
                    ApplyOutcome::Appended
                } else {
                    let ix = self.items.len() - 1;
                    self.items[ix].update(cx, |item, cx| {
                        if let ConvItem::Assistant { text, .. } = item.kind_mut() {
                            text.push_str(delta);
                        }
                        cx.notify();
                    });
                    ApplyOutcome::Remeasure(ix)
                }
            }
            ThreadEvent::AgentThinking(delta) => {
                let needs_new = match self.items.last() {
                    Some(e) => !matches!(
                        e.read(cx).kind(),
                        ConvItem::Reasoning {
                            streaming: true,
                            ..
                        }
                    ),
                    None => true,
                };
                if needs_new {
                    let id = self.items.len();
                    self.items.push(cx.new(|_| {
                        MessageItem::new(
                            ConvItem::Reasoning {
                                text: delta.clone(),
                                streaming: true,
                                collapsed: false,
                                user_toggled: false,
                            },
                            role.to_string(),
                            id,
                            weak,
                        )
                    }));
                    ApplyOutcome::Appended
                } else {
                    let ix = self.items.len() - 1;
                    self.items[ix].update(cx, |item, cx| {
                        if let ConvItem::Reasoning { text, .. } = item.kind_mut() {
                            text.push_str(delta);
                        }
                        cx.notify();
                    });
                    ApplyOutcome::Remeasure(ix)
                }
            }
            ThreadEvent::ToolCall {
                id,
                name,
                title,
                status,
            } => {
                if name == "agent" {
                    if let Some(ix) = self.find_agent_task(id, cx) {
                        self.items[ix].update(cx, |item, cx| {
                            if let ConvItem::AgentTask(t) = item.kind_mut() {
                                t.title = title.clone();
                                t.status = *status;
                            }
                            cx.notify();
                        });
                        ApplyOutcome::Remeasure(ix)
                    } else {
                        let ix = self.items.len();
                        self.items.push(cx.new(|_| {
                            MessageItem::new(
                                ConvItem::AgentTask(AgentTaskItem {
                                    id: id.clone(),
                                    title: title.clone(),
                                    status: *status,
                                    streaming: matches!(*status, ToolCallStatus::Running),
                                    sub_text: String::new(),
                                    sub_messages: Vec::new(),
                                    final_text: String::new(),
                                    is_error: false,
                                }),
                                role.to_string(),
                                ix,
                                weak,
                            )
                        }));
                        ApplyOutcome::Appended
                    }
                } else if let Some(ix) = self.find_tool(id, cx) {
                    self.items[ix].update(cx, |item, cx| {
                        if let ConvItem::ToolCall(t) = item.kind_mut() {
                            t.title = title.clone();
                            t.status = *status;
                            t.name = name.clone();
                            // Reaching a terminal status flips collapse on —
                            // matches the same flip in the ToolResult branch
                            // for tools whose result event lands first.
                            if matches!(
                                *status,
                                ToolCallStatus::Success
                                    | ToolCallStatus::Error
                                    | ToolCallStatus::Denied
                            ) && !t.streaming
                            {
                                t.collapsed = !t.user_toggled;
                            }
                        }
                        cx.notify();
                    });
                    ApplyOutcome::Remeasure(ix)
                } else {
                    let ix = self.items.len();
                    self.items.push(cx.new(|_| {
                        MessageItem::new(
                            ConvItem::ToolCall(ToolCallItem {
                                id: id.clone(),
                                name: name.clone(),
                                title: title.clone(),
                                status: *status,
                                output: String::new(),
                                is_error: false,
                                streaming: matches!(*status, ToolCallStatus::Running),
                                collapsed: false,
                                user_toggled: false,
                            }),
                            role.to_string(),
                            ix,
                            weak,
                        )
                    }));
                    ApplyOutcome::Appended
                }
            }
            ThreadEvent::ToolOutput { id, chunk } => {
                if let Some(ix) = self.find_agent_task(id, cx) {
                    self.items[ix].update(cx, |item, cx| {
                        if let ConvItem::AgentTask(t) = item.kind_mut() {
                            t.sub_text.push_str(chunk);
                            t.streaming = true;
                        }
                        cx.notify();
                    });
                    ApplyOutcome::Remeasure(ix)
                } else if let Some(ix) = self.find_tool(id, cx) {
                    self.items[ix].update(cx, |item, cx| {
                        if let ConvItem::ToolCall(t) = item.kind_mut() {
                            t.output.push_str(chunk);
                            t.streaming = true;
                        }
                        cx.notify();
                    });
                    ApplyOutcome::Remeasure(ix)
                } else {
                    ApplyOutcome::None
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
                if let Some(ix) = self.find_agent_task(id, cx) {
                    self.items[ix].update(cx, |item, cx| {
                        if let ConvItem::AgentTask(t) = item.kind_mut() {
                            // The live event carries the JSON envelope; extract the
                            // final text for the collapsed view. `sub_messages` is
                            // filled separately from the in-memory snapshot by the
                            // workspace, so don't touch it here.
                            t.final_text = agent::tools::agent::agent_final_text(output);
                            t.is_error = *is_error;
                            t.streaming = false;
                            t.status = status;
                        }
                        cx.notify();
                    });
                    ApplyOutcome::Remeasure(ix)
                } else if let Some(ix) = self.find_tool(id, cx) {
                    self.items[ix].update(cx, |item, cx| {
                        if let ConvItem::ToolCall(t) = item.kind_mut() {
                            t.output = output.clone();
                            t.is_error = *is_error;
                            t.streaming = false;
                            t.status = status;
                            // Auto-collapse once the tool call reaches a terminal
                            // status. Preserves the user's manual choice if any.
                            t.collapsed = !t.user_toggled;
                        }
                        cx.notify();
                    });
                    ApplyOutcome::Remeasure(ix)
                } else {
                    // No matching ToolCall item; insert directly as a result item.
                    let ix = self.items.len();
                    self.items.push(cx.new(|_| {
                        MessageItem::new(
                            ConvItem::ToolCall(ToolCallItem {
                                id: id.clone(),
                                name: String::new(),
                                title: String::new(),
                                status,
                                output: output.clone(),
                                is_error: *is_error,
                                streaming: false,
                                collapsed: !matches!(
                                    status,
                                    ToolCallStatus::Running | ToolCallStatus::PendingApproval
                                ),
                                user_toggled: false,
                            }),
                            role.to_string(),
                            ix,
                            weak,
                        )
                    }));
                    ApplyOutcome::Appended
                }
            }
            ThreadEvent::ToolCallAuthorization { .. } => {
                // Handled by `Workspace` as a prompt overlay; not part of the conversation flow.
                ApplyOutcome::None
            }
            ThreadEvent::Stop(_) => {
                for e in &self.items {
                    e.update(cx, |item, cx| {
                        item.finalize_streaming();
                        cx.notify();
                    });
                }
                // Stamp the per-turn usage onto the last assistant reply so its
                // footer can show input/output/cache totals for this turn. Walk
                // backward: the last item may be a tool call or reasoning block
                // emitted after the assistant text, not the assistant itself.
                if let Some(usage) = last_request_usage {
                    for e in self.items.iter().rev() {
                        let stamped = e.update(cx, |item, _cx| {
                            if let ConvItem::Assistant { token_usage, .. } = item.kind_mut() {
                                *token_usage = Some(usage);
                                true
                            } else {
                                false
                            }
                        });
                        if stamped {
                            e.update(cx, |_, cx| cx.notify());
                            break;
                        }
                    }
                }
                ApplyOutcome::All
            }
            ThreadEvent::Error(e) => {
                let ix = self.items.len();
                self.items.push(cx.new(|_| {
                    MessageItem::new(ConvItem::Error(e.to_string()), role.to_string(), ix, weak)
                }));
                ApplyOutcome::Appended
            }
            ThreadEvent::YoloToggled { .. } => {
                // UI state (badge/chip) handled by `Workspace`; not a conversation item.
                ApplyOutcome::None
            }
            ThreadEvent::PrefixStability { .. } => {
                // UI state (cache chip) handled by `Workspace`; not a conversation item.
                ApplyOutcome::None
            }
        }
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Rebuild view state from a `Thread`'s canonical message list (used when loading a historical thread).
    pub fn rebuild_from_messages(
        messages: &[Message],
        usage: &std::collections::HashMap<String, TokenUsage>,
        role: &str,
        weak: WeakEntity<Workspace>,
        cx: &mut App,
    ) -> Self {
        let plain = build_items(messages, usage);
        let items = plain
            .into_iter()
            .enumerate()
            .map(|(id, kind)| {
                cx.new(|_| MessageItem::new(kind, role.to_string(), id, weak.clone()))
            })
            .collect();
        Self { items }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::Message;
    use agent::language_model::{LanguageModelToolResult, LanguageModelToolUse, MessageContent};
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
            Message::user_with_content(vec![MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: "tu_1".to_string(),
                tool_name: Arc::from("read_file"),
                is_error: false,
                content: "file contents here".to_string(),
            })]),
        ];
        let items = build_items(&messages, &std::collections::HashMap::new());
        let tool = items
            .iter()
            .find_map(|i| match i {
                ConvItem::ToolCall(t) if t.id == "tu_1" => Some(t),
                _ => None,
            })
            .expect("tool call item present");
        assert_eq!(tool.output, "file contents here");
        assert_eq!(tool.status, ToolCallStatus::Success);
        assert!(!tool.is_error);
        assert!(
            !items
                .iter()
                .any(|i| matches!(i, ConvItem::User(t) if t.is_empty()))
        );
    }

    #[test]
    fn rebuild_pairs_error_tool_result() {
        let messages = vec![Message::user_with_content(vec![
            MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: "tu_x".to_string(),
                tool_name: Arc::from("bash"),
                is_error: true,
                content: "boom".to_string(),
            }),
        ])];
        let items = build_items(&messages, &std::collections::HashMap::new());
        let tool = items
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
            Message::user_with_content(vec![MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: "tu_agent".to_string(),
                tool_name: Arc::from("agent"),
                is_error: false,
                content: envelope,
            })]),
        ];
        let items = build_items(&messages, &std::collections::HashMap::new());
        let task = items
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
            agent::tools::agent::agent_final_text("just a plain summary"),
            "just a plain summary"
        );
        assert_eq!(
            agent::tools::agent::agent_final_text("not json { at all"),
            "not json { at all"
        );
        assert!(agent::tools::agent::agent_sub_messages("plain text").is_none());
    }
}
