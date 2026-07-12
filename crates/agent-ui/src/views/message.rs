//! Rendering of a single conversation message.
//!
//! - Text blocks render via `Markdown` with per-block copy buttons (cross-block
//!   selection + Cmd+C copy lands in a follow-up).
//! - User: a full-width bordered turn block with a muted metadata header
//!   (role · model · project) — the Claude Code TUI turn-block look, not a
//!   chat bubble.
//! - Assistant: a full-width block with a role label + markdown body.
//! - Reasoning: a collapsible block, indented secondary text with a left border.
//! - ToolCall: a card with title + status icon + monospace output.
//!
//! Streaming assistant / reasoning bodies render formatted markdown throughout
//! the stream via `Markdown::blocks` (blocks from an `IncrementalParser`), with
//! a trailing cursor on the last block. `Stop` finalizes the parser (one full
//! parse for consistency) and flips the streaming flag off — no reflow jump.
//! Streaming tool output is stricter: while lines are still arriving we paint
//! a plain monospace run and only mount the syntax-highlighted `Markdown` once
//! the final `ToolResult` lands.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use agent::language_model::{LanguageModelToolResult, MessageContent, Role};
use agent::thread::ApprovalMode;
use agent::tools::agent::{agent_final_text, agent_sub_messages};
use agent::{Message, TokenUsage, ToolCallStatus, i18n};
use base64::Engine as _;
use chrono::{Datelike as _, Local, TimeZone as _};
use gpui::prelude::*;
use gpui::{App, ClipboardItem, CursorStyle, Render, SharedString, Task, WeakEntity, px};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::Input,
    spinner::Spinner,
    tag::{Tag, TagVariant},
    v_flex,
};
use manox_components::markdown::ast::Block;
use manox_components::markdown::incremental::IncrementalParser;
use manox_components::markdown::{HeadingMode, Markdown};
use manox_components::turn_frame::TurnFrame;

use crate::Workspace;
use crate::conversation::{
    AgentTaskItem, ConvItem, ThinkingContainer, ToolCallItem, UserImage, UserTurnMeta,
};
use crate::views::centered;
use crate::workspace::AskCardSnapshot;

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
    pub(crate) ask: Option<AskCardSnapshot>,
    pub(crate) pending_plan_id: Option<String>,
}

/// Markdown renderer with theme-aware syntax highlighting.
///
/// `scrollable = true` mounts an internal vertical scrollbar; the renderer
/// sizes to its parent's box, so the parent must carry a fixed height (use
/// `h(...)` rather than `max_h(...)`).
///
/// `scrollable = false` clips overflow horizontally — the renderer itself is a
/// `w_full` + `min_w_0` column, so a long unbreakable run cannot push past the
/// env-card gutter.
fn markdown_tv(
    id: impl Into<gpui::ElementId>,
    text: impl Into<gpui::SharedString>,
    theme: &Theme,
    scrollable: bool,
) -> gpui::AnyElement {
    Markdown::new(id, text)
        .theme(theme)
        .selectable(true)
        .scrollable(scrollable)
        .heading_mode(HeadingMode::Uniform)
        .into_any_element()
}

/// One renderable conversation item, owned by its own gpui `Entity` so a
/// streaming delta notifies (and re-renders) only this item rather than the
/// whole workspace. `id` is the item's stable list index (the conversation
/// only ever appends, so the index never shifts); it keys element ids within
/// the entity's own namespace. `role` is the model display name captured at
/// creation time so a finished bubble keeps its model label after the user
/// switches models.
///
/// `parser` holds the incremental markdown state for text-bearing items
/// (Assistant, Reasoning). It is `None` for non-text items (ToolCall, Error,
/// etc.) — those render static text via `markdown_tv` which parses internally.
/// `pending_parse` + `dirty` implement backpressure: when a parse is in
/// flight, further deltas set `dirty` instead of starting a new parse, and the
/// completion handler re-arms if dirty.
pub struct MessageItem {
    kind: ConvItem,
    role: String,
    id: usize,
    /// Weak handle to the owning `Workspace`, used to read/toggle the shared
    /// `expanded_tasks` set from `AgentTask` cards.
    weak_workspace: WeakEntity<Workspace>,
    /// Incremental markdown parser for streaming text bodies (Assistant,
    /// Reasoning). `None` for non-text items.
    parser: Option<IncrementalParser>,
    /// In-flight background parse. While `Some`, new deltas set `dirty` rather
    /// than spawning a competing parse.
    pending_parse: Option<Task<()>>,
    /// Set when a delta arrived while `pending_parse` was `Some`. The
    /// completion handler re-arms a parse when this is true.
    dirty: bool,
}

impl MessageItem {
    pub fn new(kind: ConvItem, role: String, id: usize, weak: WeakEntity<Workspace>) -> Self {
        let is_text = matches!(
            kind,
            ConvItem::Assistant { .. } | ConvItem::Reasoning { .. }
        );
        Self {
            kind,
            role,
            id,
            weak_workspace: weak,
            parser: if is_text {
                Some(IncrementalParser::new())
            } else {
                None
            },
            pending_parse: None,
            dirty: false,
        }
    }

    pub fn kind(&self) -> &ConvItem {
        &self.kind
    }

    pub fn kind_mut(&mut self) -> &mut ConvItem {
        &mut self.kind
    }

    /// The parsed blocks for a text-bearing item. Returns `None` for non-text
    /// items (they don't carry an `IncrementalParser`).
    pub fn blocks(&self) -> Option<Arc<Vec<Block>>> {
        self.parser.as_ref().map(|p| p.blocks())
    }

    /// Feed a text delta to the incremental parser (synchronous fast path). For
    /// the streaming body, this updates the blocks in place on the foreground
    /// thread; Phase 3 backpressure wraps this in a background spawn when the
    /// body is long enough to jank the frame.
    pub fn update_text(&mut self, full_text: &str) {
        if let Some(parser) = &mut self.parser {
            parser.update(full_text);
        }
    }

    /// Finalize the parser (one full parse for consistency) without touching
    /// the streaming/collapse flags. Used by `rebuild_from_messages` for non-
    /// streaming text items that need their blocks populated.
    pub fn finalize_parser(&mut self) {
        if let Some(parser) = &mut self.parser {
            parser.finalize();
        }
    }

    /// Kick off a background parse if no parse is in flight; otherwise mark
    /// dirty so the completion handler re-arms. The background task parses on a
    /// worker thread and updates the parser state + notifies on completion.
    pub fn schedule_parse(&mut self, full_text: String, cx: &mut gpui::Context<Self>) {
        if self.pending_parse.is_some() {
            self.dirty = true;
            return;
        }
        self.dirty = false;
        let Some(parser) = self.parser.as_mut() else {
            return;
        };
        // Snapshot the parser state so the background task can update a clone
        // without holding the entity lock. The result (new blocks + frozen
        // offset) is applied on the foreground thread.
        let mut snapshot = parser.clone();
        let task = cx.background_spawn(async move {
            snapshot.update(&full_text);
            snapshot
        });
        let weak = cx.weak_entity();
        self.pending_parse = Some(cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            let updated = task.await;
            this.update(cx, |item, cx| {
                // The authoritative text is the item's `text` field, which the
                // append branch mutates *before* re-arming. The background
                // snapshot may lag it (deltas landed during the parse), so only
                // adopt the snapshot when it matches the authoritative text —
                // overwriting with a stale snapshot would regress the rendered
                // body and, after stream stop, permanently drop the last
                // delta. When the snapshot lagged, the foreground `update_text`
                // already holds the authoritative text, so leaving the parser
                // is correct; the dirty re-arm catches up.
                let authoritative = match item.kind() {
                    ConvItem::Assistant { text, .. } | ConvItem::Reasoning { text, .. } => {
                        text.clone()
                    }
                    _ => return,
                };
                if let Some(parser) = &mut item.parser
                    && updated.text() == authoritative
                {
                    *parser = updated;
                }
                item.pending_parse = None;
                if item.dirty {
                    item.dirty = false;
                    item.schedule_parse(authoritative, cx);
                }
                cx.notify();
            })
            .ok();
            let _ = weak;
        }));
    }

    /// Flip streaming flags off on a `Stop`. Called once per stop, so the
    /// O(items) walk is harmless. `terminal` distinguishes a turn-ending stop
    /// (`EndTurn`/`MaxTokens`/`Refusal`/cancel/error) from a mid-turn
    /// `StopReason::ToolUse`: a terminal stop freezes the activity segment
    /// (pins elapsed, auto-collapses) and the tool-call cards; a ToolUse stop
    /// only finalizes the assistant/reasoning text streaming so the next model
    /// response's tool calls fold into the same segment. The parser always gets
    /// a final pass so the frozen prefix + tail match a one-shot full parse.
    pub fn finalize_streaming(&mut self, terminal: bool) {
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
            ConvItem::Thinking(t) if terminal => {
                // Turn ended: freeze the segment, pin elapsed, auto-collapse
                // entries the user didn't pin. `finalize_segment` is
                // idempotent with `recompute_streaming`'s pinning.
                t.finalize_segment();
                for entry in &mut t.entries {
                    entry.streaming = false;
                    if matches!(
                        entry.status,
                        ToolCallStatus::Success | ToolCallStatus::Error | ToolCallStatus::Denied
                    ) {
                        entry.collapsed = !entry.user_toggled;
                    }
                }
                t.collapsed = !t.user_toggled;
            }
            // ToolUse stop (`!terminal`): leave the segment live so the next
            // model response's tool calls fold into it.
            ConvItem::Thinking(_) => {}
            ConvItem::ToolCall(t) => {
                t.streaming = false;
                if terminal
                    && matches!(
                        t.status,
                        ToolCallStatus::Success
                            | ToolCallStatus::Continued
                            | ToolCallStatus::Error
                            | ToolCallStatus::Denied
                    )
                {
                    t.collapsed = !t.user_toggled;
                }
            }
            ConvItem::AgentTask(t) => t.streaming = false,
            _ => {}
        }
        // Final full parse to guarantee the frozen prefix + tail match a
        // one-shot full parse exactly (the tail may have been held back by
        // the \n\n boundary guard during streaming).
        if let Some(parser) = &mut self.parser {
            parser.finalize();
        }
        self.pending_parse = None;
        self.dirty = false;
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
        let tool_ctx = self.weak_workspace.upgrade().map(|ws| {
            let tool_id = match &self.kind {
                ConvItem::ToolCall(t) => Some(t.id.as_str()),
                _ => None,
            };
            let read = ws.read(cx);
            ToolCallCtx {
                weak: ws.downgrade(),
                ask: tool_id.and_then(|id| read.ask_card_snapshot(id, cx)),
                pending_plan_id: read.pending_plan_id(),
            }
        });
        let blocks_arc = self.blocks();
        let blocks = blocks_arc.as_ref().map(|b| -> &[Block] { &b[..] });
        centered(render_item(
            &self.kind,
            self.id,
            &self.role,
            &theme,
            agent_ctx.as_ref(),
            tool_ctx.as_ref(),
            blocks,
        ))
    }
}

