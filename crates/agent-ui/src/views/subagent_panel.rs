//! Read-only observation panel for a single pi sub-agent run.
//!
//! The panel renders through the same [`ConversationState`] + message
//! pipeline as the main conversation: bridged child events are translated
//! into the shared [`ThreadEvent`] contract (`AgentText` / `AgentThinking` /
//! `ToolCall` / `ToolResult`), so a sub-agent run reads exactly like a
//! miniature conversation (assistant bubbles, reasoning folds, tool cards).
//!
//! The workspace accumulates the child session's bridged events
//! ([`agent::SubagentChildEvent`]) per Agent tool-call id; opening mid-run
//! backfills from that accumulation by replaying the translated events.
//! Pi sub-agent sessions are ephemeral (no persisted child transcript), so a
//! panel opened after a reload falls back to the Agent tool result's final
//! text plus a note.

use std::collections::HashMap;

use agent::Message;
use agent::ToolCallStatus;
use agent::i18n;
use agent::language_model::{MessageContent, TokenUsage};
use agent::thread::{SubagentChildEvent, ThreadEvent};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, Pixels, Render, ScrollHandle, WeakEntity, Window, px,
};
use gpui_component::{ActiveTheme as _, ElementExt as _, h_flex, v_flex};

use crate::Workspace;
use crate::conversation::{ApplyCtx, ConversationState};
use crate::views::subagents::status_indicator;

/// Translate one bridged child event into the shared [`ThreadEvent`]
/// contract. Tool titles go through the same `tool_title` derivation as the
/// main conversation, and the child call id pairs start/end under parallel
/// child tool execution.
fn thread_event_of(child: &SubagentChildEvent) -> ThreadEvent {
    match child {
        SubagentChildEvent::Text(text) => ThreadEvent::AgentText(text.clone()),
        SubagentChildEvent::Thinking(text) => ThreadEvent::AgentThinking(text.clone()),
        SubagentChildEvent::ToolStart { id, name, hint } => {
            let input = hint
                .as_ref()
                .map(|(key, value)| serde_json::json!({ key: value }));
            let title_value = input.clone().unwrap_or_default();
            ThreadEvent::ToolCall {
                id: id.clone(),
                name: name.clone(),
                title: agent::thread::tool_title(name, &title_value, None),
                status: ToolCallStatus::Running,
                input,
            }
        }
        SubagentChildEvent::ToolEnd { id, is_error, .. } => ThreadEvent::ToolResult {
            id: id.clone(),
            output: String::new(),
            is_error: *is_error,
        },
    }
}

pub(crate) struct SubagentPanel {
    /// The sub-agent's task topic — the panel's second-level banner. The
    /// right-pane tab label shows the subagent's address instead.
    topic: String,
    status: ToolCallStatus,
    /// The child run rendered as a miniature conversation through the shared
    /// message pipeline.
    conversation: Entity<ConversationState>,
    /// Rendered above the transcript when the panel fell back to the final
    /// answer (no live transcript survives a reload).
    final_note: bool,
    role: String,
    weak_workspace: WeakEntity<Workspace>,
    scroll_handle: ScrollHandle,
    stick_to_bottom: bool,
}

impl SubagentPanel {
    #[allow(clippy::too_many_arguments)] // constructor inputs are distinct presentation values, not a bundleable config
    pub(crate) fn new(
        topic: String,
        role: String,
        status: ToolCallStatus,
        backfill: &[SubagentChildEvent],
        prompt: Option<String>,
        final_text: Option<String>,
        weak_workspace: WeakEntity<Workspace>,
        cx: &mut App,
    ) -> Entity<Self> {
        let ctx = ApplyCtx {
            weak: weak_workspace.clone(),
            cwd: None,
        };
        let final_note = backfill.is_empty() && final_text.is_some();
        let role_for_conv = role.clone();
        // Seed the Captain's dispatch prompt as the opening user bubble so the
        // panel always shows the conversation's first message; the final-answer
        // fallback (when nothing was bridged) and the bridged child events
        // follow through the same display/apply pipeline as the main thread.
        let mut display = Vec::new();
        if let Some(prompt) = prompt {
            display.push(agent::db::HistoryEntry::Message(Message::user(prompt)));
        }
        if backfill.is_empty()
            && let Some(final_text) = final_text
        {
            display.push(agent::db::HistoryEntry::Message(Message::assistant(vec![
                MessageContent::Text(final_text),
            ])));
        }
        let empty_usage: HashMap<String, TokenUsage> = HashMap::new();
        let conversation = cx.new(|cx| {
            let mut conversation = ConversationState::rebuild_from_display(
                &display,
                &empty_usage,
                &role_for_conv,
                false,
                ctx.clone(),
                cx,
            );
            for child in backfill {
                conversation.apply(
                    &thread_event_of(child),
                    &role_for_conv,
                    None,
                    ctx.clone(),
                    cx,
                );
            }
            conversation
        });
        cx.new(|_| Self {
            topic,
            status,
            conversation,
            final_note,
            role,
            weak_workspace,
            scroll_handle: ScrollHandle::new(),
            stick_to_bottom: true,
        })
    }

