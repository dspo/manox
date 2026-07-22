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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::conversation::{
    ActivityEntry, AgentTaskItem, BackgroundTaskItem, ConvItem, ThinkingContainer, ToolCallItem,
    UserImage, UserTurnMeta,
};
use agent::language_model::{LanguageModelToolResult, MessageContent, Role};
use agent::thread::ApprovalMode;
use agent::{Message, TokenUsage, ToolCallStatus, i18n};
use base64::Engine as _;
use chrono::{Datelike as _, Local, TimeZone as _};
use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt as _, App, ClipboardItem, CursorStyle, Entity, Render, SharedString,
    WeakEntity, ease_out_quint, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex,
    spinner::Spinner,
    tag::{Tag, TagVariant},
    tooltip::Tooltip,
    v_flex,
};
use manox_components::markdown::terminal_panel::GitSummary;
use manox_components::markdown::{HeadingMode, Markdown, PanelKind, TerminalPanel};
use manox_components::turn_frame::TurnFrame;
use std::path::{Path, PathBuf};

use crate::Workspace;
use crate::views::centered;
use crate::workspace::AskCardSnapshot;

/// Render-time context for sub-agent task rows. `None` when the owning
/// workspace has been dropped; the row remains visible but clicks become a
/// no-op.
#[derive(Clone)]
pub struct AgentTaskCtx {
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
///
/// Mounts a fresh `Entity<Markdown>` per frame — used for static chrome
/// (notices, errors, tool output, sub-message bodies) where cross-block
/// selection is not required (code blocks carry their own hover copy button).
/// The owned streaming body uses the `MessageItem`'s persistent
/// `Entity<Markdown>` directly (see `render_assistant`).
fn markdown_tv(
    id: impl Into<gpui::ElementId>,
    text: impl Into<gpui::SharedString>,
    theme: &Theme,
    scrollable: bool,
    cx: &mut App,
) -> gpui::AnyElement {
    cx.new(|_cx| {
        Markdown::new(id, text)
            .theme(theme)
            .scrollable(scrollable)
            .heading_mode(HeadingMode::Uniform)
    })
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
/// `markdown` holds the owned `Entity<Markdown>` for text-bearing items
/// (Assistant, Reasoning): a stateful document carrying parse-once incremental
/// parsing + document-level selection, so a streaming delta re-parses only the
/// tail and a cross-block drag selects one continuous range with Cmd/Ctrl+C
/// copy. `None` for non-text items (ToolCall, Error, …) — those render static
/// chrome via `markdown_tv`, which mounts a fresh `Entity<Markdown>` per frame
/// (no persistent selection; code blocks carry their own hover copy button).
pub struct MessageItem {
    kind: ConvItem,
    role: String,
    id: usize,
    /// Weak handle to the owning `Workspace`, used by interactive message
    /// rows such as `AgentTask` to open their peer right-pane view.
    weak_workspace: WeakEntity<Workspace>,
    markdown: Option<Entity<Markdown>>,
}

impl MessageItem {
    pub fn new(kind: ConvItem, role: String, id: usize, weak: WeakEntity<Workspace>) -> Self {
        Self {
            kind,
            role,
            id,
            weak_workspace: weak,
            markdown: None,
        }
    }

    pub fn kind(&self) -> &ConvItem {
        &self.kind
    }

    pub fn kind_mut(&mut self) -> &mut ConvItem {
        &mut self.kind
    }

    /// Lazily create the owned `Entity<Markdown>` for a text-bearing item,
    /// seeded with the kind's streaming flag (true mid-stream, false for
    /// finalized/historical bodies). Returns the entity handle so the caller can
    /// mount it. `None` for non-text items (they have no owned markdown body).
    fn ensure_markdown(&mut self, cx: &mut gpui::Context<Self>) -> Option<Entity<Markdown>> {
        let is_text = matches!(self.kind, ConvItem::Assistant { .. });
        if is_text && self.markdown.is_none() {
            let streaming = match &self.kind {
                ConvItem::Assistant { streaming, .. } => *streaming,
                _ => false,
            };
            let id = self.id;
            self.markdown = Some(cx.new(|cx| {
                Markdown::new(("md", id), "")
                    .theme(cx.theme())
                    .heading_mode(HeadingMode::Uniform)
                    .streaming(streaming)
            }));
        }
        self.markdown.clone()
    }

    /// Feed a full text snapshot to the owned markdown document. `replace` runs
    /// the incremental parser's append-only fast path (re-parse only the tail),
    /// so a streaming delta pays proportional to the delta, not the full body.
    pub fn update_text(&mut self, full_text: &str, cx: &mut gpui::Context<Self>) {
        if let Some(md) = self.ensure_markdown(cx) {
            md.update(cx, |m, cx| m.replace(full_text, cx));
        }
    }

    /// Run the parser's final full parse so the frozen prefix + tail match a
    /// one-shot parse. Used by `rebuild_from_messages` for non-streaming text
    /// items loaded from history.
    pub fn finalize_parser(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(md) = &self.markdown {
            md.update(cx, |m, cx| m.finalize(cx));
        }
    }

    /// Ensure the `eix`-th activity entry's persistent `Entity<Markdown>` exists
    /// and feed it the entry's current text. Mirrors `update_text` for the
    /// activity-tree reasoning rounds: a streaming delta drives the incremental
    /// parser's append-only fast path while document-level selection + focus
    /// survive across frames (drag + Cmd/Ctrl+C), so a reasoning round selects
    /// and copies just like the top-level body.
    pub fn sync_reasoning_entry(&mut self, eix: usize, cx: &mut gpui::Context<Self>) {
        let ConvItem::Thinking(t) = &mut self.kind else {
            return;
        };
        let Some(ActivityEntry::Reasoning {
            text,
            streaming,
            markdown,
            ..
        }) = t.entries.get_mut(eix)
        else {
            return;
        };
        if markdown.is_none() {
            let streaming = *streaming;
            let id = eix;
            *markdown = Some(cx.new(|cx| {
                Markdown::new(("reasoning-md", id), "")
                    .theme(cx.theme())
                    .heading_mode(HeadingMode::Uniform)
                    .streaming(streaming)
            }));
        }
        if let Some(md) = markdown.as_ref().cloned() {
            let text = text.clone();
            md.update(cx, |m, cx| m.replace(&text, cx));
        }
    }

    /// For a rebuilt (historical) `Thinking` container, mount + finalize the
    /// persistent markdown for every reasoning round so document-level
    /// selection works on reloaded history (not just live-streamed turns).
    /// Mirrors `update_text` + `finalize_parser` for the top-level text bodies.
    pub fn rebuild_activity_reasoning(&mut self, cx: &mut gpui::Context<Self>) {
        let eixs: Vec<usize> = {
            let ConvItem::Thinking(t) = &mut self.kind else {
                return;
            };
            t.entries
                .iter()
                .enumerate()
                .filter(|(_, e)| matches!(e, ActivityEntry::Reasoning { .. }))
                .map(|(i, _)| i)
                .collect()
        };
        for eix in eixs {
            self.sync_reasoning_entry(eix, cx);
            if let ConvItem::Thinking(t) = &mut self.kind
                && let Some(ActivityEntry::Reasoning { markdown, .. }) = t.entries.get_mut(eix)
                && let Some(md) = markdown.as_ref()
            {
                md.update(cx, |m, cx| m.finalize(cx));
            }
        }
    }

    /// Ensure the `eix`-th activity entry's persistent `Entity<TerminalPanel>`
    /// exists and feed it the entry's current display output. Mirrors
    /// `sync_reasoning_entry` for tool calls: a streaming delta or a finalized
    /// result drives the panel's `set_output` while document-level selection +
    /// focus survive across frames (drag + Cmd/Ctrl+C), so tool output selects
    /// and copies like the assistant body — and renders as a terminal-styled
    /// shell (cwd / toolchain / `❯` command + ANSI-colored output) rather than
    /// a fenced code block.
    pub fn sync_tool_entry_panel(
        &mut self,
        eix: usize,
        cwd: Option<SharedString>,
        cx: &mut gpui::Context<Self>,
    ) {
        let ConvItem::Thinking(t) = &mut self.kind else {
            return;
        };
        let Some(ActivityEntry::Tool(entry)) = t.entries.get_mut(eix) else {
            return;
        };
        Self::ensure_tool_panel(entry, cwd, cx);
    }

    /// Top-level `ConvItem::ToolCall` variant of the panel sync, used by the
    /// `AskUserQuestion` answered-state card and the orphan `ToolResult` card.
    pub fn sync_tool_call_panel(
        &mut self,
        cwd: Option<SharedString>,
        cx: &mut gpui::Context<Self>,
    ) {
        let ConvItem::ToolCall(entry) = &mut self.kind else {
            return;
        };
        Self::ensure_tool_panel(entry, cwd, cx);
    }

    /// Mount (lazily) and refresh the tool-call's `TerminalPanel` from its
    /// current display output. `cwd` is the live thread working directory
    /// gathered by the caller from the workspace. Only `bash` earns a prompt
    /// block (cwd + git + `❯ command`); see the gate below — internal tools
    /// and MCP tools render the body only, so `command`/`cwd` stay `None`
    /// for them even though some carry a `command` input field.
    fn ensure_tool_panel(
        entry: &mut ToolCallItem,
        cwd: Option<SharedString>,
        cx: &mut gpui::Context<MessageItem>,
    ) {
        let (kind, body) = tool_panel_body(entry);
        if entry.panel.is_none() {
            // Only the `bash` tool runs a real shell command a human would type in
            // a terminal, so only it earns the prompt block (cwd + git + `❯
            // command`). Internal tools (grep / read_file / edit_file / glob /
            // list_directory / …) and MCP tools are manox abstractions, not
            // terminal commands — they render the body only, without the cwd
            // preamble that would imply "run this in a shell".
            let is_terminal_command = entry.name.as_str() == agent::tools::BASH;
            let command = if is_terminal_command {
                entry
                    .input
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(SharedString::from)
            } else {
                None
            };
            let cwd_for_panel = if is_terminal_command {
                cwd.clone()
            } else {
                None
            };
            let panel = cx.new(|cx| TerminalPanel::new(kind, command, cwd_for_panel, cx.theme()));
            // Probe the workdir's git state once per panel — a snapshot at the
            // moment the command ran, like a real shell prompt. Runs on the
            // background executor so the two `git` subprocess spawns never block
            // the UI; `set_git` re-renders the prompt line when it lands.
            if is_terminal_command && let Some(cwd_s) = cwd.as_ref() {
                let cwd_path = PathBuf::from(cwd_s.as_ref());
                let panel = panel.clone();
                cx.spawn(async move |_, cx| {
                    let git = cx
                        .background_spawn(async move { detect_git(&cwd_path) })
                        .await;
                    panel.update(cx, |p, cx| p.set_git(git, cx));
                })
                .detach();
            }
            entry.panel = Some(panel);
        }
        if let Some(panel) = entry.panel.as_ref().cloned() {
            let streaming = entry.streaming;
            panel.update(cx, |p, cx| {
                p.set_kind(kind, cx);
                p.set_streaming(streaming, cx);
                p.set_output(body, cx);
            });
        }
    }

    /// On history reload, mount + refresh the persistent panel for every tool
    /// entry across all activity segments (and each top-level `ToolCall`),
    /// so selection works on reloaded history and finalized output renders as a
    /// terminal panel rather than a per-frame fallback.
    pub fn rebuild_tool_panels(&mut self, cwd: Option<SharedString>, cx: &mut gpui::Context<Self>) {
        // Gather entry indices through an immutable borrow first, then drive the
        // mutable `sync_tool_*_panel` calls — otherwise `&mut self.kind` and
        // `&mut self` (via the sync method) collide.
        let tool_eixs: Vec<usize> = match &self.kind {
            ConvItem::Thinking(t) => t
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| matches!(e, ActivityEntry::Tool(_)))
                .map(|(i, _)| i)
                .collect(),
            ConvItem::ToolCall(_) => return self.sync_tool_call_panel(cwd, cx),
            _ => return,
        };
        for eix in tool_eixs {
            self.sync_tool_entry_panel(eix, cwd.clone(), cx);
        }
    }

    /// Flip streaming flags off on a `Stop`. Called once per stop, so the
    /// O(items) walk is harmless. `terminal` distinguishes a turn-ending stop
    /// (`EndTurn`/`MaxTokens`/`Refusal`/cancel/error) from a mid-turn
    /// `StopReason::ToolUse`: a terminal stop freezes the activity segment
    /// (pins elapsed, auto-collapses) and the tool-call cards; a ToolUse stop
    /// only finalizes the assistant/reasoning text streaming so the next model
    /// response's tool calls fold into the same segment. The markdown document
    /// always gets a final pass so the frozen prefix + tail match a one-shot
    /// full parse exactly.
    pub fn finalize_streaming(&mut self, terminal: bool, cx: &mut gpui::Context<Self>) {
        match &mut self.kind {
            ConvItem::Assistant { streaming, .. } => *streaming = false,
            ConvItem::Thinking(t) if terminal => {
                // Turn ended: freeze the segment, pin elapsed, auto-collapse
                // entries the user didn't pin. `finalize_segment` is
                // idempotent with `recompute_streaming`'s pinning.
                t.finalize_segment();
                for entry in &mut t.entries {
                    match entry {
                        ActivityEntry::Reasoning {
                            streaming,
                            collapsed,
                            user_toggled,
                            markdown,
                            ..
                        } => {
                            *streaming = false;
                            *collapsed = !*user_toggled;
                            if let Some(md) = markdown.as_ref() {
                                md.update(cx, |m, cx| m.finalize_streaming(cx));
                            }
                        }
                        ActivityEntry::Tool(tool) => {
                            tool.streaming = false;
                            if matches!(
                                tool.status,
                                ToolCallStatus::Success
                                    | ToolCallStatus::Error
                                    | ToolCallStatus::Denied
                            ) {
                                tool.collapsed = !tool.user_toggled;
                            }
                        }
                    }
                }
                t.collapsed = !t.user_toggled;
            }
            // ToolUse stop (`!terminal`): the segment stays open so the next
            // model response's tool calls still fold into it, but this
            // reasoning round is done — flip its `streaming` flag off (and
            // finalize its markdown) so the next `AgentThinking` opens a
            // fresh round instead of appending to the previous one.
            ConvItem::Thinking(t) => {
                t.finalize_reasoning_rounds();
                for entry in &mut t.entries {
                    if let ActivityEntry::Reasoning { markdown, .. } = entry
                        && let Some(md) = markdown.as_ref()
                    {
                        md.update(cx, |m, cx| m.finalize_streaming(cx));
                    }
                }
            }
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
            ConvItem::AgentTask(_) => {}
            _ => {}
        }
        if let Some(md) = &self.markdown {
            md.update(cx, |m, cx| m.finalize_streaming(cx));
        }
    }
    /// Close the activity segment when assistant text arrives mid-turn —
    /// mirrors `build_items`'s `close_segment` on `MessageContent::Text` so
    /// the live streaming path matches the historical rebuild path. Without
    /// this, `AgentThinking` arriving after the answer text folds into the
    /// pre-answer segment (temporal inversion, issue #216).
    pub fn close_segment_for_text(&mut self, cx: &mut gpui::Context<Self>) {
        let ConvItem::Thinking(t) = &mut self.kind else {
            return;
        };
        t.close_for_text();
        // Finalize reasoning markdown so the streaming cursor stops and the
        // final parse matches a one-shot parse.
        for entry in &mut t.entries {
            if let ActivityEntry::Reasoning { markdown, .. } = entry
                && let Some(md) = markdown.as_ref()
            {
                md.update(cx, |m, cx| m.finalize_streaming(cx));
            }
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
        let agent_ctx = self.weak_workspace.upgrade().map(|ws| AgentTaskCtx {
            weak: ws.downgrade(),
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
            }
        });
        // The owned markdown document for text-bearing items (persistent across
        // frames → selection + streaming state survive). `None` for non-text
        // items; their chrome mounts a fresh `Entity<Markdown>` per frame.
        let body = self.ensure_markdown(cx);
        centered(render_item(
            &self.kind,
            self.id,
            &self.role,
            &theme,
            agent_ctx.as_ref(),
            tool_ctx.as_ref(),
            body,
            cx,
        ))
    }
}

/// Render a `ConvItem` as an element. `ix` is the entry index (stable key for
/// collapsibles and text-block element ids). `agent_ctx` supplies expansion
/// state for `AgentTask` cards; `tool_ctx` carries the workspace weak handle
/// for `ToolCall` cards to flip their own collapse flag. `None` renders them
/// in a static state with no-op clicks (used when the owning Workspace is gone).
///
/// `body` is the owned `Entity<Markdown>` for a top-level text body
/// (Assistant / Reasoning); the recursive `render_item` calls for embedded
/// sub-messages pass `None`, falling back to a per-frame `Entity<Markdown>`.
//
// Each arg is a distinct render input; the function is a leaf dispatch, not a
// public API. Bundling would only forward the same values through an
// intermediate struct without reducing complexity.
#[allow(clippy::too_many_arguments)]
pub fn render_item(
    item: &ConvItem,
    ix: usize,
    role: &str,
    theme: &Theme,
    agent_ctx: Option<&AgentTaskCtx>,
    tool_ctx: Option<&ToolCallCtx>,
    body: Option<Entity<Markdown>>,
    cx: &mut App,
) -> gpui::AnyElement {
    match item {
        ConvItem::User {
            text,
            images,
            meta,
            display_state,
        } => match display_state {
            crate::conversation::UserMessageDisplayState::RolledBackSteer { .. } => {
                gpui::div().hidden().into_any_element()
            }
            display_state => render_user(
                UserRenderContent {
                    text,
                    images,
                    meta: meta.as_ref(),
                    pending_steer: matches!(
                        display_state,
                        crate::conversation::UserMessageDisplayState::PendingSteer { .. }
                    ),
                },
                ix,
                role,
                theme,
                cx,
            ),
        },
        ConvItem::Assistant {
            text,
            streaming: _,
            token_usage: _,
            activity_summary,
        } => render_assistant(text, ix, role, activity_summary.as_ref(), theme, body, cx),
        ConvItem::Thinking(t) => render_thinking(t, ix, theme, tool_ctx, cx),
        ConvItem::ToolCall(t) => {
            if t.name == agent::tools::ASK_USER_QUESTION {
                render_ask_user_card(t, ix, theme, tool_ctx, cx)
            } else {
                // Ordinary tool calls fold into `Thinking`; a top-level
                // ToolCall here is the answered-state fallback for an
                // `AskUserQuestion` whose interactive snapshot is gone, or a
                // defensive orphan — render it as a plain card.
                render_tool_call(t, ix, theme, tool_ctx, cx)
            }
        }
        ConvItem::AgentTask(t) => render_agent_task(t, ix, theme, agent_ctx, tool_ctx, cx),
        ConvItem::Error(msg) => render_error(msg, ix, theme, cx),
        ConvItem::Notice(msg) => render_notice(msg, ix, theme, cx),
        ConvItem::TeamMessage { from, content } => {
            render_team_message(from, content, ix, theme, cx)
        }
        ConvItem::PlanReview { plan_text, active } => {
            render_plan_review_card(plan_text, *active, ix, theme, tool_ctx, cx)
        }
        ConvItem::Recap {
            summary,
            collapsed,
            user_toggled: _,
        } => render_recap(summary, *collapsed, ix, theme, tool_ctx, cx),
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
            cx,
        ),
        ConvItem::BackgroundTask(bt) => render_background_task(bt, ix, theme, tool_ctx, cx),
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
struct UserRenderContent<'a> {
    text: &'a str,
    images: &'a [UserImage],
    meta: Option<&'a UserTurnMeta>,
    pending_steer: bool,
}

fn render_user(
    content: UserRenderContent<'_>,
    ix: usize,
    model: &str,
    theme: &Theme,
    cx: &mut App,
) -> gpui::AnyElement {
    let UserRenderContent {
        text,
        images,
        meta,
        pending_steer,
    } = content;
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

    // A persistent "steered" marker for user messages that entered the list
    // via the steer-queue drain (mid-turn injection) rather than starting a
    // fresh turn. Survives reload because it is read back from
    // `MessageUiMetadata::steered` in `from_message`.
    let steer_badge = if pending_steer {
        Some(
            gpui::div()
                .px_1()
                .py_0p5()
                .rounded(theme.radius)
                .bg(accent.opacity(0.15))
                .text_xs()
                .text_color(accent)
                .child(i18n::t("message-steer-pending-badge")),
        )
    } else {
        meta.filter(|m| m.steered).map(|_| {
            gpui::div()
                .px_1()
                .py_0p5()
                .rounded(theme.radius)
                .bg(accent.opacity(0.15))
                .text_xs()
                .text_color(accent)
                .child(i18n::t("message-steered-badge"))
        })
    };

    let mut header_el = h_flex()
        .items_center()
        .gap_1()
        .child(SharedString::from(header));
    if let Some(badge) = steer_badge {
        header_el = header_el.child(badge);
    }

    TurnFrame::new(theme)
        .group(group.clone())
        .accent(accent)
        .header(
            gpui::div()
                .text_color(theme.muted_foreground)
                .child(header_el),
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
                    cx,
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
    ix: usize,
    role: &str,
    activity_summary: Option<&crate::conversation::ActivitySummary>,
    theme: &Theme,
    body: Option<Entity<Markdown>>,
    cx: &mut App,
) -> gpui::AnyElement {
    // Owned `Entity<Markdown>` (persistent → selection + streaming survive);
    // fall back to a per-frame mount for embedded sub-message bodies.
    let body_el = match body {
        Some(md) => md.into_any_element(),
        None => markdown_tv(("assistant", ix), text.to_string(), theme, false, cx),
    };
    // Suffix for the model row summarizing the activity segment that preceded
    // this reply: " · 思考了 N 轮次 · 调用了 M 次工具 · Ss". `None` (no
    // preceding thinking/tool activity, or all counts zero) renders a bare
    // row with just the model name.
    let summary_child = activity_summary.and_then(activity_summary_text).map(|t| {
        gpui::div()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(t)
    });
    v_flex()
        .group(format!("assistant-{ix}"))
        .w_full()
        .min_w_0()
        .gap_1()
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .gap_1()
                .items_center()
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(role.to_string()),
                )
                .children(summary_child)
                .child(gpui::div().flex_1())
                .child(copy_button_hoverable(
                    ix,
                    "copy-assistant",
                    format!("assistant-{ix}"),
                    text.to_string(),
                )),
        )
        .child(
            gpui::div()
                .w_full()
                .min_w_0()
                .overflow_x_hidden()
                .child(body_el),
        )
        .into_any_element()
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
    let mut left = h_flex()
        .flex_1()
        .min_w_0()
        .overflow_x_hidden()
        .items_center()
        .gap_1()
        .text_xs()
        .text_color(accent);
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
        .min_w_0()
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
        .min_w_0()
        .gap_1()
        .px_3()
        .py_2()
        .rounded(theme.radius)
        .bg(accent.opacity(0.10))
        .child(label_row);
    if show_body {
        card = card.child(
            gpui::div()
                .w_full()
                .min_w_0()
                .text_sm()
                .text_color(theme.foreground)
                .child(body),
        );
    }
    card.into_any_element()
}