/// Render a `ConvItem` as an element. `ix` is the entry index (stable key for collapsibles and text-block element ids).
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
    blocks: Option<&[Block]>,
) -> gpui::AnyElement {
    match item {
        ConvItem::User { text, images, meta } => {
            render_user(text, images, meta.as_ref(), ix, role, theme)
        }
        ConvItem::Assistant {
            text,
            streaming,
            token_usage: _,
        } => render_assistant(text, *streaming, ix, role, theme, blocks),
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
            blocks,
        ),
        ConvItem::Thinking(t) => render_thinking(t, ix, theme, tool_ctx),
        ConvItem::ToolCall(t) => {
            if t.name == "exit_plan_mode" {
                render_plan_card(t, ix, theme, tool_ctx)
            } else if t.name == "AskUserQuestion" {
                render_ask_user_card(t, ix, theme, tool_ctx)
            } else {
                // Ordinary tool calls fold into `Thinking`; a top-level
                // ToolCall here is the answered-state fallback for an
                // `AskUserQuestion` whose interactive snapshot is gone, or a
                // defensive orphan — render it as a plain card.
                render_tool_call(t, ix, theme, tool_ctx)
            }
        }
        ConvItem::AgentTask(t) => render_agent_task(t, ix, theme, agent_ctx, tool_ctx),
        ConvItem::Error(msg) => render_error(msg, ix, theme),
        ConvItem::Notice(msg) => render_notice(msg, ix, theme),
        ConvItem::TeamMessage { from, content } => render_team_message(from, content, ix, theme),
        ConvItem::Recap {
            summary,
            collapsed,
            user_toggled: _,
        } => render_recap(summary, *collapsed, ix, theme, tool_ctx),
        ConvItem::Retry {
            attempt,
            max_attempts,
            delay_secs,
            reason,
            detail,
            collapsed,
            user_toggled: _,
        } => render_retry(
            *attempt,
            *max_attempts,
            *delay_secs,
            reason,
            detail.as_deref(),
            *collapsed,
            ix,
            theme,
            tool_ctx,
        ),
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

/// Render a user message as one full-width turn frame. The frame itself owns
/// the accent border; the bottom edge keeps only the two corners so the center
/// stays open. `min_w_0` end to end keeps long CJK / unbreakable runs from
/// collapsing the block to min-content (the failure mode of the old bubble).
pub fn render_user(
    text: &str,
    images: &[UserImage],
    meta: Option<&UserTurnMeta>,
    ix: usize,
    model: &str,
    theme: &Theme,
) -> gpui::AnyElement {
    let model_id = meta
        .map(|m| m.model_id.as_str())
        .filter(|m| !m.is_empty())
        .unwrap_or(model);
    let mut header_parts = vec![i18n::t("message-user-role").to_string()];
    if let Some(meta) = meta {
        header_parts.push(format_user_turn_time(meta.timestamp));
    }
    if !model_id.is_empty() {
        header_parts.push(model_id.to_string());
    }
    let header = header_parts.join(" > ");
    let group = format!("user-{ix}");
    let accent = meta
        .and_then(|m| m.approval_mode)
        .map(|mode| approval_mode_color(mode, theme))
        .unwrap_or(theme.accent);

    TurnFrame::new(theme)
        .group(group.clone())
        .accent(accent)
        .header(
            gpui::div()
                .text_color(theme.muted_foreground)
                .child(SharedString::from(header)),
        )
        .trailing(copy_button_hoverable(
            ix,
            "copy-user",
            group,
            text.to_string(),
        ))
        .child(
            v_flex()
                .w_full()
                .min_w_0()
                .overflow_hidden()
                .gap_2()
                .text_sm()
                .text_color(theme.foreground)
                .children(images.iter().map(|ui| {
                    gpui::img(ui.0.clone())
                        .max_w(px(280.))
                        .max_h(px(280.))
                        .rounded(theme.radius)
                        .object_fit(gpui::ObjectFit::ScaleDown)
                }))
                .child(markdown_tv(
                    ("user-text", ix),
                    text.to_string(),
                    theme,
                    false,
                )),
        )
        .into_any_element()
}

fn approval_mode_color(mode: ApprovalMode, theme: &Theme) -> gpui::Hsla {
    match mode {
        ApprovalMode::OnRequest => theme.success,
        ApprovalMode::AutoReview => theme.info,
        ApprovalMode::Yolo => theme.danger,
    }
}

fn format_user_turn_time(timestamp: i64) -> String {
    let Some(sent) = Local.timestamp_opt(timestamp, 0).single() else {
        return String::new();
    };
    let now = Local::now();
    if sent.date_naive() == now.date_naive() {
        sent.format("%H:%M").to_string()
    } else if sent.year() == now.year() {
        sent.format("%m-%d %H:%M").to_string()
    } else {
        sent.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// Render an assistant message: role label + copy button + markdown body.
/// `blocks` carries the pre-parsed blocks from the `IncrementalParser` so the
/// renderer never re-parses — the streaming path renders formatted markdown
/// throughout, with a cursor on the last block. `role` is the model display
/// name (dynamic).
pub fn render_assistant(
    text: &str,
    streaming: bool,
    ix: usize,
    role: &str,
    theme: &Theme,
    blocks: Option<&[Block]>,
) -> gpui::AnyElement {
    v_flex()
        .group(format!("assistant-{ix}"))
        .w_full()
        .gap_1()
        .child(
            h_flex()
                .gap_1()
                .items_center()
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(role.to_string()),
                )
                .child(gpui::div().flex_1())
                .child(copy_button_hoverable(
                    ix,
                    "copy-assistant",
                    format!("assistant-{ix}"),
                    text.to_string(),
                )),
        )
        .child(render_text_body(
            text,
            streaming,
            ("assistant", ix),
            theme,
            blocks,
        ))
        .into_any_element()
}

/// Render the assistant / reasoning body. When `blocks` is supplied (from the
/// `IncrementalParser`), the renderer uses them directly — no re-parse per
/// frame. When `None` (e.g. static mounts without a parser), it falls back to
/// `Markdown::new` which parses internally. `streaming` only controls the
/// trailing cursor on the last block; the full markdown layout renders
/// throughout streaming.
fn render_text_body(
    text: &str,
    streaming: bool,
    id: impl Into<gpui::ElementId>,
    theme: &Theme,
    blocks: Option<&[Block]>,
) -> gpui::AnyElement {
    let md = match blocks {
        Some(blocks) if !blocks.is_empty() => {
            Markdown::blocks(id, text.to_string(), Arc::from(blocks.to_vec()))
        }
        _ => Markdown::new(id, text.to_string()),
    };
    md.theme(theme)
        .selectable(true)
        .streaming(streaming)
        .heading_mode(HeadingMode::Uniform)
        .into_any_element()
}

/// Render a reasoning (thinking) block: expanded while streaming, collapsed when done, with a copy button.
/// Clicking the header toggles collapsed state (like tool-call cards), tracked by `user_toggled` so the user's
/// manual choice survives subsequent status transitions.
//
// Each arg is a distinct render input; the function is a leaf render helper,
// not a public API. Splitting would only forward the same values through an
// intermediate struct without reducing complexity.
#[allow(clippy::too_many_arguments)]
pub fn render_reasoning(
    text: &str,
    ix: usize,
    streaming: bool,
    collapsed: bool,
    _user_toggled: bool,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
    blocks: Option<&[Block]>,
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
                .gap_1p5()
                .items_center()
                .cursor_pointer()
                .text_xs()
                .text_color(theme.muted_foreground)
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
                        cx.notify();
                    });
                })
                .child(Icon::new(chevron).xsmall())
                .child(i18n::t("message-reasoning"))
                .child(gpui::div().flex_1())
                .child(copy_button_hoverable(
                    ix,
                    "copy-reasoning",
                    format!("reasoning-{ix}"),
                    text.to_string(),
                )),
        );
    if !collapsed {
        let body = render_text_body(text, streaming, ("reasoning", ix), theme, blocks);
        block = block.child(
            gpui::div()
                .pl_3()
                .border_l_1()
                .border_color(theme.border)
                .text_sm()
                .text_color(theme.muted_foreground)
                // Thinking renders as Lilex italic; weight inherits Light from the
                // list, so this hits Lilex-LightItalic (MediumItalic under bold).
                .italic()
                .child(body),
        );
    }
    block.into_any_element()
}

/// Optional collapsible slot for `render_banner`: turns the label row into a
/// click-toggle that shows/hides the body. Used by the recap card; `None` for
/// the always-open banners (error / notice / team message / retry).
struct CollapsibleBanner {
    collapsed: bool,
    on_click: Box<dyn Fn(&mut App) + 'static>,
}

