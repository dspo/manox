//! Read-only observation panel for a single pi sub-agent run.
//!
//! The panel renders through the same [`ConversationState`] + message
//! pipeline as the main conversation: bridged child events are translated
//! into the shared [`ThreadEvent`] contract (`AgentText` / `AgentThinking` /
//! `ToolCall` / `ToolResult`), so a sub-agent run reads exactly like a
//! miniature conversation (assistant bubbles, reasoning folds, tool cards).
//!
//! The workspace accumulates the child session's bridged events
//! ([`manox_agent::SubagentChildEvent`]) per Agent tool-call id; opening mid-run
//! backfills from that accumulation by replaying the translated events.
//! Pi sub-agent sessions are ephemeral (no persisted child transcript), so a
//! panel opened after a reload falls back to the Agent tool result's final
//! text plus a note.

use std::collections::HashMap;

use crate::i18n;
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, Pixels, Render, ScrollHandle, WeakEntity, Window, px,
};
use gpui_component::{ActiveTheme as _, ElementExt as _, h_flex, v_flex};
use manox_agent::Message;
use manox_agent::ToolCallStatus;
use manox_agent::language_model::{MessageContent, TokenUsage};
use manox_agent::thread::{SubagentChildEvent, ThreadEvent};

use crate::Workspace;
use crate::conversation::{ApplyCtx, ConversationState};
use crate::views::subagents::status_indicator;

