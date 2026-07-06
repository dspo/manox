//! Rendering of a single conversation message.
//!
//! - Text blocks use `TextView::markdown(...).selectable(true)` for selection + Cmd+C copy.
//! - Each block carries a copy button in its top-right corner that writes the whole block to the clipboard.
//! - User: right-aligned bubble with a Codex-style turn rail marker.
//! - Assistant: document-flow markdown body.
//! - Reasoning: a collapsible block, indented secondary text with a left border.
//! - ToolCall: a card with title + status icon + monospace output.
//!
//! Streaming assistant / reasoning bodies render as plain `div` (no markdown
//! re-parse on every token) and only switch to `TextView::markdown` once the
//! stream ends. The same rule applies to streaming tool output: while lines
//! are still arriving we paint a plain monospace run and only mount the
//! syntax-highlighted `TextView::markdown` once the final `ToolResult` lands.

use std::collections::{HashMap, HashSet};

use agent::language_model::{LanguageModelToolResult, MessageContent, Role};
use agent::tools::agent::{agent_final_text, agent_sub_messages};
use agent::{Message, TokenUsage, ToolCallStatus, i18n};
use gpui::prelude::*;
use gpui::{App, ClipboardItem, Render, SharedString, WeakEntity, px};
use gpui_component::text::{TextView, TextViewStyle};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::Workspace;
use crate::conversation::{AgentTaskItem, ConvItem, ToolCallItem};
use crate::views::centered;

/// Render-time context for sub-agent task cards: which task ids are currently
/// expanded, and a weak handle to toggle expansion on the owning `Workspace`.
/// `None` when the owning `Workspace` is already dropped (renders collapsed,
/// clicks no-op).
#[derive(Clone)]
pub struct AgentTaskCtx {
    pub expanded: HashSet<String>,
    pub weak: WeakEntity<Workspace>,
}

/// Render-time context for plain tool-call cards. Carries a weak handle to the
/// owning `Workspace` so the card's header can toggle its own `collapsed` flag
/// (the flag lives on the `ToolCallItem` so the user's choice survives scroll-
/// driven remounts). `None` after the Workspace drops — clicks no-op and the
/// card stays in whatever state it last rendered.
#[derive(Clone)]
pub struct ToolCallCtx {
    pub weak: WeakEntity<Workspace>,
}

/// Build a `TextViewStyle` that matches the current theme's highlight palette.
fn text_view_style(theme: &Theme) -> TextViewStyle {
    TextViewStyle {
        paragraph_gap: gpui::rems(0.55),
        heading_base_font_size: px(14.),
        heading_font_size: Some(std::sync::Arc::new(|_, base| base)),
        highlight_theme: theme.highlight_theme.clone(),
        is_dark: theme.is_dark(),
        ..TextViewStyle::default()
    }
}

/// Markdown `TextView` with theme-aware syntax highlighting.
///
/// `scrollable = true` mounts an internal vertical scrollbar. In that mode the
/// TextView sizes to its parent's box, so the parent must have a defined
/// height (use `h(...)` rather than `max_h(...)`).
fn markdown_tv(
    id: impl Into<gpui::ElementId>,
    text: impl Into<gpui::SharedString>,
    theme: &Theme,
    scrollable: bool,
) -> TextView {
    let tv = TextView::markdown(id, text)
        .selectable(true)
        .style(text_view_style(theme));
    if scrollable { tv.scrollable(true) } else { tv }
}

fn chat_markdown_tv(
    id: impl Into<gpui::ElementId>,
    text: impl AsRef<str>,
    theme: &Theme,
    scrollable: bool,
) -> TextView {
    markdown_tv(
        id,
        normalize_chat_markdown(text.as_ref()),
        theme,
        scrollable,
    )
}

fn normalize_chat_markdown(text: &str) -> String {
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    let mut in_fence = false;

    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            out.push(line.to_string());
            continue;
        }

        if !in_fence {
            if let Some((level, title)) = atx_heading_title(trimmed) {
                out.extend(render_normalized_heading(level, title));
                continue;
            }
            if let Some(next) = lines.peek()
                && let Some(level) = setext_heading_level(next)
                && !line.trim().is_empty()
            {
                out.extend(render_normalized_heading(level, line.trim()));
                lines.next();
                continue;
            }
        }

        out.push(line.to_string());
    }

    out.join("\n")
}

fn atx_heading_title(line: &str) -> Option<(u8, &str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = line.get(hashes..)?.trim_start();
    if rest.is_empty() || rest == line.get(hashes..)? {
        return None;
    }
    Some((hashes as u8, rest.trim_end_matches('#').trim()))
}

fn setext_heading_level(line: &str) -> Option<u8> {
    let trimmed = line.trim();
    if trimmed.len() < 3 {
        return None;
    }
    if trimmed.chars().all(|c| c == '=') {
        Some(1)
    } else if trimmed.chars().all(|c| c == '-') {
        Some(2)
    } else {
        None
    }
}