/// Unified banner card: a label row (accent-colored label + optional icon,
/// hover-revealed copy button, optional collapse chevron) over a foreground-
/// tinted body. All five non-card banners (error / notice / team message /
/// recap / retry) share this shape so only accent, label, icon, body, and
/// collapse differ between them.
// The parameter list is intentionally rich: each param maps to one slot the
// five call sites need to differentiate (accent / label / icon / group /
// copy / body / fold). Grouping them into a config struct would obscure the
// per-call-site differences the unification is meant to make visible.
#[allow(clippy::too_many_arguments)]
fn render_banner(
    accent: gpui::Hsla,
    label: SharedString,
    icon: Option<IconName>,
    group: impl Into<SharedString>,
    ix: usize,
    copy_prefix: &'static str,
    copy_text: String,
    body: gpui::AnyElement,
    theme: &Theme,
    collapsible: Option<CollapsibleBanner>,
) -> gpui::AnyElement {
    let group = group.into();
    let mut left = h_flex().items_center().gap_1().text_xs().text_color(accent);
    if let Some(c) = &collapsible {
        let chevron = if c.collapsed {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };
        left = left.child(Icon::new(chevron).xsmall());
    }
    left = left.when_some(icon, |row, name| row.child(Icon::new(name).xsmall()));
    left = left.child(label);

    let label_row = h_flex()
        .w_full()
        .justify_between()
        .items_center()
        .child(left)
        .child(copy_button_hoverable(
            ix,
            copy_prefix,
            group.clone(),
            copy_text,
        ));
    // `.id()` turns `Div` into `Stateful<Div>`, so erase to `AnyElement` to
    // keep the collapsible and non-collapsible branches one type. Read the
    // collapsed flag before `on_click` moves the closure out of `collapsible`.
    let show_body = match &collapsible {
        Some(c) => !c.collapsed,
        None => true,
    };
    let label_row: gpui::AnyElement = match collapsible {
        Some(c) => label_row
            .id(("banner-header", ix))
            .cursor_pointer()
            .on_click(move |_, _window, cx: &mut App| (c.on_click)(cx))
            .into_any_element(),
        None => label_row.into_any_element(),
    };

    let mut card = v_flex()
        .group(group)
        .w_full()
        .gap_1()
        .px_3()
        .py_2()
        .rounded(theme.radius)
        .bg(accent.opacity(0.10))
        .child(label_row);
    if show_body {
        card = card.child(
            gpui::div()
                .text_sm()
                .text_color(theme.foreground)
                .child(body),
        );
    }
    card.into_any_element()
}

/// Render an error message + copy button.
pub fn render_error(msg: &str, ix: usize, theme: &Theme) -> gpui::AnyElement {
    render_banner(
        theme.danger,
        i18n::t("message-error"),
        None,
        format!("error-{ix}"),
        ix,
        "copy-error",
        msg.to_string(),
        markdown_tv(("error", ix), msg.to_string(), theme, false),
        theme,
        None,
    )
}

/// Render an ephemeral system notice — status toggles, slash-command acks.
/// Neutral tones so positive state changes (e.g. "YOLO mode is on") do not
/// read as a runtime error.
pub fn render_notice(msg: &str, ix: usize, theme: &Theme) -> gpui::AnyElement {
    render_banner(
        theme.muted_foreground,
        i18n::t("message-notice"),
        None,
        format!("notice-{ix}"),
        ix,
        "copy-notice",
        msg.to_string(),
        markdown_tv(("notice", ix), msg.to_string(), theme, false),
        theme,
        None,
    )
}

/// A peer message from a teammate (or the leader) within a team. The `from`
/// name leads the label in the accent color; `content` is the peer's own body,
/// rendered as markdown. Accent-tinted to read apart from user / assistant
/// turns without the danger tone of an error.
pub fn render_team_message(
    from: &str,
    content: &str,
    ix: usize,
    theme: &Theme,
) -> gpui::AnyElement {
    // The whole label row is accent-colored, so `from` inherits primary here.
    let label: SharedString = format!("{from} · {}", i18n::t("message-team")).into();
    render_banner(
        theme.primary,
        label,
        None,
        format!("team-msg-{ix}"),
        ix,
        "copy-team-msg",
        content.to_string(),
        markdown_tv(("team-msg", ix), content.to_string(), theme, false),
        theme,
        None,
    )
}

