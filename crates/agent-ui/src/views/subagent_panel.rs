//! Read-only observation panel and persisted navigation metadata for sub-agents.

use std::collections::HashMap;

use agent::language_model::{MessageContent, StopReason, TokenUsage};
use agent::tools::agent::{agent_metrics, agent_sub_messages};
use agent::{Message, Thread, ThreadEvent, ToolCallStatus, i18n};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, Pixels, Render, ScrollHandle, SharedString, Subscription,
    WeakEntity, Window, px,
};
use gpui_component::{
    ActiveTheme as _, ElementExt as _, Icon, IconName, Sizable as _, Theme, h_flex, v_flex,
};

use crate::Workspace;
use crate::conversation::{ApplyCtx, ConversationState, agent_task_labels};
use crate::views::braille_spinner::BrailleSpinner;

#[derive(Clone, Debug)]
pub(crate) struct SubagentInfo {
    pub id: String,
    pub parent_id: Option<String>,
    pub subagent_type: String,
    pub description: String,
    pub status: ToolCallStatus,
}

#[derive(Clone, Debug)]
pub(crate) struct SubagentSnapshot {
    pub info: SubagentInfo,
    pub messages: Vec<Message>,
}

pub(crate) fn subagent_display_title(info: &SubagentInfo) -> String {
    if info.description.is_empty() {
        info.subagent_type.clone()
    } else if info.subagent_type.is_empty() {
        info.description.clone()
    } else {
        format!("{} · {}", info.subagent_type, info.description)
    }
}

pub(crate) fn status_indicator(status: ToolCallStatus, theme: &Theme) -> AnyElement {
    match status {
        ToolCallStatus::PendingApproval | ToolCallStatus::Running => BrailleSpinner::new()
            .xsmall()
            .color(theme.accent)
            .into_any_element(),
        ToolCallStatus::Success | ToolCallStatus::Continued => Icon::new(IconName::CircleCheck)
            .xsmall()
            .text_color(theme.success)
            .into_any_element(),
        ToolCallStatus::Error | ToolCallStatus::Denied => Icon::new(IconName::CircleX)
            .xsmall()
            .text_color(theme.danger)
            .into_any_element(),
        ToolCallStatus::Cancelled => Icon::new(IconName::Minus)
            .xsmall()
            .text_color(theme.muted_foreground)
            .into_any_element(),
    }
}

/// Recursively recover sub-agent navigation entries from persisted Agent tool
/// results. The result envelope already owns the child messages, so no database
/// schema or additional UI-note persistence is needed.
pub(crate) fn snapshots_from_messages(messages: &[Message]) -> Vec<SubagentSnapshot> {
    fn visit(messages: &[Message], parent_id: Option<&str>, out: &mut Vec<SubagentSnapshot>) {
        let mut entries: HashMap<String, usize> = HashMap::new();
        for message in messages {
            for content in &message.content {
                match content {
                    MessageContent::ToolUse(tool) if tool.name.as_ref() == "Agent" => {
                        let (subagent_type, description) = agent_task_labels(&tool.input);
                        let ix = out.len();
                        out.push(SubagentSnapshot {
                            info: SubagentInfo {
                                id: tool.id.clone(),
                                parent_id: parent_id.map(str::to_string),
                                subagent_type,
                                description,
                                status: ToolCallStatus::Cancelled,
                            },
                            messages: Vec::new(),
                        });
                        entries.insert(tool.id.clone(), ix);
                    }
                    MessageContent::ToolResult(result) => {
                        let Some(&ix) = entries.get(&result.tool_use_id) else {
                            continue;
                        };
                        let child_messages =
                            agent_sub_messages(&result.content).unwrap_or_default();
                        let status = agent_metrics(&result.content)
                            .and_then(|metrics| metrics.status)
                            .unwrap_or(if result.is_error {
                                ToolCallStatus::Error
                            } else {
                                ToolCallStatus::Success
                            });
                        out[ix].info.status = status;
                        out[ix].messages = child_messages.clone();
                        let id = out[ix].info.id.clone();
                        visit(&child_messages, Some(&id), out);
                    }
                    _ => {}
                }
            }
        }
    }

    let mut out = Vec::new();
    visit(messages, None, &mut out);
    out
}

pub(crate) struct SubagentPanel {
    info: SubagentInfo,
    child: Option<Entity<Thread>>,
    conversation: Entity<ConversationState>,
    subscription: Option<Subscription>,
    scroll_handle: ScrollHandle,
    stick_to_bottom: bool,
}