fn render_normalized_heading(level: u8, title: &str) -> Vec<String> {
    if level == 1 {
        vec![format!("***{title}***"), heading_underline(title)]
    } else {
        vec![format!("**{title}**")]
    }
}

fn heading_underline(title: &str) -> String {
    let width = title.chars().count().clamp(6, 32);
    "─".repeat(width)
}

/// One renderable conversation item, owned by its own gpui `Entity` so a
/// streaming delta notifies (and re-renders) only this item rather than the
/// whole workspace. `id` is the item's stable list index (the conversation
/// only ever appends, so the index never shifts); it keys element ids within
/// the entity's own namespace. `role` is the model display name captured at
/// creation time so a finished bubble keeps its model label after the user
/// switches models.
pub struct MessageItem {
    kind: ConvItem,
    role: String,
    id: usize,
    /// Weak handle to the owning `Workspace`, used to read/toggle the shared
    /// `expanded_tasks` set from `AgentTask` cards.
    weak_workspace: WeakEntity<Workspace>,
}

impl MessageItem {
    pub fn new(kind: ConvItem, role: String, id: usize, weak: WeakEntity<Workspace>) -> Self {
        Self {
            kind,
            role,
            id,
            weak_workspace: weak,
        }
    }

    pub fn kind(&self) -> &ConvItem {
        &self.kind
    }

    pub fn kind_mut(&mut self) -> &mut ConvItem {
        &mut self.kind
    }

    /// Flip every streaming flag off (terminal `Stop`). Called once per turn,
    /// so the O(items) walk is harmless. Also drives the tool-call auto-collapse:
    /// a tool card that hasn't been touched by the user gets folded into a
    /// single-line card so it stops competing with the assistant's final reply.
    pub fn finalize_streaming(&mut self) {
        match &mut self.kind {
            ConvItem::Assistant { streaming, .. } => *streaming = false,
            ConvItem::Reasoning {
                streaming,
                collapsed,
                user_toggled,
                ..
            } => {
                *streaming = false;
                *collapsed = !*user_toggled;
            }
            ConvItem::ToolCall(t) => {
                t.streaming = false;
                if matches!(
                    t.status,
                    ToolCallStatus::Success | ToolCallStatus::Error | ToolCallStatus::Denied
                ) {
                    t.collapsed = !t.user_toggled;
                }
            }
            ConvItem::AgentTask(t) => t.streaming = false,
            _ => {}
        }
    }
}

impl Render for MessageItem {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let agent_ctx = self.weak_workspace.upgrade().map(|ws| {
            let expanded = ws.read(cx).expanded_tasks.clone();
            AgentTaskCtx {
                expanded,
                weak: ws.downgrade(),
            }
        });
        let tool_ctx = self.weak_workspace.upgrade().map(|ws| ToolCallCtx {
            weak: ws.downgrade(),
        });
        centered(render_item(
            &self.kind,
            self.id,
            &self.role,
            &theme,
            agent_ctx.as_ref(),
            tool_ctx.as_ref(),
        ))
    }
}

/// Render a `ConvItem` as an element. `ix` is the entry index (stable key for collapsibles/TextView).
/// `agent_ctx` supplies expansion state for `AgentTask` cards; `tool_ctx` carries
/// the workspace weak handle for `ToolCall` cards to flip their own collapse flag.
/// `None` renders them in a static state with no-op clicks (used when the owning
/// Workspace is gone).
pub fn render_item(
    item: &ConvItem,
    ix: usize,
    role: &str,
    theme: &Theme,
    agent_ctx: Option<&AgentTaskCtx>,
    tool_ctx: Option<&ToolCallCtx>,
) -> gpui::AnyElement {
    match item {
        ConvItem::User(text) => render_user(text, ix, theme),
        ConvItem::Assistant {
            text,
            streaming,
            token_usage,
        } => render_assistant(text, *streaming, token_usage.as_ref(), ix, role, theme),
        ConvItem::Reasoning {
            text,
            streaming,
            collapsed,
            user_toggled,
        } => render_reasoning(
            text,
            ix,
            *streaming,
            *collapsed,
            *user_toggled,
            theme,
            tool_ctx,
        ),
        ConvItem::ToolCall(t) => {
            if t.name == "exit_plan_mode" {
                render_plan_card(t, ix, theme, tool_ctx)
            } else {
                render_tool_call(t, ix, theme, tool_ctx)
            }
        }
        ConvItem::AgentTask(t) => render_agent_task(t, ix, theme, agent_ctx, tool_ctx),
        ConvItem::Error(msg) => render_error(msg, ix, theme),
        ConvItem::Notice(msg) => render_notice(msg, ix, theme),
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

/// Copy button visible only when the parent element (group) is hovered.
/// The caller must attach `.group(name)` to the enclosing wrapper.
fn copy_button_hoverable(
    ix: usize,
    prefix: &'static str,
    group: impl Into<gpui::SharedString>,
    text: String,
) -> gpui::Div {
    let group = group.into();
    gpui::div()
        .opacity(0.0)
        .group_hover(group, |s| s.opacity(1.0))
        .child(copy_button(ix, prefix, text))
}

/// Render a user message as a right-aligned bubble, matching Codex.app's turn shape.
pub fn render_user(text: &str, ix: usize, theme: &Theme) -> gpui::AnyElement {
    let group = format!("user-{ix}");
    gpui::div()
        .group(group.clone())
        .relative()
        .w_full()
        .child(render_user_turn_marker(ix, &group, theme))
        .child(render_user_turn_preview(text, ix, &group, theme))
        .child(
            h_flex().w_full().justify_end().child(
                v_flex()
                    .group(group.clone())
                    .max_w(px(560.))
                    .min_w_0()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.secondary.opacity(0.65))
                    .child(h_flex().w_full().justify_end().child(copy_button_hoverable(
                        ix,
                        "copy-user",
                        group,
                        text.to_string(),
                    )))
                    .child(
                        gpui::div()
                            .text_sm()
                            .line_height(gpui::relative(1.5))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.secondary_foreground)
                            .child(chat_markdown_tv(("user-text", ix), text, theme, false)),
                    ),
            ),
        )
        .into_any_element()
}

