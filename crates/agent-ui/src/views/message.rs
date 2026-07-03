//! Rendering of a single conversation message.
//!
//! - Text blocks use `TextView::markdown(...).selectable(true)` for selection + Cmd+C copy.
//! - Each block carries a copy button in its top-right corner that writes the whole block to the clipboard.
//! - User: a right-aligned rounded card within the block.
//! - Assistant: a full-width block with a role label + markdown body.
//! - Reasoning: a collapsible block, indented secondary text with a left border.
//! - ToolCall: a card with title + status icon + monospace output.
//!
//! Streaming assistant / reasoning bodies render as plain `div` (no markdown
//! re-parse on every token) and only switch to `TextView::markdown` once the
//! stream ends. The expensive markdown layout + text shaping on every delta
//! is what produced the visible item overlap and made scrolling feel sticky.

use gpui::prelude::*;
use gpui::{App, ClipboardItem, px};
use gpui_component::text::TextView;
use gpui_component::{Icon, IconName, Sizable as _, Theme, button::{Button, ButtonVariants as _}, h_flex, v_flex};

use crate::conversation::{ConvItem, ToolCallItem};

/// Render a `ConvItem` as an element. `ix` is the entry index (stable key for collapsibles/TextView).
pub fn render_item(item: &ConvItem, ix: usize, role: &str, theme: &Theme) -> gpui::AnyElement {
    match item {
        ConvItem::User(text) => render_user(text, ix, theme),
        ConvItem::Assistant { text, streaming } => {
            render_assistant(text, *streaming, ix, role, theme)
        }
        ConvItem::Reasoning { text, streaming } => render_reasoning(text, ix, *streaming, theme),
        ConvItem::ToolCall(t) => render_tool_call(t, ix, theme),
        ConvItem::Error(msg) => render_error(msg, ix, theme),
    }
}

/// Copy button: writes `text` to the clipboard on click.
fn copy_button(ix: usize, prefix: &'static str, text: String) -> Button {
    Button::new((prefix, ix))
        .ghost()
        .xsmall()
        .icon(IconName::Copy)
        .on_click(move |_, _, cx: &mut App| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        })
}

/// Render a user message: a right-aligned rounded card + copy button.
pub fn render_user(text: &str, ix: usize, theme: &Theme) -> gpui::AnyElement {
    h_flex()
        .w_full()
        .justify_end()
        .child(
            v_flex()
                .max_w(px(560.))
                .gap_1()
                .px_3()
                .py_2()
                .rounded(theme.radius)
                .bg(theme.secondary)
                .border_1()
                .border_color(theme.border)
                .shadow_sm()
                .child(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .child(copy_button(ix, "copy-user", text.to_string())),
                )
                .child(
                    gpui::div()
                        .text_sm()
                        .text_color(theme.secondary_foreground)
                        .child(TextView::markdown(("user-text", ix), text.to_string()).selectable(true)),
                ),
        )
        .into_any_element()
}

/// Render an assistant message: role label + copy button + markdown body. `role` is the model display name (dynamic).
pub fn render_assistant(text: &str, streaming: bool, ix: usize, role: &str, theme: &Theme) -> gpui::AnyElement {
    v_flex()
        .w_full()
        .gap_2()
        .child(
            h_flex()
                .gap_1p5()
                .items_center()
                .child(Icon::new(IconName::Bot).small().text_color(theme.green))
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(role.to_string()),
                )
                .child(gpui::div().flex_1())
                .child(copy_button(ix, "copy-assistant", text.to_string())),
        )
        .child(render_text_body(text, streaming, ("assistant", ix), theme))
        .into_any_element()
}

/// Render the assistant / reasoning body. While the stream is live we paint a
/// plain text run — markdown re-parse and shaped text layout on every token
/// delta was the source of the visible item overlap and the scroll-jank.
/// When the stream ends we mount `TextView::markdown` once for selection +
/// rendering of headings, lists, and code blocks.
fn render_text_body(
    text: &str,
    streaming: bool,
    id: impl Into<gpui::ElementId> + Clone,
    theme: &Theme,
) -> gpui::AnyElement {
    if streaming {
        let shown = format!("{text}▌");
        gpui::div()
            .id(id.clone())
            .text_sm()
            .whitespace_normal()
            .text_color(theme.foreground)
            .child(shown)
            .into_any_element()
    } else {
        gpui::div()
            .id(id.clone())
            .text_sm()
            .child(TextView::markdown(id, text.to_string()).selectable(true))
            .into_any_element()
    }
}