    pub(crate) fn push(&mut self, child: &SubagentChildEvent, cx: &mut Context<Self>) {
        let event = thread_event_of(child);
        let role = self.role.clone();
        let ctx = ApplyCtx {
            weak: self.weak_workspace.clone(),
            cwd: None,
        };
        self.conversation
            .update(cx, |c, cx| c.apply(&event, &role, None, ctx, cx));
        // Re-arm tail-follow only while the viewport is still near the
        // bottom; a user who scrolled up to read history must not be yanked
        // back by the next delta.
        let off = self.scroll_handle.offset().y;
        let max = self.scroll_handle.max_offset().y;
        if (max + off).abs() < px(20.) {
            self.stick_to_bottom = true;
        }
        cx.notify();
    }

    pub(crate) fn set_status(&mut self, status: ToolCallStatus, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }
}

impl Render for SubagentPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();

        let header = h_flex()
            .w_full()
            .flex_shrink_0()
            .items_center()
            .gap_1p5()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .child(status_indicator(self.status, theme))
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.foreground)
                    .child(self.topic.clone()),
            );

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

        let mut body = v_flex()
            .id("subagent-transcript-scroll")
            .w_full()
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .overflow_x_hidden()
            .track_scroll(&scroll)
            .px_3()
            .py_2();

        if self.final_note {
            body = body.child(
                gpui::div()
                    .pb_2()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(i18n::t("subagent-panel-final-note")),
            );
        }
        if items.is_empty() && !self.final_note {
            body = body.child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(i18n::t("subagent-panel-waiting")),
            );
        }
        body = body.children(items);

        let body = body
            .on_prepaint(move |_bounds, _window, cx| {
                let Some(this) = weak.upgrade() else {
                    return;
                };
                if !this.read(cx).stick_to_bottom {
                    return;
                }
                let off = scroll.offset().y;
                let max = scroll.max_offset().y;
                if (max + off).abs() < px(1.) {
                    return;
                }
                scroll.scroll_to_bottom();
            })
            .on_scroll_wheel(
                cx.listener(|this, ev: &gpui::ScrollWheelEvent, window, cx| {
                    // An upward wheel breaks tail-follow so the user can
                    // scroll back through history; the next push re-arms it
                    // once the viewport is back near the bottom.
                    let dy = ev.delta.pixel_delta(window.line_height()).y;
                    if dy > Pixels::ZERO {
                        this.stick_to_bottom = false;
                        cx.notify();
                    }
                }),
            );

        v_flex()
            .h_full()
            .w_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(header)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_events_translate_to_shared_thread_events() {
        match thread_event_of(&SubagentChildEvent::ToolStart {
            id: "c1".into(),
            name: "Read".into(),
            hint: Some(("path".into(), "src/x.rs".into())),
        }) {
            ThreadEvent::ToolCall {
                id,
                name,
                title,
                status,
                input,
            } => {
                assert_eq!(id, "c1");
                assert_eq!(name, "Read");
                assert_eq!(status, ToolCallStatus::Running);
                assert!(title.contains("src/x.rs"), "{title}");
                assert_eq!(input.unwrap()["path"], "src/x.rs");
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }

        match thread_event_of(&SubagentChildEvent::ToolEnd {
            id: "c1".into(),
            name: "Read".into(),
            is_error: true,
        }) {
            ThreadEvent::ToolResult { id, is_error, .. } => {
                assert_eq!(id, "c1");
                assert!(is_error);
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }

        assert!(matches!(
            thread_event_of(&SubagentChildEvent::Text("hi".into())),
            ThreadEvent::AgentText(t) if t == "hi"
        ));
        assert!(matches!(
            thread_event_of(&SubagentChildEvent::Thinking("hmm".into())),
            ThreadEvent::AgentThinking(t) if t == "hmm"
        ));
    }
}