fn render_user_turn_marker(ix: usize, group: &str, theme: &Theme) -> gpui::AnyElement {
    let group: SharedString = group.to_string().into();
    v_flex()
        .id(("user-turn-marker", ix))
        .absolute()
        .left(px(-42.))
        .top(px(4.))
        .w(px(24.))
        .items_center()
        .gap(px(3.))
        .cursor_pointer()
        .children((0..9).map(|i| {
            let active = i == 4;
            let width = if active { 22. } else { 9. };
            let color = if active {
                theme.muted_foreground.opacity(0.72)
            } else {
                theme.muted_foreground.opacity(0.28)
            };
            let hover_color = if active {
                theme.foreground
            } else {
                theme.muted_foreground.opacity(0.52)
            };
            gpui::div()
                .w(px(width))
                .h(px(1.5))
                .rounded_full()
                .bg(color)
                .group_hover(group.clone(), move |s| s.bg(hover_color))
        }))
        .into_any_element()
}

fn render_user_turn_preview(text: &str, ix: usize, group: &str, theme: &Theme) -> gpui::AnyElement {
    let group: SharedString = group.to_string().into();
    let (title, body) = user_turn_preview_text(text);
    v_flex()
        .id(("user-turn-preview", ix))
        .absolute()
        .left(px(-284.))
        .top(px(-10.))
        .w(px(232.))
        .gap_1()
        .px_3()
        .py_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .shadow_md()
        .invisible()
        .group_hover(group, |s| s.visible())
        .child(
            gpui::div()
                .text_xs()
                .font_weight(gpui::FontWeight::BOLD)
                .line_height(gpui::relative(1.35))
                .text_color(theme.foreground)
                .child(title),
        )
        .child(
            gpui::div()
                .text_xs()
                .line_height(gpui::relative(1.38))
                .text_color(theme.muted_foreground)
                .child(body),
        )
        .into_any_element()
}

fn user_turn_preview_text(text: &str) -> (String, String) {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let title_source = lines.next().unwrap_or(text.trim());
    let body_source = lines.collect::<Vec<_>>().join(" ");
    let body_source = if body_source.is_empty() {
        text.trim()
    } else {
        body_source.as_str()
    };
    (truncate(title_source, 32), truncate(body_source, 96))
}

/// Render an assistant message as plain transcript text. The model label is intentionally hidden here;
/// the active model is surfaced in the composer and environment panel.
pub fn render_assistant(
    text: &str,
    streaming: bool,
    token_usage: Option<&TokenUsage>,
    ix: usize,
    _role: &str,
    theme: &Theme,
) -> gpui::AnyElement {
    v_flex()
        .group(format!("assistant-{ix}"))
        .w_full()
        .gap_1()
        .child(
            h_flex()
                .items_start()
                .gap_2()
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .line_height(gpui::relative(1.55))
                        .text_color(theme.foreground)
                        .child(render_text_body(text, streaming, ("assistant", ix), theme)),
                )
                .child(copy_button_hoverable(
                    ix,
                    "copy-assistant",
                    format!("assistant-{ix}"),
                    text.to_string(),
                )),
        )
        .children(token_usage.and_then(|u| render_token_footer(u, theme)))
        .into_any_element()
}