/// Render a reasoning (thinking) block: expanded while streaming, collapsed when done, with a copy button.
pub fn render_reasoning(text: &str, ix: usize, streaming: bool, theme: &Theme) -> gpui::AnyElement {
    let collapsed = !streaming;
    let chevron = if collapsed {
        IconName::ChevronRight
    } else {
        IconName::ChevronDown
    };
    let mut block = v_flex().w_full().gap_1().child(
        h_flex()
            .id(("reasoning-header", ix))
            .gap_1p5()
            .items_center()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(Icon::new(chevron).xsmall())
            .child("思考")
            .child(gpui::div().flex_1())
            .child(copy_button(ix, "copy-reasoning", text.to_string())),
    );
    if !collapsed {
        let body = render_text_body(text, streaming, ("reasoning", ix), theme);
        block = block.child(
            gpui::div()
                .pl_3()
                .border_l_1()
                .border_color(theme.border)
                .text_sm()
                .text_color(theme.muted_foreground)
                .child(body),
        );
    }
    block.into_any_element()
}

/// Render an error message + copy button.
pub fn render_error(msg: &str, ix: usize, theme: &Theme) -> gpui::AnyElement {
    v_flex()
        .w_full()
        .gap_1()
        .px_3()
        .py_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.danger)
        .bg(theme.danger.opacity(0.08))
        .child(
            h_flex()
                .w_full()
                .justify_between()
                .items_center()
                .child(
                    gpui::div()
                        .text_sm()
                        .text_color(theme.danger)
                        .child("错误"),
                )
                .child(copy_button(ix, "copy-error", msg.to_string())),
        )
        .child(
            gpui::div()
                .text_sm()
                .text_color(theme.danger)
                .child(TextView::markdown(("error", ix), msg.to_string()).selectable(true)),
        )
        .into_any_element()
}

/// Render a tool-call card: title + status icon + copy button + monospace output.
pub fn render_tool_call(item: &ToolCallItem, ix: usize, theme: &Theme) -> gpui::AnyElement {
    use agent::ToolCallStatus;
    let (status_icon, status_color, status_label) = match item.status {
        ToolCallStatus::PendingApproval => {
            (IconName::LoaderCircle, theme.muted_foreground, "待审批")
        }
        ToolCallStatus::Running => (IconName::LoaderCircle, theme.muted_foreground, "运行中"),
        ToolCallStatus::Success => (IconName::CircleCheck, theme.success, "完成"),
        ToolCallStatus::Error => (IconName::CircleX, theme.danger, "出错"),
        ToolCallStatus::Denied => (IconName::CircleX, theme.danger, "已拒绝"),
    };

    let title = if item.title.is_empty() {
        item.name.clone()
    } else {
        item.title.clone()
    };

    let mut card = v_flex()
        .w_full()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .overflow_hidden()
        .child(
            h_flex()
                .w_full()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .child(
                    Icon::new(IconName::SquareTerminal)
                        .small()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    gpui::div()
                        .flex_1()
                        .text_xs()
                        .font_family("monospace")
                        .text_color(theme.foreground)
                        .child(truncate(&title, 80)),
                )
                .child(copy_button(ix, "copy-tool", item.output.clone()))
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .text_xs()
                        .text_color(status_color)
                        .child(Icon::new(status_icon).xsmall())
                        .child(status_label),
                ),
        );

    // While streaming, show the live tail so newly-emitted lines stay visible
    // without a scroll handle (the full, truncated output replaces it once the
    // final `ToolResult` lands and `streaming` flips false).
    let display_output = if item.streaming {
        live_tail(&item.output)
    } else {
        item.output.clone()
    };

    if !display_output.is_empty() {
        // Wrap as a markdown code block: monospace, uninterpreted, selectable.
        let code = format!("```\n{}\n```", display_output);
        card = card.child(
            gpui::div()
                .id(("tool-output", ix))
                .max_h(px(220.))
                .overflow_y_scroll()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(theme.border)
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(TextView::markdown(("tool-output-text", ix), code).selectable(true)),
        );
    }
    card.into_any_element()
}

/// Trailing slice of live output: keep the last ~12 KiB so the most recent
/// lines are in view as they stream in. Whole-buffer lines are preserved once
/// the final result arrives.
fn live_tail(output: &str) -> String {
    const TAIL_BYTES: usize = 12 * 1024;
    if output.len() <= TAIL_BYTES {
        return output.to_string();
    }
    let cut = output.len() - TAIL_BYTES;
    // Start at the next line boundary so we don't slice mid-line.
    let start = output[cut..].find('\n').map(|i| cut + i + 1).unwrap_or(cut);
    let mut s = String::from("…（已省略前面部分）\n");
    s.push_str(&output[start..]);
    s
}

fn truncate(s: &str, max_chars: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() > max_chars {
        let t: String = one_line.chars().take(max_chars).collect();
        format!("{t}…")
    } else {
        one_line
    }
}