/// Render a compaction Recap card: a collapsible summary of the history that
/// was folded into a handoff note. Collapsed by default; the summary body is
/// model-generated markdown (not localized), only the title is. Toggling
/// follows the same `user_toggled`-stamped pattern as reasoning blocks.
pub fn render_recap(
    summary: &str,
    collapsed: bool,
    ix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
) -> gpui::AnyElement {
    let weak_workspace = tool_ctx.map(|c| c.weak.clone());
    let on_click = Box::new(move |_cx: &mut App| {
        let Some(weak) = weak_workspace.clone() else {
            return;
        };
        let ix_click = ix;
        let _ = weak.update(_cx, |w, cx| {
            let conv = w.conversation.clone();
            conv.update(cx, |c, cx| {
                if let Some(item) = c.items().get(ix_click) {
                    item.update(cx, |item, cx| {
                        if let ConvItem::Recap {
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
            cx.notify();
        });
    }) as Box<dyn Fn(&mut App) + 'static>;
    render_banner(
        theme.muted_foreground,
        i18n::t("recap-card-title"),
        Some(IconName::BookOpen),
        format!("recap-{ix}"),
        ix,
        "copy-recap",
        summary.to_string(),
        markdown_tv(("recap", ix), summary.to_string(), theme, false),
        theme,
        Some(CollapsibleBanner {
            collapsed,
            on_click,
        }),
    )
}

/// Transient retry badge shown while the provider backs off after a 429 / 5xx
/// / network error. Replaced in place by the first real content or terminal
/// error event. Amber-toned to read as "waiting, not failed". The badge line
/// carries a short `reason` (HTTP status phrase or network error class); when a
/// provider response body is available it lands in an expandable detail slot
/// below, toggled by the same `user_toggled`-stamped pattern as recap cards.
// Mirrors render_banner: each param maps to one banner slot (attempt/max/secs
// for the badge, reason/detail for the body, collapsed/tool_ctx for the fold).
#[allow(clippy::too_many_arguments)]
pub fn render_retry(
    attempt: u32,
    max_attempts: u32,
    delay_secs: u64,
    reason: &str,
    detail: Option<&str>,
    collapsed: bool,
    ix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
) -> gpui::AnyElement {
    let badge: SharedString = i18n::t_str(
        "retry-badge",
        &[
            ("attempt", &attempt.to_string()),
            ("max", &max_attempts.to_string()),
            ("secs", &delay_secs.to_string()),
            ("reason", reason),
        ],
    );
    let copy_text = badge.to_string();
    let weak_workspace = tool_ctx.map(|c| c.weak.clone());
    let on_click = Box::new(move |_cx: &mut App| {
        let Some(weak) = weak_workspace.clone() else {
            return;
        };
        let _ = weak.update(_cx, |w, cx| {
            let conv = w.conversation.clone();
            conv.update(cx, |c, cx| {
                if let Some(item) = c.items().get(ix) {
                    item.update(cx, |item, cx| {
                        if let ConvItem::Retry {
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
            cx.notify();
        });
    }) as Box<dyn Fn(&mut App) + 'static>;
    let body = detail
        .map(|d| markdown_tv(("retry", ix), d.to_string(), theme, false))
        .unwrap_or_else(|| gpui::div().into_any_element());
    let collapsible = if detail.is_some() {
        Some(CollapsibleBanner {
            collapsed,
            on_click,
        })
    } else {
        None
    };
    render_banner(
        theme.warning,
        badge,
        Some(IconName::LoaderCircle),
        format!("retry-{ix}"),
        ix,
        "copy-retry",
        copy_text,
        body,
        theme,
        collapsible,
    )
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

/// A left-aligned disclosure chevron for collapsible MessageList rows.
/// `ChevronRight` = collapsed, `ChevronDown` = expanded. xsmall + muted so it
/// reads as secondary chrome, not a primary icon. Used by `render_thinking`
/// and `render_activity_entry` so every collapsible affordance in the activity
/// flow shares one system.
fn disclosure_icon(collapsed: bool, theme: &Theme) -> gpui::AnyElement {
    let name = if collapsed {
        IconName::ChevronRight
    } else {
        IconName::ChevronDown
    };
    Icon::new(name)
        .xsmall()
        .text_color(theme.muted_foreground)
        .into_any_element()
}

/// Render one activity segment as a Claude Code–style Thinking status line.
/// The header carries a left disclosure chevron, a spinner (while live) or a
/// static dot, the elapsed-time label, and aggregated action counts; clicking
/// toggles between the summary (plus, while live, the running/latest `⎿`
/// entry) and the full `⎿` list. Each entry is itself a one-line summary that
/// expands to its full tool output.
///
/// Collapsed visibility rules:
/// - frozen + collapsed: only the summary header (no entries) — the summary IS
///   the per-segment card, not a per-tool broadcast.
/// - streaming + collapsed: the summary plus the running entry, or if none is
///   running, the latest entry — the "what's happening right now" line.
/// - expanded: every entry in arrival order.
pub fn render_thinking(
    t: &ThinkingContainer,
    ix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
) -> gpui::AnyElement {
    let label = if t.streaming {
        i18n::t_count("thinking-live", t.started_at.elapsed().as_secs() as i64)
    } else {
        // Use the frozen terminal duration; for a freshly rebuilt historical
        // segment this is `None` and the label degrades to a bare "Thought".
        match t.frozen_secs {
            Some(secs) => i18n::t_count("thinking-done", secs as i64),
            None => i18n::t("thinking-done-label"),
        }
    };
    let summary = thinking_summary(&t.entries);
    let weak_workspace = tool_ctx.map(|c| c.weak.clone());
    let ix_click = ix;

    let mut header = h_flex()
        .id(("thinking-header", ix))
        .gap_1p5()
        .items_center()
        .cursor_pointer()
        .text_xs()
        .text_color(theme.muted_foreground)
        .on_click(move |_, _window, cx: &mut App| {
            let Some(weak) = weak_workspace.clone() else {
                return;
            };
            let _ = weak.update(cx, |w, cx| {
                let conv = w.conversation.clone();
                conv.update(cx, |c, cx| {
                    if let Some(item) = c.items().get(ix_click) {
                        item.update(cx, |item, cx| {
                            if let ConvItem::Thinking(t) = item.kind_mut() {
                                t.collapsed = !t.collapsed;
                                t.user_toggled = true;
                            }
                            cx.notify();
                        });
                    }
                });
                cx.notify();
            });
        });
    // Left disclosure chevron, before the status indicator.
    header = header.child(disclosure_icon(t.collapsed, theme));
    // A spinning loader while the segment is live; a static muted dot once frozen.
    if t.streaming {
        header = header.child(Spinner::new().xsmall().color(theme.muted_foreground));
    } else {
        header = header.child(
            gpui::div()
                .w(px(6.))
                .h(px(6.))
                .rounded_full()
                .bg(theme.muted_foreground.opacity(0.5)),
        );
    }
    header = header.child(label);
    if !summary.is_empty() {
        header = header.child(gpui::div().child(summary));
    }
    header = header.child(gpui::div().flex_1());

    let mut block = v_flex()
        .group(format!("thinking-{ix}"))
        .w_full()
        .gap_1()
        .child(header);
    // Collapsed + frozen: no entries (the summary is the card). Collapsed +
    // streaming: the running entry, or the latest if none is running. Expanded:
    // every entry.
    let visible: Vec<&ToolCallItem> = if !t.collapsed {
        t.entries.iter().collect()
    } else if t.streaming {
        // Prefer a running/streaming entry; fall back to the latest.
        t.entries
            .iter()
            .rev()
            .find(|e| {
                e.streaming
                    || matches!(
                        e.status,
                        ToolCallStatus::Running | ToolCallStatus::PendingApproval
                    )
            })
            .or(t.entries.last())
            .into_iter()
            .collect()
    } else {
        Vec::new()
    };
    if !visible.is_empty() {
        block = block.child(
            v_flex().pl_3().gap_0p5().children(
                visible
                    .iter()
                    .enumerate()
                    .map(|(eix, e)| render_activity_entry(e, eix, theme, tool_ctx)),
            ),
        );
    }
    block.into_any_element()
}

/// Render one `⎿` entry of a Thinking batch: a single-line summary (status
/// icon + tool title) that expands to the full tool output on click. The
/// container index isn't needed for element ids — each `MessageItem` entity
/// namespaces its own ids, so the entry index alone is unique within a batch.
fn render_activity_entry(
    e: &ToolCallItem,
    eix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
) -> gpui::AnyElement {
    use agent::ToolCallStatus;
    let (status_icon, status_color) = match e.status {
        ToolCallStatus::PendingApproval | ToolCallStatus::Running => {
            (IconName::LoaderCircle, theme.muted_foreground)
        }
        // `Continued` is exit_plan_mode-specific and never folds into a batch,
        // but the match stays exhaustive; grouped with the success-like
        // outcomes so a hypothetical entry reads as completed, not errored.
        ToolCallStatus::Success | ToolCallStatus::Continued => {
            (IconName::CircleCheck, theme.success)
        }
        ToolCallStatus::Error | ToolCallStatus::Denied => (IconName::CircleX, theme.danger),
        // `Cancelled` is a non-response (overlay not shown or turn cancelled),
        // not a success or an error: a muted `Minus` reads "no action taken".
        ToolCallStatus::Cancelled => (IconName::Minus, theme.muted_foreground),
    };
    let show_output = e.streaming || !e.collapsed;
    // `title` falls back to `name`, then to a generic label so an orphan
    // `ToolResult` (no matching `ToolCall`, so neither is populated — the
    // live event carries no tool_name) still renders a visible `⎿` row.
    let title = if !e.title.is_empty() {
        e.title.clone()
    } else if !e.name.is_empty() {
        e.name.clone()
    } else {
        i18n::t("thinking-tool-result").to_string()
    };
    let id_for_toggle = e.id.clone();
    let weak_workspace = tool_ctx.map(|c| c.weak.clone());

    // Italic to match #140's "tool-call chrome renders as Lilex italic" — the
    // `⎿` entry is the per-tool successor to the old `render_tool_call` card.
    // Left disclosure chevron (before `⎿`) keeps the affordance on the same
    // side as the segment header so the eye knows where to look.
    let mut row = v_flex().w_full().italic().child(
        h_flex()
            .id(("act-header", eix))
            .w_full()
            .px_2()
            .py_0p5()
            .gap_1p5()
            .items_center()
            .rounded(theme.radius)
            .cursor_pointer()
            .hover(|s| s.bg(theme.secondary.opacity(0.5)))
            .on_click(move |_, _window, cx: &mut App| {
                let Some(weak) = weak_workspace.clone() else {
                    return;
                };
                let _ = weak.update(cx, |w, cx| {
                    let id = id_for_toggle.clone();
                    let conv = w.conversation.clone();
                    conv.update(cx, |c, cx| {
                        if let Some((cix, eix)) = c.find_thinking_entry(&id, &*cx)
                            && let Some(item) = c.items().get(cix)
                        {
                            item.update(cx, |item, cx| {
                                if let ConvItem::Thinking(t) = item.kind_mut()
                                    && let Some(entry) = t.entries.get_mut(eix)
                                {
                                    entry.collapsed = !entry.collapsed;
                                    entry.user_toggled = true;
                                }
                                cx.notify();
                            });
                        }
                    });
                    cx.notify();
                });
            })
            .child(disclosure_icon(e.collapsed, theme))
            .child(gpui::div().text_color(theme.muted_foreground).child("⎿"))
            .child(Icon::new(status_icon).xsmall().text_color(status_color))
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child(truncate(&title, 80)),
            ),
    );

    let display_output = if e.streaming {
        live_tail(&e.output)
    } else {
        e.output.clone()
    };

    if show_output && !display_output.is_empty() {
        row = row.child(render_tool_output(
            &display_output,
            &e.name,
            e.streaming,
            eix,
            theme,
        ));
    }
    row.into_any_element()
}

/// Aggregate a segment's tool calls into a comma-joined summary like
/// "reading 2 files, running 1 shell command". File/search categories count
/// unique targets (paths / patterns) extracted from the structured tool
/// input, so editing the same file twice reports "edited 1 file"; `bash`
/// counts command invocations. Categories with zero calls are omitted.
/// Trailing "…" mirrors the Claude Code "still working" cadence.
fn thinking_summary(entries: &[ToolCallItem]) -> String {
    use std::collections::BTreeSet;
    let mut reads: BTreeSet<String> = BTreeSet::new();
    let mut writes: BTreeSet<String> = BTreeSet::new();
    let mut edits: BTreeSet<String> = BTreeSet::new();
    let mut searches: BTreeSet<String> = BTreeSet::new();
    let mut globs: BTreeSet<String> = BTreeSet::new();
    let mut lists: BTreeSet<String> = BTreeSet::new();
    let mut running = 0u32;
    let mut other = 0u32;
    for e in entries {
        match e.name.as_str() {
            "read_file" | "write_file" | "list_directory" => {
                if let Some(p) = e.input.get("path").and_then(|v| v.as_str()) {
                    match e.name.as_str() {
                        "read_file" => {
                            reads.insert(p.to_string());
                        }
                        "write_file" => {
                            writes.insert(p.to_string());
                        }
                        _ => {
                            lists.insert(p.to_string());
                        }
                    }
                } else {
                    match e.name.as_str() {
                        "read_file" => {
                            reads.insert(String::new());
                        }
                        "write_file" => {
                            writes.insert(String::new());
                        }
                        _ => {
                            lists.insert(String::new());
                        }
                    }
                }
            }
            "edit_file" => {
                // The patch's first `[PATH#TAG]` header names the target file.
                // Everything before the last `#` is the path (paths with `#`
                // survive). Mirrors `tool_title`'s edit_file extraction.
                let path = e
                    .input
                    .get("patch")
                    .and_then(|v| v.as_str())
                    .and_then(|patch| {
                        patch.lines().find_map(|l| {
                            let l = l.trim();
                            let inner = l.strip_prefix('[')?.strip_suffix(']')?;
                            Some(inner.rsplit_once('#')?.0.to_string())
                        })
                    })
                    .unwrap_or_default();
                edits.insert(path);
            }
            "bash" => {
                // Commands count by invocation, not by unique command text —
                // running `cargo build` twice is "2 commands", not 1.
                running += 1;
            }
            "grep" => {
                let p = e
                    .input
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                searches.insert(p);
            }
            "glob" => {
                let p = e
                    .input
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                globs.insert(p);
            }
            _ => {
                other += 1;
            }
        }
    }
    let mut parts: Vec<String> = Vec::new();
    let push = |count: u32, key: &str, parts: &mut Vec<String>| {
        if count > 0 {
            parts.push(i18n::t_count(key, count as i64).to_string());
        }
    };
    push(reads.len() as u32, "thinking-reading", &mut parts);
    push(writes.len() as u32, "thinking-writing", &mut parts);
    push(edits.len() as u32, "thinking-editing", &mut parts);
    push(searches.len() as u32, "thinking-searching", &mut parts);
    push(globs.len() as u32, "thinking-globbing", &mut parts);
    push(lists.len() as u32, "thinking-listing", &mut parts);
    push(running, "thinking-running", &mut parts);
    push(other, "thinking-other", &mut parts);
    if parts.is_empty() {
        String::new()
    } else {
        format!("{}…", parts.join(", "))
    }
}

fn render_ask_user_card(
    item: &ToolCallItem,
    ix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
) -> gpui::AnyElement {
    let Some(ctx) = tool_ctx else {
        return render_tool_call(item, ix, theme, tool_ctx);
    };
    let Some(snapshot) = ctx.ask.clone() else {
        return render_tool_call(item, ix, theme, tool_ctx);
    };
    if item.status != ToolCallStatus::PendingApproval {
        return render_tool_call(item, ix, theme, tool_ctx);
    }

    let weak = ctx.weak.clone();
    let step = snapshot.step;
    let total = snapshot.total;
    let can_prev = step > 0;
    let can_next = step + 1 < total;

    let header = h_flex()
        .gap_2()
        .items_center()
        .justify_between()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(Icon::new(IconName::Info).small().text_color(theme.primary))
                .child(
                    gpui::div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child(i18n::t("workspace-clarify-title")),
                ),
        )
        .child(
            gpui::div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(format!("{}/{total}", step + 1)),
        );

    let question_row = h_flex()
        .gap_2()
        .items_center()
        .child(
            Tag::new()
                .with_variant(TagVariant::Secondary)
                .small()
                .child(snapshot.question.header.clone()),
        )
        .child(
            gpui::div()
                .text_sm()
                .text_color(theme.foreground)
                .child(snapshot.question.question.clone()),
        );

    let mut options_block = v_flex().gap_1p5();
    for (oi, opt) in snapshot.question.options.iter().enumerate() {
        let selected = snapshot.selections.get(oi).copied().unwrap_or(false);
        let indicator_size = px(16.);
        let indicator = if snapshot.question.multi_select {
            if selected {
                h_flex()
                    .size(indicator_size)
                    .rounded(px(2.))
                    .border_1()
                    .border_color(theme.primary)
                    .bg(theme.primary.opacity(0.1))
                    .items_center()
                    .justify_center()
                    .child(
                        Icon::new(IconName::Check)
                            .xsmall()
                            .text_color(theme.primary),
                    )
            } else {
                h_flex()
                    .size(indicator_size)
                    .rounded(px(2.))
                    .border_1()
                    .border_color(theme.border)
            }
        } else if selected {
            h_flex()
                .size(indicator_size)
                .rounded_full()
                .border_1()
                .border_color(theme.primary)
                .items_center()
                .justify_center()
                .child(gpui::div().size(px(8.)).rounded_full().bg(theme.primary))
        } else {
            h_flex()
                .size(indicator_size)
                .rounded_full()
                .border_1()
                .border_color(theme.border)
        };
        let weak_for_option = weak.clone();
        let option_row = h_flex()
            .gap_2()
            .items_start()
            .id(gpui::SharedString::from(format!(
                "ask-card-opt-{ix}-{step}-{oi}"
            )))
            .cursor(CursorStyle::PointingHand)
            .on_click(move |_, _, cx: &mut App| {
                let _ = weak_for_option.update(cx, |w, cx| {
                    w.toggle_ask_option(step, oi, cx);
                });
            })
            .child(indicator)
            .child(
                h_flex()
                    .flex_1()
                    .gap_1()
                    .items_center()
                    .child(
                        gpui::div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(opt.label.clone()),
                    )
                    .child(
                        gpui::div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(opt.description.clone()),
                    ),
            );
        options_block = options_block.child(option_row);
    }

    let mut other_block = v_flex().gap_1();
    if let Some(state) = snapshot.other {
        other_block = other_block
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(i18n::t("workspace-clarify-other")),
            )
            .child(Input::new(&state));
    }

    let response_block = if let Some(state) = snapshot.response_input {
        v_flex()
            .gap_1()
            .child(gpui::div().h(px(1.)).w_full().bg(theme.border).mt_1())
            .child(Input::new(&state))
    } else {
        v_flex()
    };

    let weak_prev = weak.clone();
    let weak_next = weak.clone();
    let weak_cancel = weak.clone();
    let weak_submit = weak.clone();
    let nav = h_flex()
        .gap_2()
        .items_center()
        .justify_between()
        .child(
            h_flex()
                .gap_1()
                .child(
                    Button::new(("ask-card-prev", ix))
                        .ghost()
                        .small()
                        .icon(IconName::ChevronLeft)
                        .label(i18n::t("workspace-ask-prev"))
                        .when(!can_prev, |b| b.disabled(true))
                        .on_click(move |_, _, cx: &mut App| {
                            let _ = weak_prev.update(cx, |w, cx| w.ask_prev(cx));
                        }),
                )
                .child(
                    Button::new(("ask-card-next", ix))
                        .ghost()
                        .small()
                        .icon(IconName::ChevronRight)
                        .label(i18n::t("workspace-ask-next"))
                        .when(!can_next, |b| b.disabled(true))
                        .on_click(move |_, _, cx: &mut App| {
                            let _ = weak_next.update(cx, |w, cx| w.ask_next(cx));
                        }),
                ),
        )
        .child(
            h_flex()
                .gap_1()
                .child(
                    Button::new(("ask-card-cancel", ix))
                        .ghost()
                        .small()
                        .label(i18n::t("workspace-cancel"))
                        .on_click(move |_, _, cx: &mut App| {
                            let _ = weak_cancel.update(cx, |w, cx| {
                                w.resolve_auth(agent::PermissionDecision::Deny, cx);
                            });
                        }),
                )
                .child(
                    Button::new(("ask-card-submit", ix))
                        .primary()
                        .small()
                        .label(i18n::t("workspace-submit"))
                        .on_click(move |_, _, cx: &mut App| {
                            let _ = weak_submit.update(cx, |w, cx| w.resolve_ask(cx));
                        }),
                ),
        );

    v_flex()
        .id(format!(
            "ask-card-{}-{}",
            snapshot.id, snapshot.transition_gen
        ))
        .key_context("AskDrawer")
        .w_full()
        .gap_3()
        .p_3()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .child(header)
        .child(question_row)
        .child(options_block)
        .child(other_block)
        .child(response_block)
        .child(nav)
        .into_any_element()
}

/// Render a plain tool-call card: title + status icon + copy button + (collapsible)
/// monospace output. Used only as the answered-state fallback for an
/// `AskUserQuestion` whose interactive snapshot is gone (and the defensive
/// orphan in `render_item`'s ToolCall dispatch). Ordinary tool calls no longer
/// reach this path — they fold into a `Thinking` batch via `render_thinking`.
pub fn render_tool_call(
    item: &ToolCallItem,
    ix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
) -> gpui::AnyElement {
    use agent::ToolCallStatus;
    let (status_color, status_label): (gpui::Hsla, SharedString) = match item.status {
        ToolCallStatus::PendingApproval => (theme.muted_foreground, i18n::t("status-pending")),
        ToolCallStatus::Running => (theme.muted_foreground, i18n::t("status-running")),
        ToolCallStatus::Success => (theme.success, i18n::t("status-success")),
        ToolCallStatus::Continued => (theme.muted_foreground, i18n::t("status-continued")),
        ToolCallStatus::Error => (theme.danger, i18n::t("status-error")),
        ToolCallStatus::Denied => (theme.danger, i18n::t("status-denied")),
        ToolCallStatus::Cancelled => (theme.muted_foreground, i18n::t("status-cancelled")),
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

    // Tool-call chrome and output render as Lilex italic to set them apart
    // from upright body text (#140). This card is now only the AskUserQuestion
    // answered-state fallback + the defensive orphan; ordinary tools fold into
    // `render_activity_entry`, which carries the same italic.
    let mut card = v_flex()
        .group(format!("tool-{ix}"))
        .w_full()
        .italic()
        .child(
            h_flex()
                .id(("tool-header", ix))
                .w_full()
                .px_2()
                .py_1()
                .gap_1p5()
                .items_center()
                .rounded(theme.radius)
                .cursor_pointer()
                .hover(|s| s.bg(theme.secondary.opacity(0.5)))
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
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
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
/// `Markdown` (assistant-style, no code-block wrapping, no height cap) instead
/// of the monospace scrollable container. PendingApproval forces the body open;
/// terminal status auto-collapses like a regular ToolCall.
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
            ToolCallStatus::Continued => (
                IconName::Info,
                theme.muted_foreground,
                i18n::t("status-continued"),
            ),
            ToolCallStatus::Error => (IconName::CircleX, theme.danger, i18n::t("status-error")),
            ToolCallStatus::Denied => (IconName::CircleX, theme.danger, i18n::t("status-denied")),
            ToolCallStatus::Cancelled => (
                IconName::Minus,
                theme.muted_foreground,
                i18n::t("status-cancelled"),
            ),
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
    let pending_plan_id = tool_ctx.and_then(|c| c.pending_plan_id.clone());
    let show_actions = item.status == ToolCallStatus::PendingApproval
        && pending_plan_id.as_deref() == Some(&item.id);

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
                .child(markdown_tv(
                    ("plan-text", ix),
                    item.output.clone(),
                    theme,
                    false,
                )),
        );
    }

    if show_actions {
        let id_for_continue = item.id.clone();
        let id_for_approve = item.id.clone();
        let weak_continue = tool_ctx.map(|c| c.weak.clone());
        let weak_approve = tool_ctx.map(|c| c.weak.clone());
        card = card.child(
            h_flex()
                .gap_2()
                .justify_end()
                .px_3()
                .py_2()
                .border_t_1()
                .border_color(theme.border)
                .child(
                    Button::new(("plan-card-continue", ix))
                        .ghost()
                        .small()
                        .label(i18n::t("workspace-plan-continue"))
                        .on_click(move |_, _, cx: &mut App| {
                            let Some(weak) = weak_continue.clone() else {
                                return;
                            };
                            let id = id_for_continue.clone();
                            let _ = weak.update(cx, |w, cx| {
                                w.respond_plan_for_card(&id, false, cx);
                            });
                        }),
                )
                .child(
                    Button::new(("plan-card-approve", ix))
                        .primary()
                        .small()
                        .label(i18n::t("workspace-plan-approve"))
                        .on_click(move |_, _, cx: &mut App| {
                            let Some(weak) = weak_approve.clone() else {
                                return;
                            };
                            let id = id_for_approve.clone();
                            let _ = weak.update(cx, |w, cx| {
                                w.respond_plan_for_card(&id, true, cx);
                            });
                        }),
                ),
        );
    }

    card.into_any_element()
}

/// Fixed-height container with the tool's output. While streaming we paint a
/// plain monospace run (no markdown re-parse per chunk); once the final
/// `ToolResult` lands we mount the syntax-highlighted, scrollable `Markdown`.
/// The container keeps a deterministic height either way so the parent card
/// (and the list) reports a stable layout.
fn render_tool_output(
    output: &str,
    tool_name: &str,
    streaming: bool,
    ix: usize,
    theme: &Theme,
) -> gpui::AnyElement {
    let container = gpui::div()
        .id(("tool-output", ix))
        .h(px(220.))
        .overflow_hidden()
        .px_3()
        .py_2()
        .border_t_1()
        .border_color(theme.border)
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
        ToolCallStatus::Continued => (theme.muted_foreground, i18n::t("status-continued")),
        ToolCallStatus::Error => (theme.danger, i18n::t("status-error")),
        ToolCallStatus::Denied => (theme.danger, i18n::t("status-denied")),
        ToolCallStatus::Cancelled => (theme.muted_foreground, i18n::t("status-cancelled")),
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

    let mut card = v_flex()
        .group(format!("agent-{ix}"))
        .w_full()
        // A sub-agent task is tool-call kin, so its chrome and nested output
        // render as Lilex italic alongside tool-call cards.
        .italic()
        .child(
            h_flex()
                .id(("agent-header", ix))
                .w_full()
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
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
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
        // Expanded: rebuild the child conversation from its snapshot and render
        // each item recursively. Nested agent tasks share the same expansion
        // set (keyed by id), so they expand/collapse in place too.
        let sub_items = build_items(&item.sub_messages, &HashMap::new(), false);
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
                    .children(sub_items.iter().enumerate().map(|(six, sitem)| {
                        render_item(sitem, six, "agent", theme, agent_ctx, tool_ctx, None)
                    })),
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
        .child(markdown_tv(("agent-body-text", ix), code, theme, false))
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
    // `cut` is a byte offset; round it down to a UTF-8 char boundary so the
    // slices below stay valid when the tail split lands inside a multi-byte
    // glyph (e.g. CJK output). Without this, `output[cut..]` panics.
    let cut = output.floor_char_boundary(cut);
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
/// becomes its own item. Ordinary tool uses within one user turn aggregate into
/// a single activity segment (mirroring the live `apply` path, where
/// `StopReason::ToolUse` does not close the segment). A user prompt (text-
/// bearing user message) is the turn boundary; a user-role ToolResult is not.
pub fn build_items(
    messages: &[Message],
    usage: &HashMap<String, TokenUsage>,
    trailing_streaming: bool,
) -> Vec<ConvItem> {
    let mut items: Vec<ConvItem> = Vec::new();
    // Id of the most recent user message; usage is keyed by it, so an
    // assistant reply inherits the usage of the user message preceding it.
    let mut last_user_id: Option<&str> = None;
    // Index of the active activity segment for the current user turn. `None`
    // before the first ordinary tool call and after the turn closes. Ordinary
    // tool calls across the whole turn (multiple assistant messages, tool-
    // result user messages, flanking prose) fold into this one segment so a
    // reloaded thread reproduces the live "one summary per turn" behavior.
    let mut active_segment_ix: Option<usize> = None;

    /// Close the active segment for a turn boundary or a special tool that
    /// stays standalone. Freezes and auto-collapses it; clears the index.
    fn close_segment(items: &mut [ConvItem], seg_ix: Option<usize>) {
        if let Some(ix) = seg_ix
            && let Some(ConvItem::Thinking(t)) = items.get_mut(ix)
        {
            t.accepting_entries = false;
            t.streaming = false;
            t.collapsed = !t.user_toggled;
        }
    }

    for m in messages {
        match m.role {
            Role::User => {
                let has_prompt_text = m.content.iter().any(|c| match c {
                    MessageContent::Text(t) | MessageContent::Thinking { text: t, .. } => {
                        !t.is_empty()
                    }
                    _ => false,
                }) || m
                    .content
                    .iter()
                    .any(|c| matches!(c, MessageContent::Image { .. }));
                if has_prompt_text {
                    // A new user prompt closes the current turn's activity
                    // segment so the next turn opens a fresh one. A pure-
                    // tool-result user message has no prompt text and is NOT
                    // a turn boundary.
                    close_segment(&mut items, active_segment_ix);
                    active_segment_ix = None;
                }
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
                let images: Vec<UserImage> = m
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        MessageContent::Image { data, mime_type } => {
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(data.as_bytes())
                                .ok()?;
                            let fmt = gpui::ImageFormat::from_mime_type(mime_type.as_str())?;
                            Some(UserImage(Arc::new(gpui::Image::from_bytes(fmt, bytes))))
                        }
                        _ => None,
                    })
                    .collect();
                if !text.is_empty() || !images.is_empty() {
                    items.push(ConvItem::User {
                        text,
                        images,
                        meta: Some(crate::conversation::UserTurnMeta::from_message(m)),
                    });
                }
                for c in &m.content {
                    match c {
                        MessageContent::ToolResult(tr) => {
                            pair_tool_result(&mut items, tr);
                        }
                        MessageContent::Compaction(summary) => {
                            // A compaction message is role User but carries no
                            // prompt text — render it as a Recap card instead of
                            // an empty user bubble.
                            items.push(ConvItem::Recap {
                                summary: summary.clone(),
                                collapsed: true,
                                user_toggled: false,
                            });
                        }
                        _ => {}
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
                                // Sub-agent tasks stay as standalone top-level
                                // cards (their expand panel reuses the full
                                // sub-conversation renderer); never folded.
                                close_segment(&mut items, active_segment_ix);
                                active_segment_ix = None;
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
                            } else if tu.name.as_ref() == "AskUserQuestion" {
                                // An inline clarify card: stays a top-level
                                // ToolCall so `render_ask_user_card` can drive
                                // its interactive snapshot while pending; the
                                // paired ToolResult stamps the user's answer
                                // into `output`. Never folded into a segment.
                                close_segment(&mut items, active_segment_ix);
                                active_segment_ix = None;
                                items.push(ConvItem::ToolCall(ToolCallItem {
                                    id: tu.id.clone(),
                                    name: tu.name.to_string(),
                                    title: agent::thread::tool_title(tu.name.as_ref(), &tu.input),
                                    status: ToolCallStatus::PendingApproval,
                                    output: String::new(),
                                    is_error: false,
                                    input: tu.input.clone(),
                                    streaming: false,
                                    collapsed: false,
                                    user_toggled: false,
                                }));
                            } else if tu.name.as_ref() == "exit_plan_mode" {
                                // The plan body is the card's whole point; pull it
                                // from the tool input so a reloaded thread shows the
                                // plan even before (or without) a paired approval
                                // result. Expanded by default; a paired result only
                                // stamps the verdict status, never overwrites the
                                // plan body (see `pair_tool_result`).
                                close_segment(&mut items, active_segment_ix);
                                active_segment_ix = None;
                                let plan_text = tu
                                    .input
                                    .get("plan")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                items.push(ConvItem::ToolCall(ToolCallItem {
                                    id: tu.id.clone(),
                                    name: tu.name.to_string(),
                                    title: agent::thread::tool_title(tu.name.as_ref(), &tu.input),
                                    status: ToolCallStatus::Success,
                                    output: plan_text,
                                    is_error: false,
                                    input: tu.input.clone(),
                                    streaming: false,
                                    collapsed: false,
                                    user_toggled: false,
                                }));
                            } else {
                                // Ordinary tool call: fold into the active
                                // activity segment. The segment is created at
                                // the first ordinary tool call's position and
                                // stays there; subsequent tool calls (across
                                // assistant messages and tool-result user
                                // messages within the same turn) append to it.
                                let entry = ToolCallItem {
                                    id: tu.id.clone(),
                                    name: tu.name.to_string(),
                                    title: agent::thread::tool_title(tu.name.as_ref(), &tu.input),
                                    status: ToolCallStatus::Success,
                                    output: String::new(),
                                    is_error: false,
                                    input: tu.input.clone(),
                                    streaming: false,
                                    collapsed: true,
                                    user_toggled: false,
                                };
                                match active_segment_ix {
                                    Some(ix) => {
                                        if let Some(ConvItem::Thinking(t)) = items.get_mut(ix) {
                                            t.entries.push(entry);
                                        }
                                    }
                                    None => {
                                        active_segment_ix = Some(items.len());
                                        items.push(ConvItem::Thinking(ThinkingContainer {
                                            entries: vec![entry],
                                            accepting_entries: true,
                                            streaming: false,
                                            collapsed: true,
                                            user_toggled: false,
                                            started_at: Instant::now(),
                                            frozen_secs: None,
                                        }));
                                    }
                                }
                            }
                        }
                        MessageContent::ToolResult(tr) => {
                            // Defensive: tool results normally live in user messages,
                            // but pair them here too if they ever appear in an assistant turn.
                            pair_tool_result(&mut items, tr);
                        }
                        MessageContent::Image { .. } => {}
                        // Compaction messages are `Role::User` by construction;
                        // they cannot appear in an assistant turn. Reachable
                        // here only if a future caller mis-assigns the role.
                        MessageContent::Compaction(summary) => {
                            close_segment(&mut items, active_segment_ix);
                            active_segment_ix = None;
                            items.push(ConvItem::Recap {
                                summary: summary.clone(),
                                collapsed: true,
                                user_toggled: false,
                            });
                        }
                    }
                }
            }
            Role::System => {}
        }
    }
    // Close any still-open segment at the end of the message list.
    close_segment(&mut items, active_segment_ix);
    // The trailing in-flight assistant/reasoning content of a still-running
    // thread must read as live so resumed `AgentText`/`AgentThinking` deltas
    // append to it instead of spawning a second bubble.
    if trailing_streaming && let Some(last) = items.last_mut() {
        match last {
            ConvItem::Assistant { streaming, .. } | ConvItem::Reasoning { streaming, .. } => {
                *streaming = true;
            }
            ConvItem::Thinking(t) => {
                // A resumed turn may still be mid-segment: mark the
                // container live so later-arriving `ToolCall`/`ToolOutput`
                // deltas fold into it instead of opening a fresh one.
                t.accepting_entries = true;
                t.streaming = true;
            }
            _ => {}
        }
    }
    items
}

/// Attach a tool_result to its matching item by id. Sub-agent results land in
/// `AgentTaskItem::final_text`; `exit_plan_mode` results stamp only the verdict
/// on the plan card; ordinary tool results stamp the entry inside the owning
/// `ThinkingContainer`. A result with no matching ToolUse becomes a standalone
/// single-entry `ThinkingContainer` so an orphan result still renders as a `⎿`.
fn pair_tool_result(items: &mut Vec<ConvItem>, tr: &LanguageModelToolResult) {
    let status = if tr.is_error {
        ToolCallStatus::Error
    } else {
        ToolCallStatus::Success
    };
    // Locate the owning item: an AgentTask, the exit_plan_mode plan card, or a
    // Thinking-container entry. Remember the entry index for the Thinking path
    // so we can stamp the right `⎿` inside its batch.
    let mut thinking_eix: Option<usize> = None;
    let ix = items.iter().position(|i| match i {
        ConvItem::AgentTask(t) => t.id == tr.tool_use_id,
        ConvItem::ToolCall(t) => t.id == tr.tool_use_id,
        ConvItem::Thinking(t) => match t.entries.iter().position(|e| e.id == tr.tool_use_id) {
            Some(eix) => {
                thinking_eix = Some(eix);
                true
            }
            None => false,
        },
        _ => false,
    });
    let Some(ix) = ix else {
        items.push(ConvItem::Thinking(ThinkingContainer {
            entries: vec![ToolCallItem {
                id: tr.tool_use_id.clone(),
                name: tr.tool_name.to_string(),
                title: tr.tool_name.to_string(),
                status,
                output: tr.content.clone(),
                is_error: tr.is_error,
                input: serde_json::Value::Null,
                streaming: false,
                collapsed: !matches!(
                    status,
                    ToolCallStatus::Running | ToolCallStatus::PendingApproval
                ),
                user_toggled: false,
            }],
            accepting_entries: false,
            streaming: false,
            collapsed: false,
            user_toggled: false,
            started_at: Instant::now(),
            frozen_secs: None,
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
            // `exit_plan_mode`'s body is the plan (extracted from the tool
            // input at build time); the approval verdict lives in `status`,
            // so never clobber the plan with the canned approval/rejection
            // result text.
            if t.name != "exit_plan_mode" {
                t.output = tr.content.clone();
            }
            t.is_error = tr.is_error;
            t.status = status;
            if t.name.is_empty() {
                t.name = tr.tool_name.to_string();
            }
        }
        ConvItem::Thinking(t) => {
            if let Some(eix) = thinking_eix
                && let Some(entry) = t.entries.get_mut(eix)
            {
                entry.output = tr.content.clone();
                entry.is_error = tr.is_error;
                entry.status = status;
                entry.streaming = false;
                entry.collapsed = !entry.user_toggled;
            }
            t.recompute_streaming();
            if !t.streaming {
                t.collapsed = !t.user_toggled;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::language_model::{LanguageModelToolResult, LanguageModelToolUse};

    /// A reloaded `exit_plan_mode` ToolUse with no paired ToolResult (plan was
    /// pending approval at save time) still renders the plan body, expanded —
    /// the plan is extracted from the tool input, not left empty.
    #[test]
    fn build_items_extracts_exit_plan_mode_plan_from_input() {
        let plan = "## Goal\n\nUnify the banner UI.\n\n### Critical Files\n\n- message.rs\n";
        let messages = vec![Message::assistant(vec![MessageContent::ToolUse(
            LanguageModelToolUse {
                id: "tu_plan".to_string(),
                name: Arc::from("exit_plan_mode"),
                raw_input: String::new(),
                input: serde_json::json!({ "plan": plan }),
                is_input_complete: true,
                thought_signature: None,
            },
        )])];
        let items = build_items(&messages, &HashMap::new(), false);
        let tool = items
            .iter()
            .find_map(|i| match i {
                ConvItem::ToolCall(t) if t.name == "exit_plan_mode" => Some(t),
                _ => None,
            })
            .expect("exit_plan_mode card present");
        assert_eq!(tool.output, plan);
        assert!(!tool.collapsed);
    }

    /// Pairing an approval/rejection ToolResult onto an `exit_plan_mode` card
    /// stamps the verdict status but must not clobber the plan body with the
    /// canned verdict text — the plan is the card's content, the verdict is the
    /// icon.
    #[test]
    fn pair_tool_result_preserves_exit_plan_mode_plan_body() {
        let plan = "## Goal\n\nKeep this plan body.\n";
        let mut items: Vec<ConvItem> = vec![ConvItem::ToolCall(ToolCallItem {
            id: "tu_plan".to_string(),
            name: "exit_plan_mode".to_string(),
            title: "Submit plan".to_string(),
            status: ToolCallStatus::PendingApproval,
            output: plan.to_string(),
            is_error: false,
            input: serde_json::Value::Null,
            streaming: false,
            collapsed: false,
            user_toggled: false,
        })];
        pair_tool_result(
            &mut items,
            &LanguageModelToolResult {
                tool_use_id: "tu_plan".to_string(),
                tool_name: Arc::from("exit_plan_mode"),
                is_error: false,
                content: "User approved the plan. You may now begin execution.".to_string(),
            },
        );
        match &items[0] {
            ConvItem::ToolCall(t) => {
                assert_eq!(t.output, plan, "plan body must survive pairing");
                assert_eq!(t.status, ToolCallStatus::Success);
            }
            _ => panic!("item is still a ToolCall"),
        }
    }

    #[test]
    fn live_tail_short_output_unchanged() {
        let s = "line\nline2\n";
        assert_eq!(live_tail(s), s);
    }

    #[test]
    fn live_tail_long_ascii_splits_on_newline() {
        let s = "a".repeat(13 * 1024);
        let out = live_tail(&s);
        assert!(out.starts_with(&i18n::t("message-omitted-prefix").to_string()));
        // No newline in the input -> fall back to the byte cut; still valid.
        assert!(out.ends_with('a'));
    }

    /// Regression: a byte cut landing inside a multi-byte CJK glyph used to panic
    /// `output[cut..]` with a slice-out-of-bounds. The tail must split on a char
    /// boundary instead.
    #[test]
    fn live_tail_multibyte_cut_does_not_panic() {
        // Each line is a CJK char repeated so the tail boundary lands mid-glyph.
        let line = "中".repeat(64);
        let mut s = String::new();
        for _ in 0..(13 * 1024 / line.len() + 1) {
            s.push_str(&line);
            s.push('\n');
        }
        let out = live_tail(&s);
        assert!(out.starts_with(&i18n::t("message-omitted-prefix").to_string()));
        // The retained tail must be valid UTF-8 (would have panicked before).
        assert!(out.contains('中'));
    }

    /// Helper: build a `MessageContent::ToolUse` for a tool name + JSON input.
    fn tu(id: &str, name: &str, input: serde_json::Value) -> MessageContent {
        MessageContent::ToolUse(LanguageModelToolUse {
            id: id.to_string(),
            name: Arc::from(name),
            raw_input: String::new(),
            input,
            is_input_complete: true,
            thought_signature: None,
        })
    }

    /// Helper: build a `MessageContent::ToolResult` for a tool id + name.
    fn tr(id: &str, name: &str, content: &str) -> MessageContent {
        MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: id.to_string(),
            tool_name: Arc::from(name),
            is_error: false,
            content: content.to_string(),
        })
    }

    /// A multi-step user turn — read → edit → bash, each in its own assistant
    /// message with a tool-result user message between — must rebuild as ONE
    /// activity segment holding all three entries, not one segment per tool.
    /// This is the historical-rebuild mirror of the live `Stop(ToolUse)` does-
    /// not-freeze behavior.
    #[test]
    fn build_items_aggregates_tool_loop_into_one_segment() {
        let messages = vec![
            Message::user("do the task".to_string()),
            Message::assistant(vec![tu(
                "tu_1",
                "read_file",
                serde_json::json!({"path": "a.rs"}),
            )]),
            Message::user_with_content(vec![tr("tu_1", "read_file", "a contents")]),
            Message::assistant(vec![tu(
                "tu_2",
                "edit_file",
                serde_json::json!({"patch": "[a.rs#T1]\nINS x"}),
            )]),
            Message::user_with_content(vec![tr("tu_2", "edit_file", "ok")]),
            Message::assistant(vec![tu(
                "tu_3",
                "bash",
                serde_json::json!({"command": "cargo build"}),
            )]),
            Message::user_with_content(vec![tr("tu_3", "bash", "Built.")]),
        ];
        let items = build_items(&messages, &HashMap::new(), false);
        let segments: Vec<&ThinkingContainer> = items
            .iter()
            .filter_map(|i| match i {
                ConvItem::Thinking(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(segments.len(), 1, "one turn → one activity segment");
        assert_eq!(
            segments[0].entries.len(),
            3,
            "all three tools in the segment"
        );
        assert!(!segments[0].streaming, "historical segment is frozen");
        assert!(
            !segments[0].accepting_entries,
            "historical segment is closed"
        );
        // Entries in arrival order.
        assert_eq!(segments[0].entries[0].id, "tu_1");
        assert_eq!(segments[0].entries[1].id, "tu_2");
        assert_eq!(segments[0].entries[2].id, "tu_3");
    }

    /// A second user prompt starts a new turn and closes the previous segment.
    /// A tool-result-only user message (no prompt text) is NOT a turn boundary.
    #[test]
    fn build_items_user_prompt_is_turn_boundary_tool_result_is_not() {
        let messages = vec![
            Message::user("turn one".to_string()),
            Message::assistant(vec![tu(
                "tu_1",
                "read_file",
                serde_json::json!({"path": "a.rs"}),
            )]),
            // tool-result user message — NOT a turn boundary.
            Message::user_with_content(vec![tr("tu_1", "read_file", "a")]),
            Message::assistant(vec![tu(
                "tu_2",
                "bash",
                serde_json::json!({"command": "ls"}),
            )]),
            Message::user_with_content(vec![tr("tu_2", "bash", "files")]),
            // New user prompt — IS a turn boundary.
            Message::user("turn two".to_string()),
            Message::assistant(vec![tu(
                "tu_3",
                "read_file",
                serde_json::json!({"path": "b.rs"}),
            )]),
            Message::user_with_content(vec![tr("tu_3", "read_file", "b")]),
        ];
        let items = build_items(&messages, &HashMap::new(), false);
        let segments: Vec<&ThinkingContainer> = items
            .iter()
            .filter_map(|i| match i {
                ConvItem::Thinking(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(segments.len(), 2, "two turns → two segments");
        assert_eq!(segments[0].entries.len(), 2, "turn one has 2 tools");
        assert_eq!(segments[1].entries.len(), 1, "turn two has 1 tool");
    }

    /// `agent`, `exit_plan_mode`, and `AskUserQuestion` must stay standalone
    /// top-level cards — they must not be swallowed into the activity segment,
    /// even when they appear in the same assistant message as ordinary tools.
    #[test]
    fn build_items_keeps_special_tools_standalone() {
        let messages = vec![
            Message::user("go".to_string()),
            Message::assistant(vec![
                tu("tu_1", "read_file", serde_json::json!({"path": "a.rs"})),
                tu(
                    "tu_agent",
                    "agent",
                    serde_json::json!({"subagent_type": "r", "prompt": "p"}),
                ),
                tu(
                    "tu_plan",
                    "exit_plan_mode",
                    serde_json::json!({"plan": "do it"}),
                ),
                tu(
                    "tu_ask",
                    "AskUserQuestion",
                    serde_json::json!({"questions": [{"question": "q", "header": "h", "options": [{"text": "a", "value": "a"}], "multi_select": false}]}),
                ),
            ]),
            Message::user_with_content(vec![
                tr("tu_1", "read_file", "a"),
                tr("tu_agent", "agent", "{\"final\":\"done\"}"),
                tr("tu_plan", "exit_plan_mode", "approved"),
                tr("tu_ask", "AskUserQuestion", "answered"),
            ]),
        ];
        let items = build_items(&messages, &HashMap::new(), false);
        // The ordinary tool folds into a segment; the three special tools are
        // standalone top-level cards.
        let agent = items.iter().find_map(|i| match i {
            ConvItem::AgentTask(t) if t.id == "tu_agent" => Some(t),
            _ => None,
        });
        let plan = items.iter().find_map(|i| match i {
            ConvItem::ToolCall(t) if t.name == "exit_plan_mode" => Some(t),
            _ => None,
        });
        let ask = items.iter().find_map(|i| match i {
            ConvItem::ToolCall(t) if t.name == "AskUserQuestion" => Some(t),
            _ => None,
        });
        let seg = items.iter().find_map(|i| match i {
            ConvItem::Thinking(t) => Some(t),
            _ => None,
        });
        assert!(agent.is_some(), "agent task is standalone");
        assert!(plan.is_some(), "exit_plan_mode is standalone");
        assert!(ask.is_some(), "AskUserQuestion is standalone");
        let seg = seg.expect("ordinary tool folded into a segment");
        assert_eq!(
            seg.entries.len(),
            1,
            "only the ordinary tool is in the segment"
        );
        assert_eq!(seg.entries[0].id, "tu_1");
    }

    /// `thinking_summary` deduplicates file targets: editing the same file
    /// twice reports "edited 1 file", not 2. `bash` counts invocations.
    #[test]
    fn thinking_summary_deduplicates_file_targets() {
        let entries = vec![
            ToolCallItem {
                id: "1".into(),
                name: "edit_file".into(),
                title: String::new(),
                status: ToolCallStatus::Success,
                output: String::new(),
                is_error: false,
                input: serde_json::json!({"patch": "[src/a.rs#T1]\nINS x"}),
                streaming: false,
                collapsed: false,
                user_toggled: false,
            },
            ToolCallItem {
                id: "2".into(),
                name: "edit_file".into(),
                title: String::new(),
                status: ToolCallStatus::Success,
                output: String::new(),
                is_error: false,
                // Same path → deduped.
                input: serde_json::json!({"patch": "[src/a.rs#T2]\nINS y"}),
                streaming: false,
                collapsed: false,
                user_toggled: false,
            },
            ToolCallItem {
                id: "3".into(),
                name: "edit_file".into(),
                title: String::new(),
                status: ToolCallStatus::Success,
                output: String::new(),
                is_error: false,
                // Different path.
                input: serde_json::json!({"patch": "[src/b.rs#T1]\nINS z"}),
                streaming: false,
                collapsed: false,
                user_toggled: false,
            },
            ToolCallItem {
                id: "4".into(),
                name: "bash".into(),
                title: String::new(),
                status: ToolCallStatus::Success,
                output: String::new(),
                is_error: false,
                input: serde_json::json!({"command": "cargo build"}),
                streaming: false,
                collapsed: false,
                user_toggled: false,
            },
            ToolCallItem {
                id: "5".into(),
                name: "bash".into(),
                title: String::new(),
                status: ToolCallStatus::Success,
                output: String::new(),
                is_error: false,
                // Same command → still counted as 2 invocations.
                input: serde_json::json!({"command": "cargo build"}),
                streaming: false,
                collapsed: false,
                user_toggled: false,
            },
        ];
        let summary = thinking_summary(&entries);
        // 2 unique files edited, 2 commands run.
        assert!(
            summary.contains("2"),
            "summary should count 2 for files: {summary}"
        );
        // The summary is localized; just assert it's non-empty and ends with "…".
        assert!(
            summary.ends_with('…'),
            "summary ends with ellipsis: {summary}"
        );
    }

    /// A frozen + collapsed segment shows NO entries in `render_thinking`'s
    /// visible slice; a streaming + collapsed segment shows the running/latest
    /// entry only. This mirrors the collapsed visibility rules without
    /// requiring a gpui render context — we exercise the slice logic directly.
    #[test]
    fn render_thinking_collapsed_visibility_rules() {
        let mut t = ThinkingContainer::new();
        t.accepting_entries = false;
        t.streaming = false;
        t.collapsed = true;
        t.entries.push(ToolCallItem {
            id: "1".into(),
            name: "read_file".into(),
            title: String::new(),
            status: ToolCallStatus::Success,
            output: String::new(),
            is_error: false,
            input: serde_json::Value::Null,
            streaming: false,
            collapsed: true,
            user_toggled: false,
        });
        t.entries.push(ToolCallItem {
            id: "2".into(),
            name: "bash".into(),
            title: String::new(),
            status: ToolCallStatus::Success,
            output: String::new(),
            is_error: false,
            input: serde_json::Value::Null,
            streaming: false,
            collapsed: true,
            user_toggled: false,
        });
        // Frozen + collapsed: no entries visible.
        let visible: Vec<&ToolCallItem> = if !t.collapsed {
            t.entries.iter().collect()
        } else if t.streaming {
            t.entries
                .iter()
                .rev()
                .find(|e| {
                    e.streaming
                        || matches!(
                            e.status,
                            ToolCallStatus::Running | ToolCallStatus::PendingApproval
                        )
                })
                .or(t.entries.last())
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        assert!(visible.is_empty(), "frozen + collapsed shows no entries");

        // Streaming + collapsed: the running entry (or latest) is visible.
        t.streaming = true;
        t.entries[0].streaming = true;
        t.entries[0].status = ToolCallStatus::Running;
        let visible: Vec<&ToolCallItem> = if !t.collapsed {
            t.entries.iter().collect()
        } else if t.streaming {
            t.entries
                .iter()
                .rev()
                .find(|e| {
                    e.streaming
                        || matches!(
                            e.status,
                            ToolCallStatus::Running | ToolCallStatus::PendingApproval
                        )
                })
                .or(t.entries.last())
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        assert_eq!(visible.len(), 1, "streaming + collapsed shows 1 entry");
        assert_eq!(visible[0].id, "1", "shows the running entry");

        // Expanded: all entries visible.
        t.collapsed = false;
        let visible: Vec<&ToolCallItem> = if !t.collapsed {
            t.entries.iter().collect()
        } else if t.streaming {
            t.entries
                .iter()
                .rev()
                .find(|e| {
                    e.streaming
                        || matches!(
                            e.status,
                            ToolCallStatus::Running | ToolCallStatus::PendingApproval
                        )
                })
                .or(t.entries.last())
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        assert_eq!(visible.len(), 2, "expanded shows all entries");
    }
}