impl SubagentPanel {
    pub(crate) fn live(
        child: Entity<Thread>,
        root_thread_id: String,
        info: SubagentInfo,
        weak_workspace: WeakEntity<Workspace>,
        cx: &mut App,
    ) -> Entity<Self> {
        let messages = child.read(cx).messages().to_vec();
        let conversation = build_conversation(
            &child,
            &messages,
            &info.subagent_type,
            weak_workspace.clone(),
            cx,
        );
        let panel = cx.new(|_| Self {
            info,
            child: Some(child.clone()),
            conversation,
            subscription: None,
            scroll_handle: ScrollHandle::new(),
            stick_to_bottom: true,
        });

        panel.update(cx, |this, cx| {
            let subscription =
                cx.subscribe(&child, move |this, _child, event: &ThreadEvent, cx| {
                    match event {
                        ThreadEvent::TurnStarted => this.info.status = ToolCallStatus::Running,
                        ThreadEvent::Stop(
                            StopReason::EndTurn | StopReason::MaxTokens | StopReason::Refusal,
                        ) => {
                            this.info.status = ToolCallStatus::Success;
                        }
                        ThreadEvent::Error(_) => this.info.status = ToolCallStatus::Error,
                        ThreadEvent::TurnFinished {
                            cancelled: true, ..
                        } => {
                            this.info.status = ToolCallStatus::Cancelled;
                        }
                        ThreadEvent::SubagentStarted {
                            id,
                            subagent_type,
                            description,
                            child,
                        } => {
                            let parent_id = this.info.id.clone();
                            let _ = weak_workspace.update(cx, |workspace, cx| {
                                workspace.register_live_subagent(
                                    root_thread_id.clone(),
                                    SubagentInfo {
                                        id: id.clone(),
                                        parent_id: Some(parent_id),
                                        subagent_type: subagent_type.clone(),
                                        description: description.clone(),
                                        status: ToolCallStatus::Running,
                                    },
                                    child.clone(),
                                    cx,
                                );
                            });
                        }
                        ThreadEvent::SubagentProgress { id, status, .. } => {
                            let _ = weak_workspace.update(cx, |workspace, cx| {
                                workspace.update_subagent_status(&root_thread_id, id, *status, cx);
                            });
                        }
                        _ => {}
                    }

                    let role = this.info.subagent_type.clone();
                    let cwd = this
                        .child
                        .as_ref()
                        .and_then(|thread| thread_cwd(thread, cx));
                    this.conversation.update(cx, |conversation, cx| {
                        conversation.apply(
                            event,
                            &role,
                            None,
                            ApplyCtx {
                                weak: weak_workspace.clone(),
                                cwd,
                            },
                            cx,
                        )
                    });
                    cx.notify();
                });
            this.subscription = Some(subscription);
        });

        panel
    }

    pub(crate) fn snapshot(
        snapshot: SubagentSnapshot,
        weak_workspace: WeakEntity<Workspace>,
        cx: &mut App,
    ) -> Entity<Self> {
        let empty_usage: HashMap<String, TokenUsage> = HashMap::new();
        let role = snapshot.info.subagent_type.clone();
        let conversation = cx.new(|cx| {
            ConversationState::rebuild_from_messages(
                &snapshot.messages,
                &empty_usage,
                &role,
                false,
                &[],
                ApplyCtx {
                    weak: weak_workspace,
                    cwd: None,
                },
                cx,
            )
        });
        cx.new(|_| Self {
            info: snapshot.info,
            child: None,
            conversation,
            subscription: None,
            scroll_handle: ScrollHandle::new(),
            stick_to_bottom: true,
        })
    }

    pub(crate) fn set_status(&mut self, status: ToolCallStatus, cx: &mut Context<Self>) {
        self.info.status = status;
        cx.notify();
    }

    fn render_header(&self, theme: &Theme) -> impl IntoElement {
        h_flex()
            .w_full()
            .min_w_0()
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .child(status_indicator(self.info.status, theme))
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(subagent_display_title(&self.info)),
            )
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(i18n::t("subagent-panel-read-only")),
            )
    }
}