/// One-line per-turn token breakdown shown under an assistant reply once the
/// turn has ended (hidden while streaming or when the provider sent no usage).
fn render_token_footer(u: &TokenUsage, theme: &Theme) -> Option<gpui::AnyElement> {
    // Only surface non-zero fields so a fresh turn (e.g. a cached-only request
    // with no output yet) doesn't paint a row of zeroes.
    let fields: Vec<(&str, u64)> = [
        ("Input", u.input_tokens),
        ("Output", u.output_tokens),
        ("Cache read", u.cache_read_input_tokens),
        ("Cache creation", u.cache_creation_input_tokens),
    ]
    .into_iter()
    .filter(|(_, v)| *v > 0)
    .collect();
    if fields.is_empty() {
        return None;
    }
    Some(
        h_flex()
            .gap_2()
            .text_xs()
            .line_height(gpui::relative(1.4))
            .text_color(theme.muted_foreground)
            .children(
                fields
                    .into_iter()
                    .map(|(label, v)| gpui::div().child(format!("{label}: {v}"))),
            )
            .into_any_element(),
    )
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
            .line_height(gpui::relative(1.55))
            .whitespace_normal()
            .text_color(theme.foreground)
            .child(shown)
            .into_any_element()
    } else {
        gpui::div()
            .id(id.clone())
            .text_sm()
            .line_height(gpui::relative(1.55))
            .child(chat_markdown_tv(id, text, theme, false))
            .into_any_element()
    }
}

/// Render a reasoning (thinking) block: expanded while streaming, collapsed when done, with a copy button.
/// Clicking the header toggles collapsed state (like tool-call cards), tracked by `user_toggled` so the user's
/// manual choice survives subsequent status transitions.
pub fn render_reasoning(
    text: &str,
    ix: usize,
    streaming: bool,
    collapsed: bool,
    _user_toggled: bool,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
) -> gpui::AnyElement {
    let chevron = if collapsed {
        IconName::ChevronRight
    } else {
        IconName::ChevronDown
    };
    let weak_workspace = tool_ctx.map(|c| c.weak.clone());
    let mut block = v_flex()
        .group(format!("reasoning-{ix}"))
        .w_full()
        .gap_1()
        .child(
            h_flex()
                .id(("reasoning-header", ix))
                .w_full()
                .min_h(px(24.))
                .py_0p5()
                .gap_1p5()
                .items_center()
                .rounded(theme.radius)
                .cursor_pointer()
                .text_xs()
                .text_color(theme.muted_foreground)
                .hover(|s| s.bg(theme.accent.opacity(0.05)))
                .on_click(move |_, _window, cx: &mut App| {
                    let Some(weak) = weak_workspace.clone() else {
                        return;
                    };
                    let ix_click = ix;
                    let _ = weak.update(cx, |w, cx| {
                        let conv = w.conversation.clone();
                        conv.update(cx, |c, cx| {
                            if let Some(item) = c.items().get(ix_click) {
                                item.update(cx, |item, cx| {
                                    if let ConvItem::Reasoning {
                                        collapsed,
                                        user_toggled,
                                        ..
                                    } = item.kind_mut()
                                    {
                                        *collapsed = !*collapsed;
                                        *user_toggled = true;
                                    }
                                    cx.notify();
                                });
                            }
                        });
                        w.list_state().remeasure();
                        cx.notify();
                    });
                })
                .child(Icon::new(chevron).xsmall())
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(i18n::t("message-reasoning")),
                )
                .child(copy_button_hoverable(
                    ix,
                    "copy-reasoning",
                    format!("reasoning-{ix}"),
                    text.to_string(),
                )),
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
        .group(format!("error-{ix}"))
        .w_full()
        .gap_1()
        .px_3()
        .py_2()
        .rounded(theme.radius)
        .bg(theme.danger.opacity(0.06))
        .child(
            h_flex()
                .w_full()
                .justify_between()
                .items_center()
                .child(
                    gpui::div()
                        .text_sm()
                        .text_color(theme.danger)
                        .child(i18n::t("message-error")),
                )
                .child(copy_button_hoverable(
                    ix,
                    "copy-error",
                    format!("error-{ix}"),
                    msg.to_string(),
                )),
        )
        .child(
            gpui::div()
                .text_sm()
                .text_color(theme.danger)
                .child(chat_markdown_tv(("error", ix), msg, theme, false)),
        )
        .into_any_element()
}

