//! Read-only observation panel for a single pi sub-agent run.
//!
//! The workspace accumulates the child session's bridged events
//! ([`agent::SubagentChildEvent`]) per Agent tool-call id and pushes them into
//! the open panel; opening late backfills from that accumulation. Pi
//! sub-agent sessions are ephemeral (no persisted child transcript), so a
//! panel opened after a reload falls back to the Agent tool result's final
//! text plus a note.

use agent::SubagentChildEvent;
use agent::ToolCallStatus;
use agent::i18n;
use gpui::prelude::*;
use gpui::{Context, Pixels, Render, ScrollHandle, Window, px};
use gpui_component::{ActiveTheme as _, ElementExt as _, Theme, h_flex, v_flex};

use crate::views::subagents::status_indicator;

/// One rendered transcript line. Text/thinking deltas accumulate into the
/// trailing line of the same kind; tool lifecycle events are standalone.
#[derive(Clone, Debug)]
enum SubagentLine {
    Text(String),
    Thinking(String),
    ToolStart {
        name: String,
        summary: Option<String>,
    },
    ToolEnd {
        name: String,
        is_error: bool,
    },
}

/// Deltas append to the trailing line of the same kind so a streamed answer
/// renders as one block; lifecycle events always start a fresh line.
fn push_line(lines: &mut Vec<SubagentLine>, child: &SubagentChildEvent) {
    match child {
        SubagentChildEvent::Text(t) => {
            if let Some(SubagentLine::Text(buf)) = lines.last_mut() {
                buf.push_str(t);
            } else {
                lines.push(SubagentLine::Text(t.clone()));
            }
        }
        SubagentChildEvent::Thinking(t) => {
            if let Some(SubagentLine::Thinking(buf)) = lines.last_mut() {
                buf.push_str(t);
            } else {
                lines.push(SubagentLine::Thinking(t.clone()));
            }
        }
        SubagentChildEvent::ToolStart { name, summary } => lines.push(SubagentLine::ToolStart {
            name: name.clone(),
            summary: summary.clone(),
        }),
        SubagentChildEvent::ToolEnd { name, is_error } => lines.push(SubagentLine::ToolEnd {
            name: name.clone(),
            is_error: *is_error,
        }),
    }
}

/// Multi-line plain text as one div per source line (gpui has no pre-wrap);
/// blank lines keep their height via a nbsp.
fn text_block(text: &str, thinking: bool, theme: &Theme) -> gpui::Div {
    let mut col = gpui::div().w_full().min_w_0();
    for line in text.split('\n') {
        let styled = if thinking {
            gpui::div()
                .text_xs()
                .italic()
                .text_color(theme.muted_foreground)
        } else {
            gpui::div().text_sm().text_color(theme.foreground)
        };
        col = col.child(if line.is_empty() {
            styled.child("\u{00A0}")
        } else {
            styled.child(line.to_string())
        });
    }
    col
}

pub(crate) struct SubagentPanel {
    /// `{type} · {topic}` display title, also used for the tab label.
    title: String,
    status: ToolCallStatus,
    lines: Vec<SubagentLine>,
    /// Final-answer fallback for panels opened after a reload, when no live
    /// transcript was accumulated.
    final_text: Option<String>,
    scroll_handle: ScrollHandle,
    stick_to_bottom: bool,
}

impl SubagentPanel {
    pub(crate) fn new(
        title: String,
        status: ToolCallStatus,
        backfill: &[SubagentChildEvent],
        final_text: Option<String>,
    ) -> Self {
        let mut lines = Vec::with_capacity(backfill.len());
        for event in backfill {
            push_line(&mut lines, event);
        }
        Self {
            title,
            status,
            lines,
            final_text,
            scroll_handle: ScrollHandle::new(),
            stick_to_bottom: true,
        }
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn push(&mut self, child: &SubagentChildEvent, cx: &mut Context<Self>) {
        push_line(&mut self.lines, child);
        // New content arrives: resume tail-follow unless the user has
        // scrolled up to inspect history.
        self.stick_to_bottom = true;
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
                    .child(self.title.clone()),
            );

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
            .py_2()
            .gap_2();

        if self.lines.is_empty() {
            match &self.final_text {
                Some(final_text) => {
                    body = body
                        .child(
                            gpui::div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(i18n::t("subagent-panel-final-note")),
                        )
                        .child(text_block(final_text, false, theme));
                }
                None => {
                    body = body.child(
                        gpui::div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(i18n::t("subagent-panel-waiting")),
                    );
                }
            }
        } else {
            for line in &self.lines {
                body = body.child(match line {
                    SubagentLine::Text(text) => text_block(text, false, theme),
                    SubagentLine::Thinking(text) => text_block(text, true, theme),
                    SubagentLine::ToolStart { name, summary } => {
                        let label = match summary {
                            Some(summary) => format!("▸ {name} {summary}"),
                            None => format!("▸ {name}"),
                        };
                        gpui::div()
                            .text_xs()
                            .font_family(theme.mono_font_family.clone())
                            .text_color(theme.muted_foreground)
                            .child(label)
                    }
                    SubagentLine::ToolEnd { name, is_error } => gpui::div()
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(if *is_error {
                            theme.danger
                        } else {
                            theme.success
                        })
                        .child(format!("{} {name}", if *is_error { "✗" } else { "✓" })),
                });
            }
        }

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
                    // scroll back through history; re-arm by scrolling back
                    // to the bottom.
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