impl Render for SubagentPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        if !self.stick_to_bottom {
            let max = self.scroll_handle.max_offset().y;
            let offset = self.scroll_handle.offset().y;
            self.stick_to_bottom = max <= px(0.5) || offset <= -max + px(1.);
        }
        let items: Vec<AnyElement> = self
            .conversation
            .read(cx)
            .items()
            .iter()
            .cloned()
            .map(|item| {
                v_flex()
                    .pt_1()
                    .pb_4()
                    .flex_shrink_0()
                    .min_w_0()
                    .child(item)
                    .into_any_element()
            })
            .collect();
        let scroll = self.scroll_handle.clone();
        let weak = cx.weak_entity();

        v_flex()
            .h_full()
            .w_full()
            .min_w_0()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(self.render_header(&theme))
            .child(
                v_flex()
                    .id(SharedString::from(format!(
                        "subagent-msg-scroll-{}",
                        self.info.id
                    )))
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_y_scroll()
                    .overflow_x_hidden()
                    .track_scroll(&scroll)
                    .children(items)
                    .on_prepaint(move |_bounds, _window, cx| {
                        let Some(this) = weak.upgrade() else {
                            return;
                        };
                        if !this.read(cx).stick_to_bottom {
                            return;
                        }
                        let offset = scroll.offset().y;
                        let max = scroll.max_offset().y;
                        if (max + offset).abs() >= px(1.) {
                            scroll.scroll_to_bottom();
                        }
                    })
                    .on_scroll_wheel(cx.listener(
                        |this, event: &gpui::ScrollWheelEvent, window, cx| {
                            let delta = event.delta.pixel_delta(window.line_height()).y;
                            if delta > Pixels::ZERO {
                                this.stick_to_bottom = false;
                                cx.notify();
                            }
                        },
                    )),
            )
    }
}

fn build_conversation(
    child: &Entity<Thread>,
    messages: &[Message],
    role: &str,
    weak_workspace: WeakEntity<Workspace>,
    cx: &mut App,
) -> Entity<ConversationState> {
    let empty_usage: HashMap<String, TokenUsage> = HashMap::new();
    let cwd = thread_cwd(child, cx);
    cx.new(|cx| {
        ConversationState::rebuild_from_messages(
            messages,
            &empty_usage,
            role,
            child.read(cx).is_running(),
            &[],
            ApplyCtx {
                weak: weak_workspace,
                cwd,
            },
            cx,
        )
    })
}

fn thread_cwd(thread: &Entity<Thread>, cx: &App) -> Option<SharedString> {
    let path = thread.read(cx).cwd();
    (!path.as_os_str().is_empty()).then(|| SharedString::from(path.to_string_lossy().to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use agent::language_model::{LanguageModelToolResult, LanguageModelToolUse};

    use super::*;

    fn agent_use(id: &str, subagent_type: &str, description: &str) -> MessageContent {
        MessageContent::ToolUse(LanguageModelToolUse {
            id: id.to_string(),
            name: Arc::from("Agent"),
            raw_input: String::new(),
            input: serde_json::json!({
                "subagent_type": subagent_type,
                "description": description,
                "prompt": "full delegated task"
            }),
            is_input_complete: true,
            thought_signature: None,
        })
    }

    fn agent_result(
        id: &str,
        messages: Vec<Message>,
        status: ToolCallStatus,
        is_error: bool,
    ) -> MessageContent {
        let content = serde_json::json!({
            "final": "done",
            "messages": messages,
            "metrics": {
                "tool_uses": 0,
                "token_usage": TokenUsage::default(),
                "latest_activity": null,
                "status": status
            }
        })
        .to_string();
        MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: id.to_string(),
            tool_name: Arc::from("Agent"),
            is_error,
            content,
        })
    }

    #[test]
    fn snapshots_restore_recursive_agent_hierarchy_and_status() {
        let nested_messages = vec![
            Message::assistant(vec![agent_use("nested", "Monitor", "Watch tests")]),
            Message::user_with_content(vec![agent_result(
                "nested",
                vec![Message::assistant(vec![MessageContent::Text(
                    "tests finished".into(),
                )])],
                ToolCallStatus::Error,
                true,
            )]),
        ];
        let root_messages = vec![
            Message::assistant(vec![agent_use("outer", "Explore", "Inspect renderer")]),
            Message::user_with_content(vec![agent_result(
                "outer",
                nested_messages,
                ToolCallStatus::Success,
                false,
            )]),
        ];

        let snapshots = snapshots_from_messages(&root_messages);
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].info.id, "outer");
        assert_eq!(snapshots[0].info.parent_id, None);
        assert_eq!(snapshots[0].info.subagent_type, "Explore");
        assert_eq!(snapshots[0].info.description, "Inspect renderer");
        assert_eq!(snapshots[0].info.status, ToolCallStatus::Success);
        assert_eq!(snapshots[1].info.id, "nested");
        assert_eq!(snapshots[1].info.parent_id.as_deref(), Some("outer"));
        assert_eq!(snapshots[1].info.status, ToolCallStatus::Error);
        assert_eq!(snapshots[1].messages.len(), 1);
    }

    #[test]
    fn snapshots_keep_unfinished_agent_as_cancelled() {
        let snapshots = snapshots_from_messages(&[Message::assistant(vec![agent_use(
            "unfinished",
            "Explore",
            "Inspect live state",
        )])]);

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].info.status, ToolCallStatus::Cancelled);
        assert!(snapshots[0].messages.is_empty());
    }
}