/// Render an ephemeral system notice — status toggles, slash-command acks.
/// Neutral tones so positive state changes (e.g. "YOLO 模式已开启") do not
/// read as a runtime error.
pub fn render_notice(msg: &str, ix: usize, theme: &Theme) -> gpui::AnyElement {
    v_flex()
        .group(format!("notice-{ix}"))
        .w_full()
        .gap_1()
        .px_3()
        .py_2()
        .rounded(theme.radius)
        .bg(theme.secondary.opacity(0.15))
        .child(
            h_flex()
                .w_full()
                .justify_between()
                .items_center()
                .child(
                    gpui::div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(i18n::t("message-notice")),
                )
                .child(copy_button_hoverable(
                    ix,
                    "copy-notice",
                    format!("notice-{ix}"),
                    msg.to_string(),
                )),
        )
        .child(
            gpui::div()
                .text_sm()
                .text_color(theme.foreground)
                .child(chat_markdown_tv(("notice", ix), msg, theme, false)),
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

/// Render a tool-call card: title + status icon + copy button + (collapsible)
/// monospace output. While streaming the body is always shown so the user can
/// watch the output land; once a terminal status arrives the body auto-folds
/// to a single-line card unless the user pre-toggled it open.
pub fn render_tool_call(
    item: &ToolCallItem,
    ix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
) -> gpui::AnyElement {
    use agent::ToolCallStatus;
    let (status_icon, status_color, status_label): (IconName, gpui::Hsla, SharedString) =
        match item.status {
            ToolCallStatus::PendingApproval => (
                IconName::Info,
                theme.muted_foreground,
                i18n::t("status-pending"),
            ),
            ToolCallStatus::Running => (
                IconName::LoaderCircle,
                theme.muted_foreground,
                i18n::t("status-running"),
            ),
            ToolCallStatus::Success => (
                IconName::CircleCheck,
                theme.muted_foreground,
                i18n::t("status-success"),
            ),
            ToolCallStatus::Error => (IconName::CircleX, theme.danger, i18n::t("status-error")),
            ToolCallStatus::Denied => (IconName::CircleX, theme.danger, i18n::t("status-denied")),
        };

    let title = if item.title.is_empty() {
        item.name.clone()
    } else {
        item.title.clone()
    };

    let show_body = item.streaming || !item.collapsed;
    let chevron = if item.collapsed {
        IconName::ChevronRight
    } else {
        IconName::ChevronDown
    };

    let id_for_toggle = item.id.clone();
    let weak_workspace = tool_ctx.map(|c| c.weak.clone());

    let mut card = v_flex().group(format!("tool-{ix}")).w_full().gap_1().child(
        h_flex()
            .id(("tool-header", ix))
            .w_full()
            .min_h(px(24.))
            .py_0p5()
            .gap_1p5()
            .items_center()
            .rounded(theme.radius)
            .cursor_pointer()
            .text_color(theme.muted_foreground)
            .hover(|s| s.bg(theme.accent.opacity(0.06)))
            .on_click(move |_, _window, cx: &mut App| {
                let Some(weak) = weak_workspace.clone() else {
                    return;
                };
                let _ = weak.update(cx, |w, cx| {
                    let id = id_for_toggle.clone();
                    let conv = w.conversation.clone();
                    conv.update(cx, |c, cx| {
                        if let Some(ix) = c.find_tool(&id, &*cx)
                            && let Some(item) = c.items().get(ix)
                        {
                            item.update(cx, |item, cx| {
                                if let ConvItem::ToolCall(t) = item.kind_mut() {
                                    t.collapsed = !t.collapsed;
                                    t.user_toggled = true;
                                }
                                cx.notify();
                            });
                        }
                    });
                    w.list_state().remeasure();
                    cx.notify();
                });
            })
            .child(
                Icon::new(chevron)
                    .xsmall()
                    .text_color(theme.muted_foreground),
            )
            .child(Icon::new(status_icon).xsmall().text_color(status_color))
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .truncate()
                    .text_color(theme.muted_foreground)
                    .child(truncate(&title, 80)),
            )
            .child(copy_button_hoverable(
                ix,
                "copy-tool",
                format!("tool-{ix}"),
                item.output.clone(),
            ))
            .child(
                gpui::div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(status_color)
                    .child(status_label),
            ),
    );

    let display_output = if item.streaming {
        live_tail(&item.output)
    } else {
        item.output.clone()
    };

    if show_body && !display_output.is_empty() {
        card = card.child(render_tool_output(
            &display_output,
            &item.name,
            item.streaming,
            ix,
            theme,
        ));
    }
    card.into_any_element()
}

/// Render an `exit_plan_mode` tool-call as a plan card. The body uses
/// `TextView::markdown` (assistant-style, no code-block wrapping, no height
/// cap) instead of the monospace scrollable container. PendingApproval forces
/// the body open; terminal status auto-collapses like a regular ToolCall.
fn render_plan_card(
    item: &ToolCallItem,
    ix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
) -> gpui::AnyElement {
    use agent::ToolCallStatus;
    let (status_icon, status_color, status_label): (IconName, gpui::Hsla, SharedString) =
        match item.status {
            ToolCallStatus::PendingApproval => (
                IconName::LoaderCircle,
                theme.muted_foreground,
                i18n::t("status-pending"),
            ),
            ToolCallStatus::Running => (
                IconName::LoaderCircle,
                theme.muted_foreground,
                i18n::t("status-running"),
            ),
            ToolCallStatus::Success => (
                IconName::CircleCheck,
                theme.success,
                i18n::t("status-success"),
            ),
            ToolCallStatus::Error => (IconName::CircleX, theme.danger, i18n::t("status-error")),
            ToolCallStatus::Denied => (IconName::CircleX, theme.danger, i18n::t("status-denied")),
        };

    let title = if item.title.is_empty() {
        item.name.clone()
    } else {
        item.title.clone()
    };

    // PendingApproval always shows the body; terminal status auto-collapses.
    let show_body =
        item.status == ToolCallStatus::PendingApproval || item.streaming || !item.collapsed;
    let chevron = if item.collapsed {
        IconName::ChevronRight
    } else {
        IconName::ChevronDown
    };

    let id_for_toggle = item.id.clone();
    let weak_workspace = tool_ctx.map(|c| c.weak.clone());

    let mut card = v_flex()
        .w_full()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .overflow_hidden()
        .child(
            h_flex()
                .id(("plan-header", ix))
                .w_full()
                .px_3()
                .py_1p5()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .on_click(move |_, _window, cx: &mut App| {
                    let Some(weak) = weak_workspace.clone() else {
                        return;
                    };
                    let _ = weak.update(cx, |w, cx| {
                        let id = id_for_toggle.clone();
                        let conv = w.conversation.clone();
                        conv.update(cx, |c, cx| {
                            if let Some(ix) = c.find_tool(&id, &*cx)
                                && let Some(item) = c.items().get(ix)
                            {
                                item.update(cx, |item, cx| {
                                    if let ConvItem::ToolCall(t) = item.kind_mut() {
                                        t.collapsed = !t.collapsed;
                                        t.user_toggled = true;
                                    }
                                    cx.notify();
                                });
                            }
                        });
                        w.list_state().remeasure();
                        cx.notify();
                    });
                })
                .child(
                    Icon::new(chevron)
                        .xsmall()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    Icon::new(IconName::LayoutDashboard)
                        .small()
                        .text_color(theme.accent),
                )
                .child(
                    gpui::div()
                        .flex_1()
                        .text_sm()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(theme.foreground)
                        .child(title),
                )
                .child(copy_button(ix, "copy-plan", item.output.clone()))
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

    if show_body && !item.output.is_empty() {
        card = card.child(
            gpui::div()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(theme.border)
                .text_sm()
                .text_color(theme.foreground)
                .child(chat_markdown_tv(
                    ("plan-text", ix),
                    item.output.as_str(),
                    theme,
                    false,
                )),
        );
    }

    card.into_any_element()
}

/// Fixed-height container with the tool's output. While streaming we paint a
/// plain monospace run (no markdown re-parse per chunk); once the final
/// `ToolResult` lands we mount the syntax-highlighted, scrollable
/// `TextView::markdown`. The container keeps a deterministic height either way
/// so the parent card (and the list) reports a stable layout.
fn render_tool_output(
    output: &str,
    tool_name: &str,
    streaming: bool,
    ix: usize,
    theme: &Theme,
) -> gpui::AnyElement {
    let container = gpui::div()
        .id(("tool-output", ix))
        .h(px(180.))
        .overflow_hidden()
        .px_3()
        .py_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary.opacity(0.22))
        .text_xs()
        .text_color(theme.muted_foreground);
    if streaming {
        // Plain monospace run, one div per line, while lines are still
        // arriving — no Tree-sitter, no shaped-text layout per chunk. Matches
        // the assistant body's streaming rule. gpui has no `WhiteSpace::Pre`,
        // so we split on newlines to preserve line structure.
        container
            .font_family(theme.mono_font_family.clone())
            .children(
                output
                    .split('\n')
                    .map(|line| gpui::div().child(line.to_string())),
            )
            .into_any_element()
    } else {
        let lang = lang_hint_for_tool(tool_name);
        let code = if let Some(l) = lang {
            format!("```{l}\n{output}\n```")
        } else {
            format!("```\n{output}\n```")
        };
        container
            .child(markdown_tv(("tool-output-text", ix), code, theme, true))
            .into_any_element()
    }
}

/// Render a sub-agent task card: title + status icon + chevron to expand the
/// child conversation. Collapsed shows the live streamed tail (or the final
/// result once the sub-agent stops); expanded rebuilds the child `Thread`'s
/// messages into a nested conversation and renders each item recursively.
/// `tool_ctx` is forwarded to recursive `render_item` calls so any nested
/// tool-call cards keep their own collapse state.
pub fn render_agent_task(
    item: &AgentTaskItem,
    ix: usize,
    theme: &Theme,
    agent_ctx: Option<&AgentTaskCtx>,
    tool_ctx: Option<&ToolCallCtx>,
) -> gpui::AnyElement {
    use agent::ToolCallStatus;
    let (status_color, status_label): (gpui::Hsla, SharedString) = match item.status {
        ToolCallStatus::PendingApproval => (theme.muted_foreground, i18n::t("status-pending")),
        ToolCallStatus::Running => (theme.muted_foreground, i18n::t("status-running")),
        ToolCallStatus::Success => (theme.success, i18n::t("status-success")),
        ToolCallStatus::Error => (theme.danger, i18n::t("status-error")),
        ToolCallStatus::Denied => (theme.danger, i18n::t("status-denied")),
    };

    let expanded = agent_ctx.is_some_and(|c| c.expanded.contains(&item.id));
    let chevron = if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    };
    let id_for_toggle = item.id.clone();
    let weak = agent_ctx.map(|c| c.weak.clone());
    let copy_text = if item.final_text.is_empty() {
        item.sub_text.clone()
    } else {
        item.final_text.clone()
    };

    let mut card = v_flex().group(format!("agent-{ix}")).w_full().child(
        h_flex()
            .id(("agent-header", ix))
            .w_full()
            .min_h(px(24.))
            .px_2()
            .py_1()
            .gap_1p5()
            .items_center()
            .rounded(theme.radius)
            .cursor_pointer()
            .hover(|s| s.bg(theme.secondary.opacity(0.5)))
            .on_click(move |_, _window, cx: &mut App| {
                let Some(weak) = weak.clone() else {
                    return;
                };
                let _ = weak.update(cx, |w, cx| {
                    if !w.expanded_tasks.insert(id_for_toggle.clone()) {
                        w.expanded_tasks.remove(&id_for_toggle);
                    }
                    w.list_state().remeasure();
                    cx.notify();
                });
            })
            .child(
                Icon::new(chevron)
                    .xsmall()
                    .text_color(theme.muted_foreground),
            )
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .truncate()
                    .text_color(theme.muted_foreground)
                    .child(truncate(&item.title, 80)),
            )
            .child(copy_button_hoverable(
                ix,
                "copy-agent",
                format!("agent-{ix}"),
                copy_text,
            ))
            .child(
                gpui::div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(status_color)
                    .child(status_label),
            ),
    );

    let collapsed_body = if item.streaming {
        live_tail(&item.sub_text)
    } else if !item.final_text.is_empty() {
        item.final_text.clone()
    } else {
        item.sub_text.clone()
    };

    if expanded {
        let sub_items = build_items(&item.sub_messages, &HashMap::new());
        if sub_items.is_empty() {
            if !collapsed_body.is_empty() {
                card = card.child(render_agent_body(&collapsed_body, ix, theme));
            }
        } else {
            card = card.child(
                v_flex()
                    .ml_3()
                    .pl_3()
                    .py_1()
                    .gap_3()
                    .border_l_1()
                    .border_color(theme.border.opacity(0.8))
                    .children(sub_items.iter().enumerate().map(|(six, sitem)| {
                        render_item(sitem, six, "agent", theme, agent_ctx, tool_ctx)
                    })),
            );
        }
    } else if item.streaming && !collapsed_body.is_empty() {
        card = card.child(render_agent_preview(&collapsed_body, ix, theme));
    }
    card.into_any_element()
}