/// Translate one bridged child event into the shared [`ThreadEvent`]
/// contract. Tool titles go through the same `tool_title` derivation as the
/// main conversation, and the child call id pairs start/end under parallel
/// child tool execution. A message stop carries the child message's token
/// usage, consumed by the `Stop` arm to label the just-finished reply.
/// `None` marks an observation the transcript never renders — the child's
/// resolved model — which the panel consumes as turn-header metadata.
fn thread_event_of(child: &SubagentChildEvent) -> Option<(ThreadEvent, Option<TokenUsage>)> {
    match child {
        SubagentChildEvent::Text(text) => Some((ThreadEvent::AgentText(text.clone()), None)),
        SubagentChildEvent::Thinking(text) => {
            Some((ThreadEvent::AgentThinking(text.clone()), None))
        }
        SubagentChildEvent::ToolStart { id, name, hint } => {
            let input = hint
                .as_ref()
                .map(|(key, value)| serde_json::json!({ key: value }));
            let title_value = input.clone().unwrap_or_default();
            Some((
                ThreadEvent::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    title: manox_agent::thread::tool_title(name, &title_value, None),
                    status: ToolCallStatus::Running,
                    input,
                },
                None,
            ))
        }
        SubagentChildEvent::ToolEnd {
            id,
            is_error,
            output,
            ..
        } => Some((
            ThreadEvent::ToolResult {
                id: id.clone(),
                output: output.clone(),
                is_error: *is_error,
            },
            None,
        )),
        SubagentChildEvent::Stop { reason, usage } => Some((ThreadEvent::Stop(*reason), *usage)),
        SubagentChildEvent::Error(text) => {
            Some((ThreadEvent::Error(anyhow::anyhow!("{text}")), None))
        }
        SubagentChildEvent::Model(_) => None,
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
    /// The model the child runs: every turn header's `{model}` segment and the
    /// activity rows' model name. The recipient (the sub-agent definition)
    /// lives with the conversation, which owns the turn headers.
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
        recipient: String,
        status: ToolCallStatus,
        backfill: &[SubagentChildEvent],
        prompt: Option<(String, i64)>,
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
        // Seed the Captain's dispatch prompt as the opening turn, stamped with
        // its author and its send time, so the header reads
        // `Captain > {recipient}·{model}·{time}`. The final-answer fallback
        // (when nothing was bridged) and the bridged child events follow
        // through the same display/apply pipeline as the main thread.
        let mut display = Vec::new();
        if let Some((prompt, dispatched_at)) = prompt {
            let mut message = Message::user(prompt);
            message.timestamp = dispatched_at;
            message.ui = Some(manox_agent::MessageUiMetadata {
                author: Some(manox_agent::MessageAuthor::Lead),
                ..Default::default()
            });
            display.push(manox_agent::db::HistoryEntry::Message(message));
        }
        if backfill.is_empty()
            && let Some(final_text) = final_text
        {
            display.push(manox_agent::db::HistoryEntry::Message(Message::assistant(
                vec![MessageContent::Text(final_text)],
            )));
        }
        let empty_usage: HashMap<String, TokenUsage> = HashMap::new();
        let recipient_author = manox_agent::MessageAuthor::Agent(recipient.clone());
        let conversation = cx.new(|cx| {
            let mut conversation = ConversationState::rebuild_from_display(
                &display,
                &empty_usage,
                &role_for_conv,
                recipient_author,
                false,
                ctx.clone(),
                cx,
            );
            for child in backfill {
                if let Some((event, usage)) = thread_event_of(child) {
                    conversation.apply(&event, &role_for_conv, usage, ctx.clone(), cx);
                }
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
        if let SubagentChildEvent::Model(model) = child {
            // The child's resolved model arrives once, at dispatch: later turn
            // headers name the model that actually runs the work.
            self.role = model.clone();
            cx.notify();
            return;
        }
        let Some((event, usage)) = thread_event_of(child) else {
            return;
        };
        let role = self.role.clone();
        let ctx = ApplyCtx {
            weak: self.weak_workspace.clone(),
            cwd: None,
        };
        self.conversation
            .update(cx, |c, cx| c.apply(&event, &role, usage, ctx, cx));
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

    /// Test view of the transcript mapping: every event probed here renders a
    /// row, so the header-only observations never come through.
    fn translated(child: &SubagentChildEvent) -> (ThreadEvent, Option<TokenUsage>) {
        thread_event_of(child).expect("event maps onto a transcript event")
    }

    #[test]
    fn child_events_translate_to_shared_thread_events() {
        let (event, usage) = translated(&SubagentChildEvent::ToolStart {
            id: "c1".into(),
            name: "Read".into(),
            hint: Some(("path".into(), "src/x.rs".into())),
        });
        assert!(usage.is_none());
        match event {
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

        let (event, usage) = translated(&SubagentChildEvent::ToolEnd {
            id: "c1".into(),
            name: "Read".into(),
            is_error: true,
            output: "ok".into(),
        });
        assert!(usage.is_none());
        match event {
            ThreadEvent::ToolResult {
                id,
                is_error,
                output,
                ..
            } => {
                assert_eq!(id, "c1");
                assert!(is_error);
                assert_eq!(output, "ok");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        }

        assert!(matches!(
            translated(&SubagentChildEvent::Text("hi".into())),
            (ThreadEvent::AgentText(t), None) if t == "hi"
        ));
        assert!(matches!(
            translated(&SubagentChildEvent::Thinking("hmm".into())),
            (ThreadEvent::AgentThinking(t), None) if t == "hmm"
        ));
        assert!(
            thread_event_of(&SubagentChildEvent::Model("glm-5.2".into())).is_none(),
            "the child's resolved model is header metadata, never a transcript row"
        );
    }

    /// A message stop maps onto the shared `Stop` boundary with the child
    /// message's usage; a terminal provider error maps onto `Error`.
    #[test]
    fn stop_and_error_events_translate_to_thread_boundaries() {
        let (event, usage) = translated(&SubagentChildEvent::Stop {
            reason: manox_agent::language_model::StopReason::ToolUse,
            usage: Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
        });
        assert!(matches!(
            event,
            ThreadEvent::Stop(manox_agent::language_model::StopReason::ToolUse)
        ));
        assert_eq!(usage.unwrap().input_tokens, 10);

        let (event, usage) = translated(&SubagentChildEvent::Stop {
            reason: manox_agent::language_model::StopReason::EndTurn,
            usage: None,
        });
        assert!(matches!(
            event,
            ThreadEvent::Stop(manox_agent::language_model::StopReason::EndTurn)
        ));
        assert!(usage.is_none());

        let (event, usage) = translated(&SubagentChildEvent::Error("boom".into()));
        assert!(usage.is_none());
        assert!(matches!(event, ThreadEvent::Error(e) if e.to_string() == "boom"));
    }
}