/// Render an error message + copy button.
pub fn render_error(msg: &str, ix: usize, theme: &Theme, cx: &mut App) -> gpui::AnyElement {
    render_banner(
        theme.danger,
        i18n::t("message-error"),
        None,
        format!("error-{ix}"),
        ix,
        "copy-error",
        msg.to_string(),
        markdown_tv(("error", ix), msg.to_string(), theme, false, cx),
        theme,
        None,
    )
}

/// Render an ephemeral system notice — status toggles, slash-command acks.
/// Neutral tones so positive state changes (e.g. "YOLO mode is on") do not
/// read as a runtime error.
pub fn render_notice(msg: &str, ix: usize, theme: &Theme, cx: &mut App) -> gpui::AnyElement {
    render_banner(
        theme.muted_foreground,
        i18n::t("message-notice"),
        None,
        format!("notice-{ix}"),
        ix,
        "copy-notice",
        msg.to_string(),
        markdown_tv(("notice", ix), msg.to_string(), theme, false, cx),
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
    cx: &mut App,
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
        markdown_tv(("team-msg", ix), content.to_string(), theme, false, cx),
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
    cx: &mut App,
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
        markdown_tv(("recap", ix), summary.to_string(), theme, false, cx),
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
    cx: &mut App,
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
        .map(|d| markdown_tv(("retry", ix), d.to_string(), theme, false, cx))
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
        x if x == agent::tools::BASH => Some("bash"),
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

/// Render an activity segment as a flat stack of its entries — each a
/// self-collapsible reasoning round or tool node, sitting at the same level
/// as the surrounding assistant text (no segment-level header, no left rail,
/// no branch connectors). Entries default to collapsed so the live stream
/// does not auto-play their content; a manual expand reveals the body. The
/// segment's aggregated totals ("思考了 N 轮次 · 调用了 M 次工具 · Ss") are
/// rendered on the following reply's model row via `activity_summary`.
pub fn render_thinking(
    t: &ThinkingContainer,
    ix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
    cx: &mut App,
) -> gpui::AnyElement {
    if t.entries.is_empty() {
        // A freshly-created container before any entry has arrived renders
        // nothing — the first reasoning delta or tool call lands next frame.
        return gpui::div().into_any_element();
    }
    // No segment-level header or rail-and-branch tree: each entry is a self-
    // collapsible row at the same level as the surrounding assistant text,
    // defaulting to collapsed so the live stream does not auto-play. The
    // segment's totals are summarized on the following reply's model row.
    v_flex()
        .w_full()
        .min_w_0()
        .gap_0p5()
        .debug_selector(|| format!("message-overflow-activity-tree-{ix}"))
        .children(
            t.entries
                .iter()
                .enumerate()
                .map(|(eix, e)| render_activity_entry(e, eix, ix, theme, tool_ctx, cx)),
        )
        .into_any_element()
}

/// Render one activity entry (a reasoning round or a tool node) as a flat,
/// self-collapsible row. No branch connector or left rail — entries sit at
/// the same indentation level as the surrounding assistant text.
fn render_activity_entry(
    e: &ActivityEntry,
    eix: usize,
    cix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
    cx: &mut App,
) -> gpui::AnyElement {
    let entry = match e {
        ActivityEntry::Reasoning {
            text,
            streaming,
            collapsed,
            user_toggled,
            markdown,
        } => render_reasoning_entry(
            text,
            *streaming,
            *collapsed,
            *user_toggled,
            markdown.clone(),
            eix,
            cix,
            theme,
            tool_ctx,
            cx,
        ),
        ActivityEntry::Tool(tool) => render_tool_entry(tool, eix, theme, tool_ctx, cx),
    };
    // No branch/rail wrapper — a plain full-width overflow guard so the entry
    // body stays within the message column on narrow widths. The debug
    // selectors back the overflow test (which now covers the flat layout).
    gpui::div()
        .w_full()
        .min_w_0()
        .overflow_x_hidden()
        .debug_selector(|| format!("message-overflow-activity-entry-{cix}-{eix}"))
        .child(
            gpui::div()
                .min_w_0()
                .overflow_x_hidden()
                .debug_selector(|| format!("message-overflow-activity-entry-body-{cix}-{eix}"))
                .child(entry),
        )
        .into_any_element()
}

/// Render a reasoning round entry in the activity tree: a lightweight node
/// labeled "思考" that expands to show the raw thinking text. No card
/// chrome — just a subtle hover affordance and muted styling.
#[allow(clippy::too_many_arguments)]
fn render_reasoning_entry(
    text: &str,
    streaming: bool,
    collapsed: bool,
    _user_toggled: bool,
    markdown: Option<Entity<Markdown>>,
    eix: usize,
    cix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
    cx: &mut App,
) -> gpui::AnyElement {
    let weak_workspace = tool_ctx.map(|c| c.weak.clone());
    let label = i18n::t("message-reasoning").to_string();
    // Default-collapsed: the live stream does not auto-play the reasoning
    // text — only a manual expand (user_toggled → collapsed=false) reveals
    // the body. The streaming spinner on the header still shows work in
    // progress while collapsed.
    let show_body = !collapsed;

    let mut row = v_flex().w_full().min_w_0().flex_1().italic().child(
        h_flex()
            .id(("reasoning-entry", eix))
            .w_full()
            .min_w_0()
            .px_2()
            .py_0p5()
            .gap_1p5()
            .items_center()
            .rounded(theme.radius)
            .cursor_pointer()
            .hover(|s| s.bg(theme.secondary.opacity(0.3)))
            .on_click(move |_, _window, cx: &mut App| {
                let Some(weak) = weak_workspace.clone() else {
                    return;
                };
                let _ = weak.update(cx, |w, cx| {
                    let conv = w.conversation.clone();
                    conv.update(cx, |c, cx| {
                        // Toggle the specific container's reasoning entry by
                        // index. `cix` is the container's position in the
                        // conversation items list, captured at render time.
                        if let Some(item) = c.items().get(cix) {
                            item.update(cx, |item, cx| {
                                if let ConvItem::Thinking(t) = item.kind_mut()
                                    && let Some(ActivityEntry::Reasoning {
                                        collapsed,
                                        user_toggled,
                                        ..
                                    }) = t.entries.get_mut(eix)
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
            .child(disclosure_icon(collapsed, theme))
            .child(
                Icon::new(if streaming {
                    IconName::LoaderCircle
                } else {
                    IconName::BookOpen
                })
                .xsmall()
                .text_color(theme.muted_foreground),
            )
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .overflow_x_hidden()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(truncate(&label, 80)),
            ),
    );

    if show_body && !text.is_empty() {
        // The persistent `Entity<Markdown>` (synced by the streaming/rebuild
        // path) carries parse-once incremental parsing + document-level
        // selection; fall back to a per-frame mount only before the first sync.
        let body = match markdown {
            Some(md) => md.into_any_element(),
            None => markdown_tv(("reasoning-entry-body", eix), text, theme, false, cx),
        };
        row = row.child(
            gpui::div()
                .id(("reasoning-body", eix))
                .w_full()
                .min_w_0()
                .pl_6()
                .py_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(body),
        );
    }
    row.into_any_element()
}

/// Render a tool entry in the activity tree: a compact execution node with
/// status icon, tool name, and expandable output. No card chrome — just a
/// lightweight row with a subtle hover affordance.
fn render_tool_entry(
    e: &ToolCallItem,
    eix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
    cx: &mut App,
) -> gpui::AnyElement {
    use agent::ToolCallStatus;
    let (status_icon, status_color) = match e.status {
        ToolCallStatus::PendingApproval | ToolCallStatus::Running => {
            (IconName::LoaderCircle, theme.muted_foreground)
        }
        ToolCallStatus::Success | ToolCallStatus::Continued => {
            (IconName::CircleCheck, theme.success)
        }
        ToolCallStatus::Error | ToolCallStatus::Denied => (IconName::CircleX, theme.danger),
        ToolCallStatus::Cancelled => (IconName::Minus, theme.muted_foreground),
    };
    // Default-collapsed: a streaming tool does not auto-reveal its output —
    // only a manual expand does. The status icon still spins while running.
    let show_output = !e.collapsed;
    let title = if !e.title.is_empty() {
        e.title.clone()
    } else if !e.name.is_empty() {
        e.name.clone()
    } else {
        i18n::t("thinking-tool-result").to_string()
    };
    let id_for_toggle = e.id.clone();
    let weak_workspace = tool_ctx.map(|c| c.weak.clone());

    // The terminal frame: a titlebar (command summary + status + disclosure)
    // that toggles the body, and the body itself. One bordered rounded box so
    // the pair reads as a single terminal window rather than a floating header
    // above a detached panel.
    let mut frame = v_flex()
        .w_full()
        .min_w_0()
        .flex_1()
        .italic()
        .border_1()
        .border_color(theme.border)
        .rounded(theme.radius)
        .overflow_hidden()
        .child(
            h_flex()
                .id(("act-header", eix))
                .w_full()
                .min_w_0()
                .px_2()
                .py_0p5()
                .gap_1p5()
                .items_center()
                .when(show_output, |h| h.border_b_1().border_color(theme.border))
                .cursor_pointer()
                .hover(|s| s.bg(theme.secondary.opacity(0.3)))
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
                                        && let Some(ActivityEntry::Tool(entry)) =
                                            t.entries.get_mut(eix)
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
                .child(Icon::new(status_icon).xsmall().text_color(status_color))
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w_0()
                        .overflow_x_hidden()
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.muted_foreground)
                        .child(truncate(&title, 80)),
                ),
        );

    if show_output && !e.output.is_empty() {
        frame = frame.child(render_tool_output(e, eix, theme, cx));
    }
    frame.into_any_element()
}

/// Format the activity segment summary for the model row of the reply that
/// follows it: " · 思考了 N 轮次 · 调用了 M 次工具 · Ss". Returns `None`
/// when every count is zero (or the duration is absent/zero) so the model
/// row stays bare. The leading " · " separates it from the model name.
fn activity_summary_text(s: &crate::conversation::ActivitySummary) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if s.thinking_rounds > 0 {
        parts.push(i18n::t_count("thinking-rounds", s.thinking_rounds as i64).to_string());
    }
    if s.tool_calls > 0 {
        parts.push(i18n::t_count("thinking-tool-calls", s.tool_calls as i64).to_string());
    }
    if let Some(secs) = s.duration_secs
        && secs > 0
    {
        parts.push(i18n::t_count("thinking-duration", secs as i64).to_string());
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!(" · {}", parts.join(" · ")))
    }
}

/// Aggregate a segment's tool calls into a comma-joined summary like
/// "reading 2 files, running 1 shell command". File/search categories count
/// unique targets (paths / patterns) extracted from the structured tool
/// input, so editing the same file twice reports "edited 1 file"; `bash`
/// counts command invocations. Categories with zero calls are omitted.
/// Trailing "…" mirrors the Claude Code "still working" cadence.
#[cfg(test)]
fn thinking_summary(entries: &[ActivityEntry]) -> String {
    use std::collections::BTreeSet;
    let mut reads: BTreeSet<String> = BTreeSet::new();
    let mut writes: BTreeSet<String> = BTreeSet::new();
    let mut edits: BTreeSet<String> = BTreeSet::new();
    let mut searches: BTreeSet<String> = BTreeSet::new();
    let mut globs: BTreeSet<String> = BTreeSet::new();
    let mut lists: BTreeSet<String> = BTreeSet::new();
    let mut running = 0u32;
    let mut fetching = 0u32;
    let mut browsing = 0u32;
    let mut other = 0u32;
    for e in entries {
        let ActivityEntry::Tool(e) = e else { continue };
        match e.name.as_str() {
            x if x == agent::tools::READ || x == agent::tools::WRITE || x == agent::tools::LIST => {
                if let Some(p) = e.input.get("path").and_then(|v| v.as_str()) {
                    match e.name.as_str() {
                        x if x == agent::tools::READ => {
                            reads.insert(p.to_string());
                        }
                        x if x == agent::tools::WRITE => {
                            writes.insert(p.to_string());
                        }
                        _ => {
                            lists.insert(p.to_string());
                        }
                    }
                } else {
                    match e.name.as_str() {
                        x if x == agent::tools::READ => {
                            reads.insert(String::new());
                        }
                        x if x == agent::tools::WRITE => {
                            writes.insert(String::new());
                        }
                        _ => {
                            lists.insert(String::new());
                        }
                    }
                }
            }
            x if x == agent::tools::EDIT => {
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
            x if x == agent::tools::BASH => {
                // Commands count by invocation, not by unique command text —
                // running `cargo build` twice is "2 commands", not 1.
                running += 1;
            }
            x if x == agent::tools::GREP => {
                let p = e
                    .input
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                searches.insert(p);
            }
            x if x == agent::tools::GLOB => {
                let p = e
                    .input
                    .get("pattern")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                globs.insert(p);
            }
            x if x == agent::tools::WEB_FETCH => {
                // Read-side network activity: fetching a doc URL or reading a
                // browser tab's content. Counted by invocation.
                fetching += 1;
            }
            "WebExploreOpen"
            | "WebExploreNavigate"
            | "WebExploreClick"
            | "WebExploreType"
            | "WebExploreScroll"
            | "WebExploreYield"
            | "web_explore_read_wait"
            | "web_explore_write"
            | "WebExploreClose" => {
                // Driving the browser tab itself — navigation and interaction.
                browsing += 1;
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
    push(fetching, "thinking-fetching", &mut parts);
    push(browsing, "thinking-browsing", &mut parts);
    push(other, "thinking-other", &mut parts);
    if parts.is_empty() {
        String::new()
    } else {
        format!("{}…", parts.join(", "))
    }
}

/// Render a plan-review item as a drawer card that emerges from beneath the
/// composer, mirroring `render_ask_user_card`'s shell (negative bottom margin +
/// shadow + slide-in animation + `PlanDrawer` key context). The header carries
/// the plan title and three affordances — download / copy / open in a right-pane
/// [PlanPreviewTab] — and the footer carries the three verdicts that delegate to
/// `Workspace::respond_plan_review`. The composer below stays live so the user
/// can discuss or refine the plan instead of picking a verdict.
fn render_plan_review_card(
    plan_text: &str,
    active: bool,
    ix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
    cx: &mut App,
) -> gpui::AnyElement {
    let Some(weak) = tool_ctx.map(|c| c.weak.clone()) else {
        return v_flex()
            .w_full()
            .min_w_0()
            .p_3()
            .rounded(px(18.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(markdown_tv(
                ("plan-review", ix),
                plan_text.to_string(),
                theme,
                false,
                cx,
            ))
            .into_any_element();
    };
    let accent = theme.accent;

    let download_btn = Button::new(("plan-download", ix))
        .ghost()
        .xsmall()
        .icon(IconName::ExternalLink)
        .tooltip(i18n::t("plan-card-download"))
        .on_click({
            let text = plan_text.to_string();
            move |_, _, cx: &mut App| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            }
        });

    let copy_btn = Button::new(("plan-copy", ix))
        .ghost()
        .xsmall()
        .icon(IconName::Copy)
        .tooltip(i18n::t("plan-card-copy"))
        .on_click({
            let text = plan_text.to_string();
            move |_, _, cx: &mut App| {
                cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            }
        });

    let plan_owned = plan_text.to_string();
    let weak_sb = weak.clone();
    let sidebar_btn = Button::new(("plan-sidebar", ix))
        .ghost()
        .xsmall()
        .icon(IconName::PanelRightClose)
        .tooltip(i18n::t("plan-card-sidebar"))
        .on_click(move |_, _, cx: &mut App| {
            let _ = weak_sb.update(cx, |ws, cx| {
                ws.open_plan_in_editor(plan_owned.clone(), cx);
            });
        });

    let header = h_flex()
        .w_full()
        .min_w_0()
        .items_center()
        .gap_2()
        .child(
            Icon::new(IconName::LayoutDashboard)
                .xsmall()
                .text_color(accent),
        )
        .child(
            gpui::div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(accent)
                .child(i18n::t("plan-card-title")),
        )
        .child(download_btn)
        .child(copy_btn)
        .child(sidebar_btn);

    let body = gpui::div().w_full().min_w_0().p_1().child(markdown_tv(
        ("plan-review", ix),
        plan_text.to_string(),
        theme,
        false,
        cx,
    ));
    if !active {
        // Consumed: a verdict was clicked or a free-form message superseded
        // this plan. Render a plain read-only record — no drawer shadow, no
        // slide-in, no verdict footer — so the plan stays readable as history
        // but cannot be re-judged.
        return v_flex()
            .w_full()
            .min_w_0()
            .gap_2p5()
            .px_3()
            .py_3()
            .rounded(px(18.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(header)
            .child(body)
            .into_any_element();
    }

    let weak_clear = weak.clone();
    let clear_btn = Button::new(("plan-verdict-clear", ix))
        .ghost()
        .small()
        .label(i18n::t("plan-drawer-clear"))
        .on_click(move |_, window, cx: &mut App| {
            let _ = weak_clear.update(cx, |w, cx| {
                w.respond_plan_review(agent::PlanReviewChoice::ImplementClearContext, window, cx);
            });
        });

    let weak_impl = weak;
    let impl_btn = Button::new(("plan-verdict-implement", ix))
        .ghost()
        .small()
        .label(i18n::t("plan-drawer-implement"))
        .on_click(move |_, window, cx: &mut App| {
            let _ = weak_impl.update(cx, |w, cx| {
                w.respond_plan_review(agent::PlanReviewChoice::Implement, window, cx);
            });
        });

    let footer = h_flex()
        .w_full()
        .min_w_0()
        .items_center()
        .justify_end()
        .gap_2()
        .border_t_1()
        .border_color(theme.border)
        .pt_2p5()
        .child(clear_btn)
        .child(impl_btn);

    v_flex()
        .id(format!("plan-card-{ix}"))
        .key_context("PlanDrawer")
        .w_full()
        .min_w_0()
        .gap_2p5()
        .px_3()
        .pt_3()
        // Extra bottom padding + negative margin let the composer cover the
        // drawer tail, so the card reads as emerging from beneath it — the same
        // trick render_ask_user_card uses.
        .pb_5()
        .mb(px(-10.))
        .rounded(px(18.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .shadow_lg()
        .child(header)
        .child(body)
        .child(footer)
        .with_animation(
            format!("plan-card-slide-{ix}"),
            Animation::new(Duration::from_millis(180)).with_easing(ease_out_quint()),
            |el, delta| el.mt(px(8. * (1. - delta))).opacity(delta),
        )
        .into_any_element()
}

fn render_ask_user_card(
    item: &ToolCallItem,
    ix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
    cx: &mut App,
) -> gpui::AnyElement {
    let Some(ctx) = tool_ctx else {
        return render_tool_call(item, ix, theme, tool_ctx, cx);
    };
    let Some(snapshot) = ctx.ask.clone() else {
        return render_tool_call(item, ix, theme, tool_ctx, cx);
    };
    if item.status != ToolCallStatus::PendingApproval {
        return render_tool_call(item, ix, theme, tool_ctx, cx);
    }

    let weak = ctx.weak.clone();
    let step = snapshot.step;
    let total = snapshot.total;
    let can_prev = step > 0;
    let can_next = step + 1 < total;

    let title = if snapshot.question.header.trim().is_empty() {
        i18n::t("workspace-clarify-title")
    } else {
        snapshot.question.header.clone().into()
    };

    let weak_prev = weak.clone();
    let weak_next = weak.clone();
    let weak_submit = weak.clone();
    let weak_cancel = weak.clone();
    let header = h_flex()
        .w_full()
        .min_w_0()
        .items_center()
        .justify_between()
        .child(
            gpui::div()
                .flex_1()
                .min_w_0()
                .overflow_x_hidden()
                .text_sm()
                .text_color(theme.foreground)
                .child(title),
        )
        .child(
            h_flex()
                .items_center()
                .gap_1()
                .child(
                    Button::new(("ask-card-prev", ix))
                        .ghost()
                        .xsmall()
                        .icon(IconName::ChevronLeft)
                        .when(!can_prev, |b| b.disabled(true))
                        .on_click(move |_, _, cx: &mut App| {
                            let _ = weak_prev.update(cx, |w, cx| w.ask_prev(cx));
                        }),
                )
                .child(
                    gpui::div()
                        .min_w(px(44.))
                        .text_center()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{} of {total}", step + 1)),
                )
                .child(
                    Button::new(("ask-card-next", ix))
                        .ghost()
                        .xsmall()
                        .icon(if can_next {
                            IconName::ChevronRight
                        } else {
                            IconName::Check
                        })
                        .on_click(move |_, window, cx: &mut App| {
                            if can_next {
                                let _ = weak_next.update(cx, |w, cx| w.ask_next(cx));
                            } else {
                                let _ = weak_submit.update(cx, |w, cx| w.submit_input(window, cx));
                            }
                        }),
                )
                .child(
                    Button::new(("ask-card-cancel", ix))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .on_click(move |_, _, cx: &mut App| {
                            let _ = weak_cancel.update(cx, |w, cx| {
                                w.resolve_auth(agent::PermissionDecision::Deny, cx);
                            });
                        }),
                ),
        );

    let question_row = gpui::div()
        .w_full()
        .min_w_0()
        .text_sm()
        .text_color(theme.foreground)
        .child(snapshot.question.question.clone());

    let mut options_block = v_flex().w_full().min_w_0().gap_1p5();
    for (oi, opt) in snapshot.question.options.iter().enumerate() {
        let selected = snapshot.selections.get(oi).copied().unwrap_or(false);
        let indicator_size = px(15.);
        let indicator = if snapshot.question.multi_select {
            if selected {
                h_flex()
                    .size(indicator_size)
                    .rounded(px(3.))
                    .border_1()
                    .border_color(theme.primary)
                    .bg(theme.primary.opacity(0.08))
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
                    .rounded(px(3.))
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
            .w_full()
            .min_w_0()
            .gap_2p5()
            .items_start()
            .px_2()
            .py_1p5()
            .rounded(px(10.))
            .when(selected, |row| row.bg(theme.accent.opacity(0.08)))
            .hover(|row| row.bg(theme.accent.opacity(0.06)))
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
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .overflow_x_hidden()
                    .child(
                        h_flex()
                            .min_w_0()
                            .items_center()
                            .gap_1p5()
                            .child(
                                gpui::div()
                                    .flex_shrink_0()
                                    .text_sm()
                                    .text_color(theme.foreground)
                                    .child(format!("{}.", oi + 1)),
                            )
                            .child(
                                gpui::div()
                                    .min_w_0()
                                    .overflow_x_hidden()
                                    .text_sm()
                                    .text_color(theme.foreground)
                                    .child(opt.label.clone()),
                            )
                            .when(opt.recommended, |row| {
                                row.child(
                                    Tag::new()
                                        .with_variant(TagVariant::Secondary)
                                        .small()
                                        .child(i18n::t("workspace-ask-recommended")),
                                )
                            }),
                    )
                    .when(!opt.description.trim().is_empty(), |col| {
                        col.child(
                            gpui::div()
                                .mt_0p5()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(opt.description.clone()),
                        )
                    }),
            );
        options_block = options_block.child(option_row);
    }

    v_flex()
        .id(format!(
            "ask-card-{}-{}",
            snapshot.id, snapshot.transition_gen
        ))
        .key_context("AskDrawer")
        .w_full()
        .min_w_0()
        .gap_2p5()
        .px_3()
        .pt_3()
        // The extra bottom padding plus negative margin lets the composer cover
        // the drawer tail, so the card reads as emerging from beneath it.
        .pb_5()
        .mb(px(-10.))
        .rounded(px(18.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.background)
        .shadow_lg()
        .child(header)
        .child(question_row)
        .child(options_block)
        .with_animation(
            format!("ask-card-slide-{}", snapshot.transition_gen),
            Animation::new(Duration::from_millis(180)).with_easing(ease_out_quint()),
            |el, delta| el.mt(px(8. * (1. - delta))).opacity(delta),
        )
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
    cx: &mut App,
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
    // `render_activity_entry`, which carries the same italic. The header is the
    // terminal titlebar (command summary + status + copy + chevron), clicking it
    // toggles the body; the pair shares one bordered frame.
    let mut card = v_flex()
        .group(format!("tool-{ix}"))
        .w_full()
        .min_w_0()
        .italic()
        .border_1()
        .border_color(theme.border)
        .rounded(theme.radius)
        .overflow_hidden()
        .child(
            h_flex()
                .id(("tool-header", ix))
                .w_full()
                .min_w_0()
                .px_2()
                .py_1()
                .gap_1p5()
                .items_center()
                .when(show_body, |h| h.border_b_1().border_color(theme.border))
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
                        .min_w_0()
                        .overflow_x_hidden()
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

    if show_body && !item.output.is_empty() {
        card = card.child(render_tool_output(item, ix, theme, cx));
    }
    card.into_any_element()
}

/// Fixed-height container with the tool's output. While streaming we paint a
/// plain monospace run (no markdown re-parse per chunk); once the final
/// `ToolResult` lands we mount the syntax-highlighted, scrollable `Markdown`.
/// The container keeps a deterministic height either way so the parent card
/// (and the list) reports a stable layout.
/// Strip the hashline envelope from a `read_file` result for display: drop the
/// leading `[path#TAG]` header and the `N:` line-number prefix on each numbered
/// line, so the user sees raw file content rather than the anchoring prefixes
/// the LLM relies on. Returns the input unchanged when the first line is not
/// the `[path#TAG]` header — non-hashline output (errors, non-`read_file` tools)
/// passes through verbatim. Only the first `digits:` run is stripped per line,
/// so file content that itself begins with `digits:` is preserved. The
/// persisted `ToolCallItem.output` is never touched; this is display-only.
fn strip_hashline_numbering(raw: &str) -> String {
    let mut lines = raw.split('\n');
    let Some(header) = lines.next() else {
        return String::new();
    };
    if !is_hashline_header(header) {
        return raw.to_string();
    }
    let mut out = String::with_capacity(raw.len());
    for (i, line) in lines.enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(strip_leading_line_number(line));
    }
    out
}

/// Recognize the `[path#TAG]` header `format_numbered` emits: bracketed, with
/// a non-empty path and tag separated by the first `#`.
fn is_hashline_header(line: &str) -> bool {
    let Some(inner) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return false;
    };
    let mut parts = inner.splitn(2, '#');
    matches!((parts.next(), parts.next()), (Some(p), Some(t)) if !p.is_empty() && !t.is_empty())
}

/// Strip a leading `<digits>:` prefix if present; return the remainder. A line
/// without the prefix passes through unchanged (per-line fallback).
fn strip_leading_line_number(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i > 0 && i < bytes.len() && bytes[i] == b':' {
        &line[i + 1..]
    } else {
        line
    }
}

fn render_tool_output(
    item: &ToolCallItem,
    ix: usize,
    theme: &Theme,
    cx: &mut App,
) -> gpui::AnyElement {
    // Persistent terminal panel: the conversation handler mounts it at every
    // live output chunk, finalized result, and reloaded-history entry, so the
    // common path renders the `Entity<TerminalPanel>` directly — giving tool
    // output the same document-level selection + Cmd/Ctrl+C copy as message
    // bodies (drag/copy survive across frames) and rendering the body as a
    // terminal-styled shell (cwd / toolchain / `❯` command + ANSI-colored
    // output) rather than a per-frame fenced code block.
    if let Some(panel) = item.panel.clone() {
        return panel.into_any_element();
    }
    // Defensive fallback (panel not yet mounted): render the display output as
    // a fenced code block so the body still appears while selection degrades to
    // per-frame. The persistent panel is the supported path; this only fires
    // for paths the conversation handler doesn't sync (e.g. a freshly built
    // entry before the first `ToolOutput`).
    let display_output = if item.streaming {
        live_tail(&item.output)
    } else {
        item.output.clone()
    };
    // Display-only transform for `read_file`: hide the hashline `[path#TAG]`
    // header and `N:` line numbers so the user sees raw file content. The raw
    // `output` (LLM-facing, persisted, edit_file-anchored) is untouched, so
    // copy-selection yields the display text while the LLM still sees numbered
    // output on the next turn. Non-`read_file` tools borrow the raw output
    // without allocating.
    let display: std::borrow::Cow<'_, str> = if item.name == agent::tools::READ {
        std::borrow::Cow::Owned(strip_hashline_numbering(&display_output))
    } else {
        std::borrow::Cow::Borrowed(&display_output)
    };
    let lang = lang_hint_for_tool(&item.name);
    let code = if let Some(l) = lang {
        format!("```{l}\n{display}\n```")
    } else {
        format!("```\n{display}\n```")
    };
    let container = gpui::div()
        .id(("tool-output", ix))
        .w_full()
        .min_w_0()
        .debug_selector(|| format!("message-overflow-tool-output-{ix}"))
        .px_3()
        .py_2()
        .border_t_1()
        .border_color(theme.border)
        .text_xs()
        .text_color(theme.muted_foreground);
    container
        .child(markdown_tv(
            ("tool-output-text", ix),
            code,
            theme,
            false,
            cx,
        ))
        .into_any_element()
}

fn agent_status_icon_name(status: ToolCallStatus) -> IconName {
    use agent::ToolCallStatus;
    match status {
        ToolCallStatus::Running | ToolCallStatus::PendingApproval => IconName::LoaderCircle,
        ToolCallStatus::Success | ToolCallStatus::Continued => IconName::CircleCheck,
        ToolCallStatus::Error | ToolCallStatus::Denied => IconName::CircleX,
        ToolCallStatus::Cancelled => IconName::Minus,
    }
}

/// Status icon for a sub-agent task row. Running/Pending get a spinning accent
/// loader; Success/Continued get a green check; Error/Denied get a red cross;
/// Cancelled gets a muted minus.
fn agent_status_icon(status: ToolCallStatus, theme: &Theme) -> (IconName, gpui::Hsla) {
    use agent::ToolCallStatus;
    let color = match status {
        ToolCallStatus::Running | ToolCallStatus::PendingApproval => theme.accent,
        ToolCallStatus::Success | ToolCallStatus::Continued => theme.success,
        ToolCallStatus::Error | ToolCallStatus::Denied => theme.danger,
        ToolCallStatus::Cancelled => theme.muted_foreground,
    };
    (agent_status_icon_name(status), color)
}

/// Render a sub-agent task as a single-line clickable item:
/// `[status_icon] {subagent_type} · {description}`.
/// Clicking opens (or focuses) a read-only `SubagentPanel` tab in the right
/// pane. No expand/collapse, no inline body, no metrics chip.
pub fn render_agent_task(
    item: &AgentTaskItem,
    ix: usize,
    theme: &Theme,
    agent_ctx: Option<&AgentTaskCtx>,
    _tool_ctx: Option<&ToolCallCtx>,
    _cx: &mut App,
) -> gpui::AnyElement {
    use agent::ToolCallStatus;
    let (icon_name, icon_color) = agent_status_icon(item.status, theme);
    let is_running = matches!(
        item.status,
        ToolCallStatus::Running | ToolCallStatus::PendingApproval
    );

    // Build the display title: "Type · Description", falling back gracefully.
    let display_title = if item.description.is_empty() {
        if item.subagent_type.is_empty() {
            item.id.clone()
        } else {
            item.subagent_type.clone()
        }
    } else if item.subagent_type.is_empty() {
        item.description.clone()
    } else {
        format!("{} · {}", item.subagent_type, item.description)
    };

    let id_for_click = item.id.clone();
    let weak = agent_ctx.map(|c| c.weak.clone());

    let tooltip_text = display_title.clone();
    let row = h_flex()
        .id(("agent-row", ix))
        .debug_selector(move || format!("message-overflow-agent-row-{ix}"))
        .w_full()
        .min_w_0()
        .px_2()
        .py_1()
        .gap_1p5()
        .items_center()
        .rounded(theme.radius)
        .cursor_pointer()
        .hover(|s| s.bg(theme.secondary.opacity(0.5)))
        .tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx))
        .on_click(move |_, _window, cx: &mut App| {
            let Some(weak) = weak.clone() else {
                return;
            };
            let _ = weak.update(cx, |w, cx| {
                w.open_subagent_tab_by_id(&id_for_click, cx);
            });
        });

    let icon_el: gpui::AnyElement = if is_running {
        Spinner::new()
            .icon(icon_name)
            .xsmall()
            .color(icon_color)
            .into_any_element()
    } else {
        Icon::new(icon_name)
            .xsmall()
            .text_color(icon_color)
            .into_any_element()
    };

    row.child(icon_el)
        .child(
            gpui::div()
                .debug_selector(move || format!("message-overflow-agent-title-{ix}"))
                .flex_1()
                .min_w_0()
                .truncate()
                .whitespace_nowrap()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .text_color(theme.muted_foreground)
                .child(display_title),
        )
        .into_any_element()
}

/// Render a background task status card. Shows the task kind icon,
/// description, task ID, status badge, event/byte counts, and a Stop
/// button while running.
fn render_background_task(
    bt: &BackgroundTaskItem,
    ix: usize,
    theme: &Theme,
    tool_ctx: Option<&ToolCallCtx>,
    _cx: &mut App,
) -> gpui::AnyElement {
    use agent::background_task::{TaskKind, TaskStatus};

    let is_running = matches!(bt.status, TaskStatus::Running | TaskStatus::Stopping);
    let kind_str = match bt.kind {
        TaskKind::MonitorCommand => i18n::t("background-task-kind-command"),
        TaskKind::MonitorWebSocket => i18n::t("background-task-kind-websocket"),
        TaskKind::BackgroundBash => i18n::t("background-task-kind-bash"),
    };
    let status_str = match bt.status {
        TaskStatus::Running => i18n::t("background-task-status-running"),
        TaskStatus::Stopping => i18n::t("background-task-status-stopping"),
        TaskStatus::Completed => i18n::t("background-task-status-completed"),
        TaskStatus::Failed => i18n::t("background-task-status-failed"),
        TaskStatus::TimedOut => i18n::t("background-task-status-timed-out"),
        TaskStatus::Stopped => i18n::t("background-task-status-stopped"),
        TaskStatus::SessionEnded => i18n::t("background-task-status-session-ended"),
    };
    let (icon_name, icon_color) = match bt.status {
        TaskStatus::Running | TaskStatus::Stopping => (IconName::LoaderCircle, theme.accent),
        TaskStatus::Completed => (IconName::CircleCheck, theme.success),
        TaskStatus::Failed | TaskStatus::TimedOut => (IconName::CircleX, theme.danger),
        TaskStatus::Stopped | TaskStatus::SessionEnded => (IconName::Minus, theme.muted_foreground),
    };
    let title = format!("{kind_str} · {desc}", desc = bt.description);
    let status_text = format!(
        "{status_str} · {events} events · {bytes} bytes",
        events = bt.event_count,
        bytes = bt.total_bytes,
    );
    let _ = bt.exit_code;
    let _ = tool_ctx;
    let task_id_for_stop = bt.task_id.clone();

    let row = h_flex()
        .id(("bg-task", ix))
        .w_full()
        .min_w_0()
        .px_2()
        .py_1p5()
        .gap_1p5()
        .items_center()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .when(is_running, |s| {
            s.child(
                Spinner::new()
                    .icon(icon_name.clone())
                    .xsmall()
                    .color(icon_color),
            )
        })
        .when(!is_running, |s| {
            s.child(Icon::new(icon_name).xsmall().text_color(icon_color))
        })
        .child(
            v_flex()
                .gap_0p5()
                .child(
                    gpui::div()
                        .truncate()
                        .whitespace_nowrap()
                        .text_xs()
                        .font_family(theme.mono_font_family.clone())
                        .text_color(theme.foreground)
                        .child(title),
                )
                .child(
                    gpui::div()
                        .truncate()
                        .whitespace_nowrap()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(status_text),
                ),
        );

    // Stop button while running.
    if is_running {
        row.child(
            Button::new(("stop-bg-task", ix))
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .label(i18n::t("background-task-stop"))
                .on_click({
                    let task_id = task_id_for_stop.clone();
                    move |_, _window, cx: &mut App| {
                        let task_id = task_id.clone();
                        drop(cx.background_spawn(async move {
                            let _ = agent::background_task::stop(&task_id);
                        }));
                    }
                }),
        )
        .into_any_element()
    } else {
        row.into_any_element()
    }
}

/// Map a tool call to its panel rendering kind and the body text the panel
/// renders. `read_file`/`write_file` → `File` (the agent-ui layer strips the
/// hashline `[path#TAG]` header + `N:` prefixes for read_file so the panel shows
/// plain content; write_file feeds the written content from the tool input).
/// `edit_file` → `Diff` (the panel classifies the `+`/`-`/`@@` lines). Anything
/// else → `Plain` (ANSI-parsed command output). Streaming bodies take the live
/// tail so the most recent lines are in view as they stream in.
fn tool_panel_body(entry: &ToolCallItem) -> (PanelKind, String) {
    let raw = if entry.streaming {
        live_tail(&entry.output)
    } else {
        entry.output.clone()
    };
    match entry.name.as_str() {
        x if x == agent::tools::READ => (PanelKind::File, strip_hashline_numbering(&raw)),
        // write_file's `output` is a one-line confirmation ("Wrote N bytes"), not
        // the file content; the content lives in the tool input. Show the written
        // content with a line-number gutter on success. On failure (`is_error`)
        // `output` carries the error — surface that as plain text via the default
        // arm so the user sees what went wrong, not just what was attempted.
        x if x == agent::tools::WRITE && !entry.is_error => {
            let content = entry
                .input
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            (PanelKind::File, content)
        }
        x if x == agent::tools::EDIT => (PanelKind::Diff, raw),
        _ => (PanelKind::Plain, raw),
    }
}

/// Snapshot the workdir's git state for the `TerminalPanel` prompt line:
/// branch name (`HEAD` when detached) + counts of modified / deleted / conflict
/// / untracked paths. Shells out to `git` (two short subprocess spawns) on the
/// background executor; returns `None` outside a git repo so the panel omits
/// the `git:…` segment entirely.
fn detect_git(cwd: &Path) -> Option<GitSummary> {
    use std::process::Command;
    // `rev-parse --abbrev-ref HEAD` succeeds inside any repo (yields the branch
    // name, or `HEAD` when detached) and fails outside one — so a missing branch
    // is a reliable not-a-repo signal.
    let branch = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let branch = branch?;
    let mut modified = 0usize;
    let mut deleted = 0usize;
    let mut conflict = 0usize;
    let mut untracked = 0usize;
    let porcelain = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    for line in porcelain.lines() {
        let b = line.as_bytes();
        if b.len() < 2 {
            continue;
        }
        let x = b[0] as char;
        let y = b[1] as char;
        if x == '?' && y == '?' {
            untracked += 1;
        } else if [x, y] == ['U', 'U']
            || [x, y] == ['A', 'A']
            || [x, y] == ['D', 'D']
            || [x, y] == ['A', 'U']
            || [x, y] == ['U', 'A']
            || [x, y] == ['D', 'U']
            || [x, y] == ['U', 'D']
        {
            conflict += 1;
        } else if x == 'D' || y == 'D' {
            deleted += 1;
        } else {
            // M (modified/staged), A (added), R (renamed), C (copied) — surface
            // as modified so the marker stays meaningful without a per-code table.
            modified += 1;
        }
    }
    Some(GitSummary {
        branch: Some(branch),
        modified,
        deleted,
        conflict,
        untracked,
    })
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
    ///
    /// `frozen_secs` is pinned to `Some(0)` so historical containers rebuilt
    /// from persisted messages do not fall back to a live
    /// `started_at.elapsed()` — `build_items` creates every container with a
    /// fresh `Instant::now()`, so an unfrozen timer would tick forever from
    /// zero on a reloaded thread.
    fn close_segment(items: &mut [ConvItem], seg_ix: Option<usize>) {
        if let Some(ix) = seg_ix
            && let Some(ConvItem::Thinking(t)) = items.get_mut(ix)
        {
            t.accepting_entries = false;
            t.streaming = false;
            t.collapsed = !t.user_toggled;
            if t.frozen_secs.is_none() {
                t.frozen_secs = Some(0);
            }
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
                        display_state: crate::conversation::UserMessageDisplayState::Normal,
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
                            // Assistant prose interrupts the activity segment:
                            // reasoning after it starts a fresh round.
                            close_segment(&mut items, active_segment_ix);
                            // Snapshot the segment's totals (after close pins
                            // `frozen_secs`) for this reply's model row.
                            let activity_summary =
                                active_segment_ix.and_then(|ix| match &items[ix] {
                                    ConvItem::Thinking(seg) => seg.activity_summary(),
                                    _ => None,
                                });
                            active_segment_ix = None;
                            items.push(ConvItem::Assistant {
                                text: t.clone(),
                                streaming: false,
                                token_usage: last_user_id.and_then(|id| usage.get(id).copied()),
                                activity_summary,
                            });
                        }
                        MessageContent::Thinking { text, .. } => {
                            // Fold reasoning into the active activity segment.
                            // A reasoning block before any tool calls opens the
                            // segment; subsequent reasoning and tools share it.
                            let entry = ActivityEntry::Reasoning {
                                text: text.clone(),
                                streaming: false,
                                collapsed: true,
                                user_toggled: false,
                                markdown: None,
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
                        MessageContent::ToolUse(tu) => {
                            if tu.name.as_ref() == agent::tools::AGENT {
                                // Sub-agent tasks stay as standalone compact
                                // rows; their full conversation lives in a
                                // read-only right-pane tab.
                                close_segment(&mut items, active_segment_ix);
                                active_segment_ix = None;
                                let (subagent_type, description) =
                                    crate::conversation::agent_task_labels(&tu.input);
                                items.push(ConvItem::AgentTask(AgentTaskItem {
                                    id: tu.id.clone(),
                                    subagent_type,
                                    description,
                                    status: ToolCallStatus::Success,
                                    is_error: false,
                                }));
                            } else if tu.name.as_ref() == agent::tools::ASK_USER_QUESTION {
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
                                    panel: None,
                                }));
                            } else {
                                // Ordinary tool call: fold into the active
                                // activity segment. The segment is created at
                                // the first ordinary tool call's position and
                                // stays there; subsequent tool calls (across
                                // assistant messages and tool-result user
                                // messages within the same turn) append to it.
                                let entry = ActivityEntry::Tool(ToolCallItem {
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
                                    panel: None,
                                });
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
            ConvItem::Assistant { streaming, .. } => {
                *streaming = true;
            }
            ConvItem::Thinking(t) => {
                // A resumed turn may still be mid-segment: mark the
                // container live so later-arriving `ToolCall`/`ToolOutput`
                // deltas fold into it instead of opening a fresh one.
                // Unfreeze the timer too — `close_segment` just pinned it,
                // but a live container must tick from `started_at` (now).
                t.accepting_entries = true;
                t.streaming = true;
                t.frozen_secs = None;
            }
            _ => {}
        }
    }
    items
}

/// Attach a tool_result to its matching item by id. Sub-agent results only
/// stamp their compact row's terminal state; ordinary tool results stamp the
/// entry inside the owning `ThinkingContainer`. A result with no matching
/// ToolUse becomes a standalone single-entry `ThinkingContainer` so an orphan
/// result still renders as a `⎿`.
fn pair_tool_result(items: &mut Vec<ConvItem>, tr: &LanguageModelToolResult) {
    let status = if tr.is_error {
        ToolCallStatus::Error
    } else {
        ToolCallStatus::Success
    };
    // Locate the owning item: an AgentTask or a Thinking-container entry.
    // Remember the entry index for the Thinking path so we can stamp the
    // right `⎿` inside its batch.
    let mut thinking_eix: Option<usize> = None;
    let ix = items.iter().position(|i| match i {
        ConvItem::AgentTask(t) => t.id == tr.tool_use_id,
        ConvItem::ToolCall(t) => t.id == tr.tool_use_id,
        ConvItem::Thinking(t) => match t.find_tool_entry_index(&tr.tool_use_id) {
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
            entries: vec![ActivityEntry::Tool(ToolCallItem {
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
                panel: None,
            })],
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
        ConvItem::Thinking(t) => {
            if let Some(eix) = thinking_eix
                && let Some(ActivityEntry::Tool(entry)) = t.entries.get_mut(eix)
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
    use gpui::{
        AnyWindowHandle, Bounds, Pixels, Render, TestAppContext, VisualTestContext, Window, size,
    };

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

    #[test]
    fn agent_statuses_use_the_expected_icons() {
        fn path(icon: IconName) -> SharedString {
            gpui_component::IconNamed::path(icon)
        }
        assert_eq!(
            path(agent_status_icon_name(ToolCallStatus::PendingApproval)),
            path(IconName::LoaderCircle)
        );
        assert_eq!(
            path(agent_status_icon_name(ToolCallStatus::Running)),
            path(IconName::LoaderCircle)
        );
        assert_eq!(
            path(agent_status_icon_name(ToolCallStatus::Success)),
            path(IconName::CircleCheck)
        );
        assert_eq!(
            path(agent_status_icon_name(ToolCallStatus::Continued)),
            path(IconName::CircleCheck)
        );
        assert_eq!(
            path(agent_status_icon_name(ToolCallStatus::Error)),
            path(IconName::CircleX)
        );
        assert_eq!(
            path(agent_status_icon_name(ToolCallStatus::Denied)),
            path(IconName::CircleX)
        );
        assert_eq!(
            path(agent_status_icon_name(ToolCallStatus::Cancelled)),
            path(IconName::Minus)
        );
    }

    struct MessageOverflowProbe;

    impl Render for MessageOverflowProbe {
        fn render(
            &mut self,
            _window: &mut Window,
            cx: &mut gpui::Context<Self>,
        ) -> impl IntoElement {
            let mut thinking = ThinkingContainer::new();
            thinking.collapsed = false;
            thinking.streaming = false;
            thinking.entries.push(ActivityEntry::Reasoning {
                text: "reasoning ".repeat(300),
                streaming: false,
                collapsed: false,
                user_toggled: true,
                markdown: None,
            });
            thinking.entries.push(ActivityEntry::Tool(ToolCallItem {
                id: "tool-long-final-output".into(),
                name: "Bash".into(),
                title: "bash: a very long final command title that should never force the message list wider than its host".into(),
                status: ToolCallStatus::Success,
                output: format!("{}\n{}", "x".repeat(2048), "y".repeat(2048)),
                is_error: false,
                input: serde_json::json!({"command": "printf"}),
                streaming: false,
                collapsed: false,
                user_toggled: true,
                panel: None,
            }));
            thinking.entries.push(ActivityEntry::Tool(ToolCallItem {
                id: "tool-long-streaming-output".into(),
                name: "Bash".into(),
                title: "bash: a very long streaming command title that should keep horizontal scroll local".into(),
                status: ToolCallStatus::Running,
                output: format!("{}\n{}", "z".repeat(2048), "w".repeat(2048)),
                is_error: false,
                input: serde_json::json!({"command": "printf"}),
                streaming: true,
                collapsed: false,
                user_toggled: true,
                panel: None,
            }));

            let thinking_item = ConvItem::Thinking(thinking);
            let agent_item = ConvItem::AgentTask(AgentTaskItem {
                id: "agent-long-title".into(),
                subagent_type: "Explore".into(),
                description: "检查一段非常长的中英文混合标题 and verify that it remains a single truncated line without rendering metrics or child output".into(),
                status: ToolCallStatus::Success,
                is_error: false,
            });
            let theme = cx.theme().clone();
            gpui::div()
                .id("message-overflow-probe")
                .w(px(260.))
                .min_w_0()
                .overflow_x_hidden()
                .debug_selector(|| "message-overflow-host".into())
                .child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .debug_selector(|| "message-overflow-item".into())
                        .child(render_item(
                            &thinking_item,
                            0,
                            "test-model",
                            &theme,
                            None,
                            None,
                            None,
                            cx,
                        ))
                        .child(render_item(
                            &agent_item,
                            1,
                            "test-model",
                            &theme,
                            None,
                            None,
                            None,
                            cx,
                        )),
                )
        }
    }

    fn assert_width_within(bounds: Bounds<Pixels>, max_width: Pixels, label: &str) {
        assert!(
            bounds.size.width <= max_width,
            "{label} should stay within the narrow host, got {:?}",
            bounds.size.width
        );
    }

    #[gpui::test]
    fn message_overflow_activity_tree_stays_within_narrow_width(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let window = cx.open_window(size(px(320.), px(520.)), move |_, _| MessageOverflowProbe);
        cx.run_until_parked();
        let any: AnyWindowHandle = window.into();
        let mut cx = VisualTestContext::from_window(any, cx);
        cx.update(|window, cx| {
            window.draw(cx).clear();
        });

        for selector in [
            "message-overflow-host",
            "message-overflow-item",
            "message-overflow-activity-tree-0",
            "message-overflow-activity-entry-0-0",
            "message-overflow-activity-entry-body-0-0",
            "message-overflow-activity-entry-0-1",
            "message-overflow-activity-entry-body-0-1",
            "message-overflow-tool-output-1",
            "message-overflow-activity-entry-0-2",
            "message-overflow-activity-entry-body-0-2",
            "message-overflow-tool-output-2",
            "message-overflow-agent-row-1",
            "message-overflow-agent-title-1",
        ] {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("missing debug bounds for {selector}"));
            assert_width_within(bounds, px(260.), selector);
        }

        let row = cx
            .debug_bounds("message-overflow-agent-row-1")
            .expect("agent row bounds");
        assert!(
            row.size.height <= px(40.),
            "agent task must remain a compact single row, got {:?}",
            row.size.height
        );
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
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            Message::user_with_content(vec![tr("tu_1", "Read", "a contents")]),
            Message::assistant(vec![tu(
                "tu_2",
                "Edit",
                serde_json::json!({"patch": "[a.rs#T1]\nINS x"}),
            )]),
            Message::user_with_content(vec![tr("tu_2", "Edit", "ok")]),
            Message::assistant(vec![tu(
                "tu_3",
                "Bash",
                serde_json::json!({"command": "cargo build"}),
            )]),
            Message::user_with_content(vec![tr("tu_3", "Bash", "Built.")]),
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
        // Entries in arrival order — all tool entries.
        let tool_ids: Vec<&str> = segments[0]
            .entries
            .iter()
            .filter_map(|e| match e {
                ActivityEntry::Tool(t) => Some(t.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_ids, vec!["tu_1", "tu_2", "tu_3"]);
    }

    /// Historical `ThinkingContainer`s rebuilt from persisted messages must
    /// have `frozen_secs` pinned so `activity_summary` reports a fixed duration
    /// (not a live `started_at.elapsed()`) — otherwise the timer ticks forever
    /// from zero on a reloaded thread (regression: the old "计时一直进行" bug).
    #[test]
    fn build_items_freezes_historical_segment_elapsed() {
        let messages = vec![
            Message::user("do it".to_string()),
            Message::assistant(vec![tu(
                "tu_1",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            Message::user_with_content(vec![tr("tu_1", "Read", "a contents")]),
            // Second user prompt closes the first turn's segment.
            Message::user("next".to_string()),
        ];
        let items = build_items(&messages, &HashMap::new(), false);
        let seg = items.iter().find_map(|i| match i {
            ConvItem::Thinking(t) => Some(t),
            _ => None,
        });
        let seg = seg.expect("segment present");
        assert!(
            seg.frozen_secs.is_some(),
            "historical segment must pin elapsed to stop the timer"
        );
    }
    #[test]
    fn build_items_user_prompt_is_turn_boundary_tool_result_is_not() {
        let messages = vec![
            Message::user("turn one".to_string()),
            Message::assistant(vec![tu(
                "tu_1",
                "Read",
                serde_json::json!({"path": "a.rs"}),
            )]),
            // tool-result user message — NOT a turn boundary.
            Message::user_with_content(vec![tr("tu_1", "Read", "a")]),
            Message::assistant(vec![tu(
                "tu_2",
                "Bash",
                serde_json::json!({"command": "ls"}),
            )]),
            Message::user_with_content(vec![tr("tu_2", "Bash", "files")]),
            // New user prompt — IS a turn boundary.
            Message::user("turn two".to_string()),
            Message::assistant(vec![tu(
                "tu_3",
                "Read",
                serde_json::json!({"path": "b.rs"}),
            )]),
            Message::user_with_content(vec![tr("tu_3", "Read", "b")]),
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

    /// `agent` and `AskUserQuestion` must stay standalone top-level cards — they
    /// must not be swallowed into the activity segment, even when they appear in
    /// the same assistant message as ordinary tools.
    #[test]
    fn build_items_keeps_special_tools_standalone() {
        let messages = vec![
            Message::user("go".to_string()),
            Message::assistant(vec![
                tu("tu_1", "Read", serde_json::json!({"path": "a.rs"})),
                tu(
                    "tu_agent",
                    "Agent",
                    serde_json::json!({
                        "subagent_type": "r",
                        "description": "inspect p",
                        "prompt": "p"
                    }),
                ),
                tu(
                    "tu_ask",
                    "AskUserQuestion",
                    serde_json::json!({"questions": [{"question": "q", "header": "h", "options": [{"text": "a", "value": "a"}], "multi_select": false}]}),
                ),
            ]),
            Message::user_with_content(vec![
                tr("tu_1", "Read", "a"),
                tr("tu_agent", "Agent", "{\"final\":\"done\"}"),
                tr("tu_ask", "AskUserQuestion", "answered"),
            ]),
        ];
        let items = build_items(&messages, &HashMap::new(), false);
        // The ordinary tool folds into a segment; the two special tools are
        // standalone top-level cards.
        let agent = items.iter().find_map(|i| match i {
            ConvItem::AgentTask(t) if t.id == "tu_agent" => Some(t),
            _ => None,
        });
        let ask = items.iter().find_map(|i| match i {
            ConvItem::ToolCall(t) if t.name == agent::tools::ASK_USER_QUESTION => Some(t),
            _ => None,
        });
        let seg = items.iter().find_map(|i| match i {
            ConvItem::Thinking(t) => Some(t),
            _ => None,
        });
        assert!(agent.is_some(), "agent task is standalone");
        assert!(ask.is_some(), "AskUserQuestion is standalone");
        let seg = seg.expect("ordinary tool folded into a segment");
        assert_eq!(
            seg.entries.len(),
            1,
            "only the ordinary tool is in the segment"
        );
        match &seg.entries[0] {
            ActivityEntry::Tool(t) => assert_eq!(t.id, "tu_1"),
            _ => panic!("expected tool entry"),
        }
    }

    /// `thinking_summary` deduplicates file targets: editing the same file
    /// twice reports "edited 1 file", not 2. `bash` counts invocations.
    #[test]
    fn thinking_summary_deduplicates_file_targets() {
        let entries: Vec<ActivityEntry> = vec![
            ActivityEntry::Tool(ToolCallItem {
                id: "1".into(),
                name: "Edit".into(),
                title: String::new(),
                status: ToolCallStatus::Success,
                output: String::new(),
                is_error: false,
                input: serde_json::json!({"patch": "[src/a.rs#T1]\nINS x"}),
                streaming: false,
                collapsed: false,
                user_toggled: false,
                panel: None,
            }),
            ActivityEntry::Tool(ToolCallItem {
                id: "2".into(),
                name: "Edit".into(),
                title: String::new(),
                status: ToolCallStatus::Success,
                output: String::new(),
                is_error: false,
                // Same path → deduped.
                input: serde_json::json!({"patch": "[src/a.rs#T2]\nINS y"}),
                streaming: false,
                collapsed: false,
                user_toggled: false,
                panel: None,
            }),
            ActivityEntry::Tool(ToolCallItem {
                id: "3".into(),
                name: "Edit".into(),
                title: String::new(),
                status: ToolCallStatus::Success,
                output: String::new(),
                is_error: false,
                // Different path.
                input: serde_json::json!({"patch": "[src/b.rs#T1]\nINS z"}),
                streaming: false,
                collapsed: false,
                user_toggled: false,
                panel: None,
            }),
            ActivityEntry::Tool(ToolCallItem {
                id: "4".into(),
                name: "Bash".into(),
                title: String::new(),
                status: ToolCallStatus::Success,
                output: String::new(),
                is_error: false,
                input: serde_json::json!({"command": "cargo build"}),
                streaming: false,
                collapsed: false,
                user_toggled: false,
                panel: None,
            }),
            ActivityEntry::Tool(ToolCallItem {
                id: "5".into(),
                name: "Bash".into(),
                title: String::new(),
                status: ToolCallStatus::Success,
                output: String::new(),
                is_error: false,
                // Same command → still counted as 2 invocations.
                input: serde_json::json!({"command": "cargo build"}),
                streaming: false,
                collapsed: false,
                user_toggled: false,
                panel: None,
            }),
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
        t.entries.push(ActivityEntry::Tool(ToolCallItem {
            id: "1".into(),
            name: "Read".into(),
            title: String::new(),
            status: ToolCallStatus::Success,
            output: String::new(),
            is_error: false,
            input: serde_json::Value::Null,
            streaming: false,
            collapsed: true,
            user_toggled: false,
            panel: None,
        }));
        t.entries.push(ActivityEntry::Tool(ToolCallItem {
            id: "2".into(),
            name: "Bash".into(),
            title: String::new(),
            status: ToolCallStatus::Success,
            output: String::new(),
            is_error: false,
            input: serde_json::Value::Null,
            streaming: false,
            collapsed: true,
            user_toggled: false,
            panel: None,
        }));

        /// Replicate the visibility logic from `render_thinking` for testing.
        fn visible_entries(t: &ThinkingContainer) -> Vec<usize> {
            if !t.collapsed {
                (0..t.entries.len()).collect()
            } else if t.streaming {
                t.entries
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, e)| match e {
                        ActivityEntry::Reasoning { streaming, .. } => *streaming,
                        ActivityEntry::Tool(tool) => {
                            tool.streaming
                                || matches!(
                                    tool.status,
                                    ToolCallStatus::Running | ToolCallStatus::PendingApproval
                                )
                        }
                    })
                    .or(t.entries.iter().enumerate().next_back())
                    .map(|(i, _)| i)
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            }
        }

        // Frozen + collapsed: no entries visible.
        assert!(
            visible_entries(&t).is_empty(),
            "frozen + collapsed shows no entries"
        );

        // Streaming + collapsed: the running entry (or latest) is visible.
        t.streaming = true;
        if let ActivityEntry::Tool(tool) = &mut t.entries[0] {
            tool.streaming = true;
            tool.status = ToolCallStatus::Running;
        }
        let vis = visible_entries(&t);
        assert_eq!(vis.len(), 1, "streaming + collapsed shows 1 entry");
        assert_eq!(vis[0], 0, "shows the running entry");

        // Expanded: all entries visible.
        t.collapsed = false;
        assert_eq!(visible_entries(&t).len(), 2, "expanded shows all entries");
    }

    #[test]
    fn strip_hashline_numbering_drops_header_and_line_prefixes() {
        let raw = "[a.rs#1A2B]\n1:fn main() {\n2:}";
        assert_eq!(strip_hashline_numbering(raw), "fn main() {\n}");
    }

    #[test]
    fn strip_hashline_numbering_preserves_digit_colon_content() {
        // File content that itself begins with `digits:` survives: only the
        // first `N:` run (the hashline line number) is stripped.
        let raw = "[cfg.toml#TAG]\n1:10: first\n2:20: second";
        assert_eq!(strip_hashline_numbering(raw), "10: first\n20: second");
    }

    #[test]
    fn strip_hashline_numbering_passes_through_non_header_output() {
        // Errors and non-hashline shapes are returned verbatim.
        let err = "file not found: missing.rs";
        assert_eq!(strip_hashline_numbering(err), "file not found: missing.rs");
        assert_eq!(strip_hashline_numbering(""), "");
    }

    #[test]
    fn strip_hashline_numbering_keeps_blank_lines() {
        let raw = "[a.rs#T]\n1:line one\n2:\n3:line three";
        assert_eq!(strip_hashline_numbering(raw), "line one\n\nline three");
    }

    /// `MessageContent::Thinking` folds into the same activity segment as
    /// tool calls — one `ThinkingContainer` holds both reasoning rounds and
    /// tool entries. This mirrors the live `apply()` behavior where
    /// `AgentThinking` deltas fold into the active segment.
    #[test]
    fn build_items_folds_thinking_into_activity_segment() {
        let messages = vec![
            Message::user("go".to_string()),
            Message::assistant(vec![
                MessageContent::Thinking {
                    text: "let me think about this".to_string(),
                    signature: None,
                },
                MessageContent::ToolUse(LanguageModelToolUse {
                    id: "tu_1".to_string(),
                    name: Arc::from("Read"),
                    raw_input: String::new(),
                    input: serde_json::json!({"path": "a.rs"}),
                    is_input_complete: true,
                    thought_signature: None,
                }),
            ]),
            Message::user_with_content(vec![MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: "tu_1".to_string(),
                tool_name: Arc::from("Read"),
                is_error: false,
                content: "file contents".to_string(),
            })]),
        ];
        let items = build_items(&messages, &HashMap::new(), false);
        let containers: Vec<&ThinkingContainer> = items
            .iter()
            .filter_map(|i| match i {
                ConvItem::Thinking(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(containers.len(), 1, "one container for thinking + tool");
        let t = containers[0];
        assert_eq!(t.entries.len(), 2, "one reasoning + one tool");
        assert!(
            matches!(&t.entries[0], ActivityEntry::Reasoning { text, .. } if text == "let me think about this"),
            "first entry is reasoning"
        );
        assert!(
            matches!(&t.entries[1], ActivityEntry::Tool(tool) if tool.id == "tu_1"),
            "second entry is tool"
        );
    }

    /// Multiple `Thinking` blocks within the same turn produce multiple
    /// reasoning entries within the same activity segment.
    #[test]
    fn build_items_multiple_thinking_rounds_in_one_segment() {
        let messages = vec![
            Message::user("go".to_string()),
            // First assistant response: thinking + tool
            Message::assistant(vec![
                MessageContent::Thinking {
                    text: "round 1".to_string(),
                    signature: None,
                },
                MessageContent::ToolUse(LanguageModelToolUse {
                    id: "tu_1".to_string(),
                    name: Arc::from("Read"),
                    raw_input: String::new(),
                    input: serde_json::Value::Null,
                    is_input_complete: true,
                    thought_signature: None,
                }),
            ]),
            Message::user_with_content(vec![MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: "tu_1".to_string(),
                tool_name: Arc::from("Read"),
                is_error: false,
                content: "done".to_string(),
            })]),
            // Second assistant response: more thinking + text.
            // The thinking folds into the same segment since the text comes
            // after it (text closes the segment, but thinking is already in).
            Message::assistant(vec![
                MessageContent::Thinking {
                    text: "round 2".to_string(),
                    signature: None,
                },
                MessageContent::Text("here is the answer".to_string()),
            ]),
        ];
        let items = build_items(&messages, &HashMap::new(), false);
        let containers: Vec<&ThinkingContainer> = items
            .iter()
            .filter_map(|i| match i {
                ConvItem::Thinking(t) => Some(t),
                _ => None,
            })
            .collect();
        // Both thinking rounds and the tool share one segment (the text
        // closes it after the second thinking is already added).
        assert_eq!(containers.len(), 1, "one segment for the whole turn");
        let t = containers[0];
        assert_eq!(t.entries.len(), 3, "2 reasoning + 1 tool");
        // Verify entry types and order.
        assert!(matches!(
            &t.entries[0],
            ActivityEntry::Reasoning { text, .. } if text == "round 1"
        ));
        assert!(matches!(&t.entries[1], ActivityEntry::Tool(tool) if tool.id == "tu_1"));
        assert!(matches!(
            &t.entries[2],
            ActivityEntry::Reasoning { text, .. } if text == "round 2"
        ));
    }

    /// `activity_summary` snapshots the segment's round/tool counts and the
    /// pinned elapsed for the following reply's model row.
    #[test]
    fn activity_summary_counts_rounds_tools_duration() {
        let mut t = ThinkingContainer::new();
        // Empty segment → no row suffix.
        assert!(t.activity_summary().is_none());
        t.frozen_secs = Some(42);
        t.entries.push(ActivityEntry::Reasoning {
            text: "thinking...".into(),
            streaming: false,
            collapsed: true,
            user_toggled: false,
            markdown: None,
        });
        t.entries.push(ActivityEntry::Tool(ToolCallItem {
            id: "t1".into(),
            name: "Read".into(),
            title: String::new(),
            status: ToolCallStatus::Success,
            output: String::new(),
            is_error: false,
            input: serde_json::json!({"path": "a.rs"}),
            streaming: false,
            collapsed: true,
            user_toggled: false,
            panel: None,
        }));
        t.entries.push(ActivityEntry::Tool(ToolCallItem {
            id: "t2".into(),
            name: "Bash".into(),
            title: String::new(),
            status: ToolCallStatus::Success,
            output: String::new(),
            is_error: false,
            input: serde_json::json!({"command": "cargo build"}),
            streaming: false,
            collapsed: true,
            user_toggled: false,
            panel: None,
        }));
        let s = t.activity_summary().expect("non-empty segment");
        assert_eq!(s.thinking_rounds, 1);
        assert_eq!(s.tool_calls, 2);
        assert_eq!(s.duration_secs, Some(42));

        // A segment whose only counts are zero still reports `None`.
        let mut empty = ThinkingContainer::new();
        empty.frozen_secs = Some(0);
        assert!(empty.activity_summary().is_none());
    }

    /// Historical rebuild of the issue #216 scenario: text THEN thinking within
    /// one assistant message must produce two activity segments, not fold the
    /// post-text thinking into the pre-text segment. The `Text` arm calls
    /// `close_segment` so the subsequent `Thinking` opens a fresh container.
    /// This test confirms the rebuild path is correct (the live `apply` path
    /// is fixed by `close_for_text` to match this behavior).
    #[test]
    fn build_items_text_before_thinking_opens_new_segment() {
        // Round 1: thinking + tool → tool result
        // Round 2: Text THEN Thinking (the issue's temporal-inversion scenario)
        let messages = vec![
            Message::user("go".to_string()),
            Message::assistant(vec![
                MessageContent::Thinking {
                    text: "round 1".to_string(),
                    signature: None,
                },
                tu("tu_1", "Read", serde_json::json!({"path": "a.rs"})),
            ]),
            Message::user_with_content(vec![tr("tu_1", "Read", "a")]),
            Message::assistant(vec![
                MessageContent::Text("the answer".to_string()),
                MessageContent::Thinking {
                    text: "round 2".to_string(),
                    signature: None,
                },
            ]),
        ];
        let items = build_items(&messages, &HashMap::new(), false);
        let containers: Vec<&ThinkingContainer> = items
            .iter()
            .filter_map(|i| match i {
                ConvItem::Thinking(t) => Some(t),
                _ => None,
            })
            .collect();
        assert_eq!(
            containers.len(),
            2,
            "text closes old segment, thinking opens new"
        );
        assert_eq!(containers[0].entries.len(), 2, "round 1: thinking + tool");
        assert_eq!(containers[1].entries.len(), 1, "round 2: thinking only");
        assert_eq!(
            items
                .iter()
                .filter(|i| matches!(i, ConvItem::Assistant { .. }))
                .count(),
            1,
            "one assistant text between the two segments"
        );
    }
}
