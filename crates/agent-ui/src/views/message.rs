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

use std::collections::HashSet;

use gpui::prelude::*;
use gpui::{App, ClipboardItem, WeakEntity, px};
use gpui_component::text::{TextView, TextViewStyle};
use gpui_component::{
    Icon, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::Workspace;
use crate::conversation::{AgentTaskItem, ConvItem, ToolCallItem};

/// Render-time context for sub-agent task cards: which task ids are currently
/// expanded, and a weak handle to toggle expansion on the owning `Workspace`.
#[derive(Clone)]
pub struct AgentTaskCtx {
    pub expanded: HashSet<String>,
    pub weak: WeakEntity<Workspace>,
}

/// Build a `TextViewStyle` that matches the current theme's highlight palette.
fn text_view_style(theme: &Theme) -> TextViewStyle {
    TextViewStyle {
        highlight_theme: theme.highlight_theme.clone(),
        is_dark: theme.is_dark(),
        ..TextViewStyle::default()
    }
}

/// Markdown `TextView` with theme-aware syntax highlighting.
fn markdown_tv(
    id: impl Into<gpui::ElementId>,
    text: impl Into<gpui::SharedString>,
    theme: &Theme,
) -> TextView {
    TextView::markdown(id, text)
        .selectable(true)
        .style(text_view_style(theme))
}

/// Render a `ConvItem` as an element. `ix` is the entry index (stable key for collapsibles/TextView).
/// `agent_ctx` supplies expansion state for `AgentTask` cards.
pub fn render_item(
    item: &ConvItem,
    ix: usize,
    role: &str,
    theme: &Theme,
    agent_ctx: &AgentTaskCtx,
) -> gpui::AnyElement {
    match item {
        ConvItem::User(text) => render_user(text, ix, theme),
        ConvItem::Assistant { text, streaming } => {
            render_assistant(text, *streaming, ix, role, theme)
        }
        ConvItem::Reasoning { text, streaming } => render_reasoning(text, ix, *streaming, theme),
        ConvItem::ToolCall(t) => render_tool_call(t, ix, theme),
        ConvItem::AgentTask(t) => render_agent_task(t, ix, theme, agent_ctx),
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
                .child(h_flex().w_full().justify_end().child(copy_button(
                    ix,
                    "copy-user",
                    text.to_string(),
                )))
                .child(
                    gpui::div()
                        .text_sm()
                        .text_color(theme.secondary_foreground)
                        .child(markdown_tv(("user-text", ix), text.to_string(), theme)),
                ),
        )
        .into_any_element()
}

/// Render an assistant message: role label + copy button + markdown body. `role` is the model display name (dynamic).
pub fn render_assistant(
    text: &str,
    streaming: bool,
    ix: usize,
    role: &str,
    theme: &Theme,
) -> gpui::AnyElement {
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
/// rendering of headings, lists, and code blocks with syntax highlighting.
fn render_text_body(
    text: &str,
    streaming: bool,
    id: impl Into<gpui::ElementId> + Clone,
    theme: &Theme,
) -> gpui::AnyElement {
    if streaming {
        // Plain text div while streaming — no Tree-sitter involved, so cursor
        // can stay inline without corrupting any parser.
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
            .child(markdown_tv(id, text.to_string(), theme))
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
                .child(gpui::div().text_sm().text_color(theme.danger).child("错误"))
                .child(copy_button(ix, "copy-error", msg.to_string())),
        )
        .child(
            gpui::div()
                .text_sm()
                .text_color(theme.danger)
                .child(markdown_tv(("error", ix), msg.to_string(), theme)),
        )
        .into_any_element()
}

/// Heuristic: map a tool name to a markdown code-block language tag so that
/// syntax highlighting can colour the output.
fn lang_hint_for_tool(name: &str) -> Option<&'static str> {
    match name {
        "bash" => Some("bash"),
        "python" => Some("python"),
        _ => None,
    }
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
                        .font_family(theme.mono_font_family.clone())
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
        // Language-annotated code block for syntax highlighting, plain block otherwise.
        let lang = lang_hint_for_tool(&item.name);
        let code = if let Some(l) = lang {
            format!("```{l}\n{display_output}\n```")
        } else {
            format!("```\n{display_output}\n```")
        };
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
                .child(markdown_tv(("tool-output-text", ix), code, theme)),
        );
    }
    card.into_any_element()
}