/// Short live preview for a running sub-agent. Completed tasks collapse back to
/// a single status row so the final assistant answer remains the visual focus.
fn render_agent_preview(text: &str, ix: usize, theme: &Theme) -> gpui::AnyElement {
    if text.is_empty() {
        return gpui::div().into_any_element();
    }
    gpui::div()
        .id(("agent-preview", ix))
        .ml_5()
        .max_h(px(92.))
        .overflow_hidden()
        .px_2()
        .py_1()
        .rounded(theme.radius)
        .bg(theme.secondary.opacity(0.12))
        .font_family(theme.mono_font_family.clone())
        .text_xs()
        .line_height(gpui::relative(1.35))
        .text_color(theme.muted_foreground)
        .children(
            text.split('\n')
                .map(|line| gpui::div().child(line.to_string())),
        )
        .into_any_element()
}

/// Transcript-style fallback body for an expanded sub-agent when no message
/// snapshot is available.
fn render_agent_body(text: &str, ix: usize, theme: &Theme) -> gpui::AnyElement {
    if text.is_empty() {
        return gpui::div().into_any_element();
    }
    gpui::div()
        .id(("agent-body", ix))
        .ml_3()
        .pl_3()
        .py_1()
        .border_l_1()
        .border_color(theme.border.opacity(0.8))
        .text_sm()
        .line_height(gpui::relative(1.55))
        .text_color(theme.foreground)
        .child(chat_markdown_tv(
            ("agent-body-text", ix),
            text,
            theme,
            false,
        ))
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
    let mut s = format!("{}\n", i18n::t("message-omitted-prefix"));
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

/// Build a flat `ConvItem` list from a `Thread`'s canonical message list.
/// Shared by `ConversationState::rebuild_from_messages` (top-level, wraps each
/// item in its own `Entity`) and the nested sub-agent panel (renders plain
/// items inline, since the snapshot is static once expanded).
///
/// Tool calls pair ToolUse with ToolResult by `tool_use_id`; an unpaired side
/// becomes its own item.
pub fn build_items(messages: &[Message], usage: &HashMap<String, TokenUsage>) -> Vec<ConvItem> {
    let mut items: Vec<ConvItem> = Vec::new();
    // Id of the most recent user message; usage is keyed by it, so an
    // assistant reply inherits the usage of the user message preceding it.
    let mut last_user_id: Option<&str> = None;
    for m in messages {
        match m.role {
            Role::User => {
                last_user_id = Some(m.id.as_str());
                // Text becomes a user bubble; ToolResult blocks pair back to the
                // ToolCall item emitted from the preceding assistant ToolUse.
                // ToolResults live in user messages per the Anthropic wire contract.
                let text: String = m
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        MessageContent::Text(t) | MessageContent::Thinking { text: t, .. } => {
                            Some(t.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !text.is_empty() {
                    items.push(ConvItem::User(text));
                }
                for c in &m.content {
                    if let MessageContent::ToolResult(tr) = c {
                        pair_tool_result(&mut items, tr);
                    }
                }
            }
            Role::Assistant => {
                for c in &m.content {
                    match c {
                        MessageContent::Text(t) => {
                            items.push(ConvItem::Assistant {
                                text: t.clone(),
                                streaming: false,
                                token_usage: last_user_id.and_then(|id| usage.get(id).copied()),
                            });
                        }
                        MessageContent::Thinking { text, .. } => {
                            items.push(ConvItem::Reasoning {
                                text: text.clone(),
                                streaming: false,
                                collapsed: true,
                                user_toggled: false,
                            });
                        }
                        MessageContent::ToolUse(tu) => {
                            if tu.name.as_ref() == "agent" {
                                let title = agent::thread::tool_title(tu.name.as_ref(), &tu.input);
                                items.push(ConvItem::AgentTask(AgentTaskItem {
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
                                items.push(ConvItem::ToolCall(ToolCallItem {
                                    id: tu.id.clone(),
                                    name: tu.name.to_string(),
                                    title: tu.name.to_string(),
                                    status: ToolCallStatus::Success,
                                    output: String::new(),
                                    is_error: false,
                                    streaming: false,
                                    collapsed: true,
                                    user_toggled: false,
                                }));
                            }
                        }
                        MessageContent::ToolResult(tr) => {
                            // Defensive: tool results normally live in user messages,
                            // but pair them here too if they ever appear in an assistant turn.
                            pair_tool_result(&mut items, tr);
                        }
                        MessageContent::Image { .. } => {}
                    }
                }
            }
            Role::System => {}
        }
    }
    items
}

/// Attach a tool_result to its matching item by id. Sub-agent results land in
/// `AgentTaskItem::final_text`; ordinary tool results land in `ToolCallItem::output`.
/// If no match exists, emit a standalone ToolCall result item.
fn pair_tool_result(items: &mut Vec<ConvItem>, tr: &LanguageModelToolResult) {
    let status = if tr.is_error {
        ToolCallStatus::Error
    } else {
        ToolCallStatus::Success
    };
    let ix = items.iter().position(|i| match i {
        ConvItem::AgentTask(t) => t.id == tr.tool_use_id,
        ConvItem::ToolCall(t) => t.id == tr.tool_use_id,
        _ => false,
    });
    let Some(ix) = ix else {
        items.push(ConvItem::ToolCall(ToolCallItem {
            id: tr.tool_use_id.clone(),
            name: tr.tool_name.to_string(),
            title: String::new(),
            status,
            output: tr.content.clone(),
            is_error: tr.is_error,
            streaming: false,
            collapsed: !matches!(
                status,
                ToolCallStatus::Running | ToolCallStatus::PendingApproval
            ),
            user_toggled: false,
        }));
        return;
    };
    match &mut items[ix] {
        ConvItem::AgentTask(t) => {
            // On reload the in-memory snapshot map is empty, so restore the
            // sub-conversation from the persisted JSON envelope.
            t.final_text = agent_final_text(&tr.content);
            t.sub_messages = agent_sub_messages(&tr.content).unwrap_or_default();
            t.is_error = tr.is_error;
            t.status = status;
        }
        ConvItem::ToolCall(t) => {
            t.output = tr.content.clone();
            t.is_error = tr.is_error;
            t.status = status;
            if t.name.is_empty() {
                t.name = tr.tool_name.to_string();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_chat_markdown;

    #[test]
    fn normalize_chat_markdown_flattens_atx_headings() {
        assert_eq!(
            normalize_chat_markdown("# Big\n\n### Small"),
            "***Big***\n──────\n\n**Small**"
        );
    }

    #[test]
    fn normalize_chat_markdown_keeps_code_fence_content() {
        assert_eq!(
            normalize_chat_markdown("```md\n# still code\n```\n# Title"),
            "```md\n# still code\n```\n***Title***\n──────"
        );
    }

    #[test]
    fn normalize_chat_markdown_flattens_setext_headings() {
        assert_eq!(
            normalize_chat_markdown("Title\n-----\nbody"),
            "**Title**\nbody"
        );
    }
}