/// Render a sub-agent task card: title + status icon + chevron to expand the
/// child conversation. Collapsed shows the live streamed tail (or the final
/// result once the sub-agent stops); expanded rebuilds the child `Thread`'s
/// messages into a nested conversation and renders each item recursively.
pub fn render_agent_task(
    item: &AgentTaskItem,
    ix: usize,
    theme: &Theme,
    agent_ctx: &AgentTaskCtx,
) -> gpui::AnyElement {
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

    let expanded = agent_ctx.expanded.contains(&item.id);
    let chevron = if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    };
    let id_for_toggle = item.id.clone();
    let weak = agent_ctx.weak.clone();
    let copy_text = if item.final_text.is_empty() {
        item.sub_text.clone()
    } else {
        item.final_text.clone()
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
                .id(("agent-header", ix))
                .w_full()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .on_click(move |_, _window, cx: &mut App| {
                    let _ = weak.update(cx, |w, cx| {
                        // Toggle membership in `expanded_tasks`; notify so the
                        // chevron/body re-render immediately.
                        if !w.expanded_tasks.insert(id_for_toggle.clone()) {
                            w.expanded_tasks.remove(&id_for_toggle);
                        }
                        cx.notify();
                    });
                })
                .child(
                    Icon::new(IconName::Bot)
                        .small()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    Icon::new(chevron)
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    gpui::div()
                        .flex_1()
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.foreground)
                        .child(truncate(&item.title, 80)),
                )
                .child(copy_button(ix, "copy-agent", copy_text))
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

    // Collapsed body: the live tail while streaming, the final result once done,
    // or whatever was streamed if the sub-agent produced no final text.
    let collapsed_body = if item.streaming {
        live_tail(&item.sub_text)
    } else if !item.final_text.is_empty() {
        item.final_text.clone()
    } else {
        item.sub_text.clone()
    };

    if expanded {
        // Expanded: rebuild the child conversation from its snapshot and render
        // each item recursively. Nested agent tasks share the same expansion
        // set (keyed by id), so they expand/collapse in place too.
        let sub = crate::conversation::ConversationState::rebuild_from_messages(&item.sub_messages);
        let sub_items: Vec<_> = sub
            .items()
            .iter()
            .enumerate()
            .map(|(six, sitem)| render_item(sitem, six, "agent", theme, agent_ctx))
            .collect();
        if sub_items.is_empty() {
            if !collapsed_body.is_empty() {
                card = card.child(render_agent_body(&collapsed_body, ix, theme));
            }
        } else {
            card = card.child(
                v_flex()
                    .border_t_1()
                    .border_color(theme.border)
                    .px_3()
                    .py_2()
                    .gap_1()
                    .children(sub_items),
            );
        }
    } else if !collapsed_body.is_empty() {
        card = card.child(render_agent_body(&collapsed_body, ix, theme));
    }
    card.into_any_element()
}

/// Monospace, scrollable body for a sub-agent card (collapsed tail or fallback
/// when the snapshot is empty).
fn render_agent_body(text: &str, ix: usize, theme: &Theme) -> gpui::AnyElement {
    if text.is_empty() {
        return gpui::div().into_any_element();
    }
    let code = format!("```\n{text}\n```");
    gpui::div()
        .id(("agent-body", ix))
        .max_h(px(220.))
        .overflow_y_scroll()
        .px_3()
        .py_2()
        .border_t_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.muted_foreground)
        .child(markdown_tv(("agent-body-text", ix), code, theme))
        .into_any_element()
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
