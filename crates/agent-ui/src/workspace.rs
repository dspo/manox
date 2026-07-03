//! Top-level workspace view.
//!
//! Holds `Entity<agent::Thread>` + `Entity<Sidebar>`; `cx.subscribe` handles:
//! - `ThreadEvent`: text/thinking/tool deltas go to `ConversationState`; `ToolCallAuthorization` opens an approval overlay;
//!   the terminal `Stop` (non-ToolUse) triggers `save_thread`.
//! - `SidebarEvent`: new conversation / open history / delete.
//!
//! Enter in the input box → append a user message + run_turn + persist (the sidebar shows the new entry immediately).

use std::path::PathBuf;

use agent::language_model::StopReason;
use agent::provider::config::WireApi;
use agent::provider::registry;
use agent::{PermissionDecision, Thread, ThreadEvent, ThreadId, save_thread};
use gpui::{
    AnyElement, Context, CursorStyle, DismissEvent, DragMoveEvent, Entity, MouseButton,
    MouseUpEvent, Pixels, Render, ScrollHandle, ScrollWheelEvent, Subscription, Window, prelude::*,
    px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme, TitleBar,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{PopupMenu, PopupMenuItem},
    tab::TabBar,
    tag::{Tag, TagVariant},
    text::TextView,
    v_flex,
};

use crate::conversation::ConversationState;
use crate::views::composer_menu::{
    PendingAttachment, build_plus_menu, build_slash_menu, load_attachment, render_attachment_chips,
};
use crate::views::sidebar::{Sidebar, SidebarEvent};
use crate::views::{centered, message::render_item};

/// A pending tool-call authorization prompted by `ThreadEvent::ToolCallAuthorization`.
struct PendingAuth {
    id: String,
    tool_name: String,
    summary: String,
}

/// A parsed `AskUserQuestion` prompt awaiting the user's selections.
struct PendingAsk {
    id: String,
    questions: Vec<AskQuestion>,
    /// Per-question toggled option flags, aligned with `questions[i].options`.
    selections: Vec<Vec<bool>>,
    /// Per-question free-form "Other" input; non-empty text overrides the
    /// option selection for that question.
    others: Vec<Entity<InputState>>,
}

struct AskQuestion {
    question: String,
    header: String,
    multi_select: bool,
    options: Vec<AskOption>,
}

struct AskOption {
    label: String,
    description: String,
}

pub struct Workspace {
    pub(crate) cwd: PathBuf,
    pub(crate) thread: Entity<Thread>,
    sidebar: Entity<Sidebar>,
    conversation: ConversationState,
    pub(crate) input_state: Entity<InputState>,
    /// Right-side markdown composer; opened via the `ToggleEditor` shortcut.
    /// Plain-text edit mode by default; `ToggleEditorPreview` switches to a
    /// rendered markdown preview (gpui-component `TextView::markdown`).
    editor_state: Entity<InputState>,
    editor_open: bool,
    editor_preview: bool,
    /// Editor pane width, driven by dragging the divider. In-memory only.
    editor_width: Pixels,
    pending_auth: Option<PendingAuth>,
    /// A pending `AskUserQuestion` card; takes precedence over `pending_auth`
    /// in the overlay slot. `None` unless the latest authorization is an
    /// `AskUserQuestion` call.
    pending_ask: Option<PendingAsk>,
    pub(crate) model_open: bool,
    /// PopupMenu entity for the open model selector; created on open, destroyed on close.
    model_menu: Option<Entity<PopupMenu>>,
    model_menu_sub: Option<Subscription>,
    plus_open: bool,
    plus_menu: Option<Entity<PopupMenu>>,
    plus_menu_sub: Option<Subscription>,
    slash_open: bool,
    slash_menu: Option<Entity<PopupMenu>>,
    slash_menu_sub: Option<Subscription>,
    /// Files picked via the `+` menu, not yet sent. Cleared on submit.
    pending_attachments: Vec<PendingAttachment>,
    thread_sub: Option<Subscription>,
    sidebar_sub: Option<Subscription>,
    input_sub: Option<Subscription>,
    editor_sub: Option<Subscription>,
    /// Tracked scroll position of the message list; used to auto-scroll to the
    /// latest message while the user is parked at the bottom and to leave them
    /// alone when they've scrolled up to read history.
    scroll_handle: ScrollHandle,
    /// Whether new content should keep the viewport pinned to the bottom.
    /// Reset to `false` by manual user scrolls that move the viewport away.
    stick_to_bottom: bool,
}

/// Right-side composer width. Wide enough for rendered markdown
/// (headings, lists, code blocks) alongside the 1100px window.
const EDITOR_PANEL_WIDTH: f32 = 640.;
const EDITOR_MIN_WIDTH: f32 = 320.;
const EDITOR_MAX_WIDTH: f32 = 960.;
/// Width of the drag handle between the main column and the editor pane.
const EDITOR_DIVIDER_WIDTH: f32 = 6.;
// Mirrors `views/sidebar.rs` (`Sidebar` renders at `w(px(260.))`). Kept here so
// the editor pane's resize clamp can reserve space for the sidebar + main
// column without depending on the sidebar's internals.
const SIDEBAR_WIDTH: f32 = 260.;
/// Floor for the main column width when the editor pane is dragged wide.
const MAIN_MIN_WIDTH: f32 = 160.;

/// Drag payload for the editor pane divider. Doubles as the invisible drag
/// ghost view, mirroring Zed's `DraggedDock` pattern.
struct DraggedEditorDivider;

impl Render for DraggedEditorDivider {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let thread = {
            let id = ThreadId(uuid::Uuid::new_v4().to_string());
            Thread::new(id, cwd.clone(), cx)
        };

        let input_state = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .rows(4)
                .submit_on_enter(true)
                .placeholder("输入消息…")
        });

        let editor_state = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("markdown")
                .line_number(true)
                .folding(false)
                .soft_wrap(true)
                .submit_on_enter(false)
                .placeholder("编写 markdown…（Cmd-Enter 发送）")
        });

        let sidebar = cx.new(Sidebar::new);

        let mut ws = Self {
            cwd,
            thread,
            sidebar,
            conversation: ConversationState::new(),
            input_state,
            editor_state,
            editor_open: false,
            editor_preview: false,
            editor_width: px(EDITOR_PANEL_WIDTH),
            pending_auth: None,
            pending_ask: None,
            model_open: false,
            model_menu: None,
            model_menu_sub: None,
            plus_open: false,
            plus_menu: None,
            plus_menu_sub: None,
            slash_open: false,
            slash_menu: None,
            slash_menu_sub: None,
            pending_attachments: Vec::new(),
            thread_sub: None,
            sidebar_sub: None,
            input_sub: None,
            editor_sub: None,
            scroll_handle: ScrollHandle::new(),
            stick_to_bottom: true,
        };
        ws.thread_sub = Some(ws.subscribe_thread(cx));
        ws.sidebar_sub = Some(ws.subscribe_sidebar(cx));
        ws.input_sub = Some(ws.subscribe_input(window, cx));
        ws.editor_sub = Some(ws.subscribe_editor(window, cx));
        let id = ws.thread.read(cx).id.0.clone();
        ws.sidebar.update(cx, |s, cx| s.set_selected(Some(id), cx));
        ws
    }

    fn subscribe_thread(&self, cx: &mut Context<Self>) -> Subscription {
        let thread = self.thread.clone();
        cx.subscribe(&thread, |this, _thread, ev: &ThreadEvent, cx| {
            match ev {
                ThreadEvent::ToolCallAuthorization {
                    id,
                    tool_name,
                    summary,
                    input,
                } => {
                    if tool_name == "AskUserQuestion" {
                        this.pending_ask = parse_pending_ask(id.clone(), input.clone());
                    }
                    this.pending_auth = Some(PendingAuth {
                        id: id.clone(),
                        tool_name: tool_name.clone(),
                        summary: summary.clone(),
                    });
                    cx.notify();
                }
                ThreadEvent::Stop(reason) => {
                    this.conversation.apply(ev);
                    if this.stick_to_bottom {
                        this.scroll_handle.scroll_to_bottom();
                    }
                    // Persist on terminal state (not the ToolUse mid-state).
                    if !matches!(reason, StopReason::ToolUse) {
                        save_thread(this.thread.clone(), cx);
                    }
                    cx.notify();
                }
                _ => {
                    // Capture stick state before mutating the list: a user at the
                    // bottom should stay glued there as new tokens stream in. If
                    // they've scrolled up, we leave their viewport alone.
                    let was_at_bottom =
                        this.stick_to_bottom || Self::is_at_bottom(&this.scroll_handle);
                    this.conversation.apply(ev);
                    this.stick_to_bottom = was_at_bottom;
                    if was_at_bottom {
                        this.scroll_handle.scroll_to_bottom();
                    }
                    cx.notify();
                }
            }
        })
    }

    /// True if the message list is parked at the bottom within `STICK_THRESHOLD`
    /// pixels. Uses the cached `stick_to_bottom` flag as a fast path: if we
    /// already know the user wanted to follow along, don't second-guess them.
    fn is_at_bottom(handle: &ScrollHandle) -> bool {
        const STICK_THRESHOLD: f32 = 32.;
        let max = handle.max_offset();
        if max.y <= px(0.) {
            return true;
        }
        handle.offset().y >= max.y - px(STICK_THRESHOLD)
    }

    fn subscribe_sidebar(&self, cx: &mut Context<Self>) -> Subscription {
        let sidebar = self.sidebar.clone();
        cx.subscribe(&sidebar, |this, _sidebar, ev: &SidebarEvent, cx| match ev {
            SidebarEvent::NewThread => this.start_new_thread(cx),
            SidebarEvent::OpenThread(id) => this.open_thread(id.clone(), cx),
            SidebarEvent::DeleteThread(id) => this.delete_thread(id.clone(), cx),
        })
    }

    fn subscribe_input(&self, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        let input = self.input_state.clone();
        cx.subscribe_in(
            &input,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { shift, .. } if !shift => this.submit_input(window, cx),
                InputEvent::Change => this.sync_slash_menu(window, cx),
                _ => {}
            },
        )
    }

    /// Submit the right-side editor on Cmd/Ctrl-Enter (`InputEvent::PressEnter`
    /// with `secondary` set). Plain Enter inserts a newline (submit_on_enter
    /// is off for the panel editor).
    fn subscribe_editor(&self, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        let editor = self.editor_state.clone();
        cx.subscribe_in(&editor, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { secondary, shift } = ev
                && *secondary
                && !shift
            {
                this.submit_editor(window, cx);
            }
        })
    }

    /// Open the `⁄` command menu when the input is exactly `/`, close it otherwise. The menu is
    /// static decoration; selecting a row does nothing but dismiss.
    fn sync_slash_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.input_state.read(cx).value().to_string();
        let should_open = value == "/";
        if should_open && !self.slash_open {
            let theme = cx.theme().clone();
            let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
                build_slash_menu(menu, &theme)
            });
            let sub = cx.subscribe(&menu, |this, _menu, _: &DismissEvent, cx| {
                this.close_slash_menu();
                cx.notify();
            });
            self.slash_open = true;
            self.slash_menu = Some(menu);
            self.slash_menu_sub = Some(sub);
            cx.notify();
        } else if !should_open && self.slash_open {
            self.close_slash_menu();
            cx.notify();
        }
    }

    fn close_slash_menu(&mut self) {
        self.slash_open = false;
        self.slash_menu = None;
        self.slash_menu_sub = None;
    }

    /// Switch to a new thread: persist the current one, build/load the new one, re-subscribe, and rebuild the conversation view.
    fn attach_thread(&mut self, new_thread: Entity<Thread>, cx: &mut Context<Self>) {
        save_thread(self.thread.clone(), cx);

        self.thread = new_thread;
        let id = self.thread.read(cx).id.0.clone();
        let messages: Vec<agent::Message> = self.thread.read(cx).messages().to_vec();
        self.conversation = ConversationState::rebuild_from_messages(&messages);
        self.pending_auth = None;
        self.thread_sub = Some(self.subscribe_thread(cx));
        self.sidebar
            .update(cx, |s, cx| s.set_selected(Some(id), cx));
        cx.notify();
    }

    fn start_new_thread(&mut self, cx: &mut Context<Self>) {
        let id = ThreadId(uuid::Uuid::new_v4().to_string());
        let new = Thread::new(id, self.cwd.clone(), cx);
        self.attach_thread(new, cx);
    }

    fn open_thread(&mut self, id: String, cx: &mut Context<Self>) {
        let store = self.sidebar.read(cx).store();
        let Some(loaded) = store.update(cx, |s, cx| s.load_thread(&id, cx)) else {
            return;
        };
        self.attach_thread(loaded, cx);
    }

    fn delete_thread(&mut self, id: String, cx: &mut Context<Self>) {
        let store = self.sidebar.read(cx).store();
        let is_current = self.thread.read(cx).id.0 == id;
        store.update(cx, |s, cx| s.delete_thread(&id, cx));
        if is_current {
            self.start_new_thread(cx);
        }
    }

    pub(crate) fn submit_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input_state.read(cx).value().to_string();
        let attachments = std::mem::take(&mut self.pending_attachments);
        if (text.trim().is_empty() && attachments.is_empty()) || self.thread.read(cx).is_running() {
            self.pending_attachments = attachments;
            return;
        }
        self.input_state
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.close_slash_menu();

        if attachments.is_empty() {
            self.send_user_turn(text, Vec::new(), cx);
            return;
        }

        // Reading attachment bytes is blocking IO; do it off the UI thread, then start the turn.
        cx.spawn(async move |this, cx| {
            let (text, extra) = cx
                .background_spawn(async move {
                    let mut text = text;
                    let mut extra = Vec::new();
                    for att in &attachments {
                        if let Some(content) = load_attachment(att, &mut text) {
                            extra.push(content);
                        }
                    }
                    (text, extra)
                })
                .await;
            this.update(cx, |this, cx| this.send_user_turn(text, extra, cx))
                .ok();
        })
        .detach();
    }

    /// Append the user turn (text plus any image content) to the thread and start the run.
    fn send_user_turn(
        &mut self,
        text: String,
        images: Vec<agent::language_model::MessageContent>,
        cx: &mut Context<Self>,
    ) {
        use agent::language_model::MessageContent;
        self.conversation.push_user(text.clone());
        self.thread.update(cx, |thread, cx| {
            if images.is_empty() {
                thread.insert_user_message(text, cx);
            } else {
                let mut content = Vec::with_capacity(images.len() + 1);
                if !text.trim().is_empty() {
                    content.push(MessageContent::Text(text));
                }
                content.extend(images);
                thread.insert_user_message_with_content(content, cx);
            }
            thread.run_turn(cx);
        });
        // Persist on submit so the sidebar shows the new entry immediately.
        save_thread(self.thread.clone(), cx);
        cx.notify();
    }

    fn toggle_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editor_open {
            self.open_editor(window, cx);
        } else {
            self.close_editor(window, cx);
        }
    }

    /// Open the markdown editor: hide the inline composer and transfer its draft
    /// text into the editor so writing continues there. Submit from the editor
    /// with Cmd-Enter; close with Ctrl-G / Cmd-W to move the draft back.
    fn open_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor_open {
            return;
        }
        // Close any open inline menus so they don't linger behind the hidden footer.
        self.close_slash_menu();
        self.close_plus_menu();
        let draft = self.input_state.read(cx).value().to_string();
        self.editor_open = true;
        self.editor_preview = false;
        self.editor_state.update(cx, |s, cx| {
            s.set_value(draft, window, cx);
            s.focus(window, cx);
        });
        self.input_state
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    /// Close the markdown editor without submitting: move the draft back into the
    /// inline composer and reveal it again.
    fn close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editor_open {
            return;
        }
        let draft = self.editor_state.read(cx).value().to_string();
        self.editor_open = false;
        self.editor_preview = false;
        self.input_state.update(cx, |s, cx| {
            s.set_value(draft, window, cx);
            s.focus(window, cx);
        });
        self.editor_state
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    /// Toggle the right-side composer between plain-text edit and rendered
    /// markdown preview. No-op when the panel is closed.
    fn toggle_editor_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.set_editor_preview(!self.editor_preview, window, cx);
    }

    /// Switch the editor panel to preview (`Write` tab) or rendered markdown
    /// (`Preview` tab). No-op when the panel is closed or already in that mode.
    /// Returning to `Write` focuses the editor so typing works immediately.
    fn set_editor_preview(&mut self, preview: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editor_open || self.editor_preview == preview {
            return;
        }
        self.editor_preview = preview;
        if !preview {
            self.editor_state.update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }

    /// Submit the editor text to the thread, then close the panel and return
    /// focus to the inline input.
    fn submit_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor_state.read(cx).value().to_string();
        if text.trim().is_empty() || self.thread.read(cx).is_running() {
            return;
        }
        self.conversation.push_user(text.clone());
        self.thread.update(cx, |thread, cx| {
            thread.insert_user_message(text, cx);
            thread.run_turn(cx);
        });
        save_thread(self.thread.clone(), cx);
        self.editor_state.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        self.editor_open = false;
        self.editor_preview = false;
        self.input_state.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn model_label(&self, cx: &mut Context<Self>) -> String {
        self.thread
            .read(cx)
            .model()
            .map(|m| m.name().to_string())
            .unwrap_or_else(|| "未配置模型".to_string())
    }

    fn resolve_auth(&mut self, decision: PermissionDecision, cx: &mut Context<Self>) {
        self.pending_ask = None;
        if let Some(auth) = self.pending_auth.take() {
            self.thread.update(cx, |thread, cx| {
                thread.respond_authorization(
                    &auth.id,
                    agent::ToolAuthorizationResponse::Decision(decision),
                    cx,
                );
            });
            cx.notify();
        }
    }

    /// Allocate the per-question `InputState` entities for the pending ask card
    /// on first render. `InputState::new` needs a `Window`, which the event
    /// handler lacks, so creation is deferred to here.
    fn ensure_ask_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ask) = self.pending_ask.as_mut() else {
            return;
        };
        if ask.others.len() == ask.questions.len() {
            return;
        }
        ask.others = (0..ask.questions.len())
            .map(|_| cx.new(|cx| InputState::new(window, cx)))
            .collect();
    }

    /// Toggle an option in the pending ask card. Single-select questions reset
    /// siblings; multi-select toggles in place.
    fn toggle_ask_option(&mut self, qi: usize, oi: usize, cx: &mut Context<Self>) {
        if let Some(ask) = self.pending_ask.as_mut()
            && let Some(sel) = ask.selections.get_mut(qi)
        {
            let multi = ask
                .questions
                .get(qi)
                .map(|q| q.multi_select)
                .unwrap_or(false);
            let prev = sel.get(oi).copied().unwrap_or(false);
            if multi {
                if let Some(slot) = sel.get_mut(oi) {
                    *slot = !*slot;
                }
            } else {
                for s in sel.iter_mut() {
                    *s = false;
                }
                if let Some(slot) = sel.get_mut(oi) {
                    *slot = !prev;
                }
            }
        }
        cx.notify();
    }

    /// Submit the pending ask card: gather answers (per-question "Other" text
    /// overrides option selections) and forward them to the thread.
    fn resolve_ask(&mut self, cx: &mut Context<Self>) {
        let ask = match self.pending_ask.take() {
            Some(a) => a,
            None => return,
        };
        let mut answers: Vec<(String, String)> = Vec::with_capacity(ask.questions.len());
        for (i, q) in ask.questions.iter().enumerate() {
            let other = ask
                .others
                .get(i)
                .map(|s| s.read(cx).value().trim().to_string())
                .unwrap_or_default();
            let answer = if !other.is_empty() {
                other
            } else {
                let sel = ask.selections.get(i).map(|s| s.as_slice()).unwrap_or(&[]);
                let selected: Vec<&str> = q
                    .options
                    .iter()
                    .zip(sel.iter())
                    .filter_map(|(o, &s)| s.then_some(o.label.as_str()))
                    .collect();
                selected.join(", ")
            };
            answers.push((q.question.clone(), answer));
        }
        let id = ask.id.clone();
        self.pending_auth = None;
        self.thread.update(cx, |thread, cx| {
            thread.respond_authorization(
                &id,
                agent::ToolAuthorizationResponse::AskUserQuestion {
                    answers,
                    response: None,
                },
                cx,
            );
        });
        cx.notify();
    }

    /// Abort the current turn.
    pub(crate) fn cancel_turn(&mut self, cx: &mut Context<Self>) {
        self.thread.update(cx, |thread, cx| {
            thread.cancel(cx);
        });
        cx.notify();
    }

    fn render_auth_overlay(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        // AskUserQuestion renders its own card; suppress the generic approval
        // modal while a question card is open (both share the same id).
        if self.pending_ask.is_some() {
            return None;
        }
        let auth = self.pending_auth.as_ref()?;
        let summary = auth.summary.clone();
        let tool_name = auth.tool_name.clone();

        Some(
            gpui::div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.background.opacity(0.6))
                .child(
                    v_flex()
                        .w(px(420.))
                        .p_4()
                        .gap_3()
                        .rounded(theme.radius)
                        .bg(theme.background)
                        .border_1()
                        .border_color(theme.border)
                        .shadow_lg()
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(Icon::new(IconName::Info).small().text_color(theme.warning))
                                .child(
                                    gpui::div()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child("工具调用审批"),
                                ),
                        )
                        .child(
                            gpui::div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(format!("工具：{tool_name}")),
                        )
                        .child(
                            gpui::div()
                                .p_2()
                                .rounded(theme.radius)
                                .bg(theme.secondary)
                                .text_xs()
                                .font_family(theme.mono_font_family.clone())
                                .text_color(theme.foreground)
                                .child(summary),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("auth-deny")
                                        .ghost()
                                        .small()
                                        .label("拒绝")
                                        .on_click(cx.listener({
                                            move |this, _, _, cx| {
                                                this.resolve_auth(PermissionDecision::Deny, cx);
                                            }
                                        })),
                                )
                                .child(
                                    Button::new("auth-allow")
                                        .ghost()
                                        .small()
                                        .label("始终允许")
                                        .on_click(cx.listener({
                                            move |this, _, _, cx| {
                                                this.resolve_auth(
                                                    PermissionDecision::AlwaysAllow,
                                                    cx,
                                                );
                                            }
                                        })),
                                )
                                .child(
                                    Button::new("auth-once")
                                        .primary()
                                        .small()
                                        .label("允许一次")
                                        .on_click(cx.listener({
                                            move |this, _, _, cx| {
                                                this.resolve_auth(
                                                    PermissionDecision::AllowOnce,
                                                    cx,
                                                );
                                            }
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Question card for `AskUserQuestion`. Mirrors the approval overlay's
    /// container; the body renders each question with toggleable option
    /// buttons and a free-form "Other" input.
    fn render_ask_overlay(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let ask = self.pending_ask.as_ref()?;

        let mut card = v_flex()
            .w(px(520.))
            .max_h(px(560.))
            .p_4()
            .gap_3()
            .rounded(theme.radius)
            .bg(theme.background)
            .border_1()
            .border_color(theme.border)
            .shadow_lg()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(IconName::Info).small().text_color(theme.primary))
                    .child(
                        gpui::div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("澄清问题"),
                    ),
            );

        for (qi, q) in ask.questions.iter().enumerate() {
            let sel = ask.selections.get(qi);
            let other = ask.others.get(qi).cloned();
            let mut qblock = v_flex().gap_2().child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Tag::new()
                            .with_variant(TagVariant::Secondary)
                            .small()
                            .child(q.header.clone()),
                    )
                    .child(
                        gpui::div()
                            .text_sm()
                            .text_color(theme.foreground)
                            .child(q.question.clone()),
                    ),
            );
            for (oi, opt) in q.options.iter().enumerate() {
                let selected = sel.and_then(|s| s.get(oi).copied()).unwrap_or(false);
                let mut btn = Button::new(gpui::SharedString::from(format!("ask-{qi}-{oi}")));
                if selected {
                    btn = btn.primary();
                } else {
                    btn = btn.ghost();
                }
                btn = btn.small().label(opt.label.clone()).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.toggle_ask_option(qi, oi, cx);
                    },
                ));
                qblock = qblock.child(
                    h_flex().gap_2().items_start().child(btn).child(
                        gpui::div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(opt.description.clone()),
                    ),
                );
            }
            if let Some(state) = other {
                qblock = qblock.child(
                    v_flex()
                        .gap_1()
                        .child(
                            gpui::div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("其他（自由输入）"),
                        )
                        .child(Input::new(&state)),
                );
            }
            card = card.child(qblock);
        }

        card = card.child(
            h_flex()
                .gap_2()
                .justify_end()
                .child(
                    Button::new("ask-cancel")
                        .ghost()
                        .small()
                        .label("取消")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.resolve_auth(PermissionDecision::Deny, cx);
                        })),
                )
                .child(
                    Button::new("ask-submit")
                        .primary()
                        .small()
                        .label("提交")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.resolve_ask(cx);
                        })),
                ),
        );

        Some(
            gpui::div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.background.opacity(0.6))
                .child(card.into_any_element())
                .into_any_element(),
        )
    }

    /// Cascading model selector using PopupMenu with Provider → Model submenus.
    ///
    /// Closed: a ghost button showing the current model with a chevron.
    /// Open: an absolute-positioned PopupMenu; hovering a Provider row expands
    /// a flyout submenu listing its Models. PopupMenu handles all hover,
    /// click-outside, and keyboard-dismiss behavior internally.
    fn render_model_selector(&mut self, _theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let label = self.model_label(cx);
        let open = self.model_open;

        let trigger = Button::new("model-trigger")
            .ghost()
            .small()
            .label(label)
            .icon(if open {
                IconName::ChevronUp
            } else {
                IconName::ChevronDown
            })
            .on_click(cx.listener(|this, _, window, cx| {
                if this.model_open {
                    this.model_open = false;
                    this.model_menu = None;
                    this.model_menu_sub = None;
                } else {
                    this.model_open = true;
                    let workspace = cx.entity();
                    let menu = PopupMenu::build(window, cx, |menu, window, cx| {
                        Self::build_model_popup_menu(menu, workspace, window, cx)
                    });
                    let sub = cx.subscribe(
                        &menu,
                        |this: &mut Workspace,
                         _menu: Entity<PopupMenu>,
                         _: &DismissEvent,
                         cx: &mut Context<Workspace>| {
                            this.model_open = false;
                            this.model_menu = None;
                            this.model_menu_sub = None;
                            cx.notify();
                        },
                    );
                    this.model_menu = Some(menu);
                    this.model_menu_sub = Some(sub);
                }
                cx.notify();
            }));

        if !open {
            return trigger.into_any_element();
        }

        let menu = self
            .model_menu
            .clone()
            .expect("model_menu exists when open");

        gpui::div()
            .relative()
            .child(trigger)
            .child(
                // PopupMenu has its own bg/border/shadow and on_mouse_down_out.
                // `.occlude()` renders the dropdown above all non-occluded elements
                // (footer borders, message list, etc.).
                gpui::div()
                    .id("model-dropdown")
                    .absolute()
                    .bottom_full()
                    .right_0()
                    .occlude()
                    .child(menu),
            )
            .into_any_element()
    }

    /// WireApi → Tag variant + label mapping for the model menu.
    fn wire_tag_variant(wire: WireApi) -> (TagVariant, &'static str) {
        match wire {
            WireApi::Anthropic => (TagVariant::Primary, "Anthropic"),
            WireApi::Responses => (TagVariant::Info, "Responses"),
            WireApi::Completions => (TagVariant::Warning, "Completions"),
            WireApi::Unavailable => (TagVariant::Secondary, "N/A"),
        }
    }

    /// Cascading model menu grouped by provider; each model row shows a wire-api Tag.
    fn build_model_popup_menu(
        menu: PopupMenu,
        workspace: Entity<Workspace>,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        let mut providers: Vec<(String, Vec<agent::language_model::AnyLanguageModel>)> = Vec::new();
        for m in registry::global().models() {
            let prov = m.provider_name();
            if let Some(last) = providers.last_mut()
                && last.0 == prov
            {
                last.1.push(m.clone());
            } else {
                providers.push((prov, vec![m.clone()]));
            }
        }

        let mut menu = menu;
        if providers.is_empty() {
            menu = menu.item(PopupMenuItem::Label("No models configured".into()));
        }
        for (prov_name, models) in providers {
            let ws = workspace.clone();
            menu = menu.submenu(prov_name, window, cx, move |submenu, _window, _cx| {
                let mut submenu = submenu;
                for m in &models {
                    let model_id = m.id();
                    let model_name = m.name().to_string();
                    let wire = m.wire_api();
                    let (variant, label) = Self::wire_tag_variant(wire);
                    let ws = ws.clone();
                    submenu = submenu.item(
                        PopupMenuItem::element(move |_window, _cx| {
                            h_flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    Tag::new()
                                        .with_variant(variant)
                                        .outline()
                                        .small()
                                        .child(label),
                                )
                                .child(model_name.clone())
                        })
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            ws.update(cx, |this, cx| {
                                if let Some(m) = registry::global().get_model(model_id.as_ref()) {
                                    this.thread.update(cx, |t, cx| t.set_model(m, cx));
                                }
                            });
                        }),
                    );
                }
                submenu
            });
        }
        menu
    }

    /// Composer row: a rounded container with the `+` menu button, `Input`, and a circular send/stop button.
    fn render_composer(
        &mut self,
        running: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        h_flex()
            .w_full()
            .items_end()
            .gap_2()
            .p_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(self.render_plus_button(cx))
            .child(gpui::div().flex_1().child(Input::new(&self.input_state)))
            .child(self.render_send_button(running, cx))
            .into_any_element()
    }

    /// The composer `+` button and its popup menu (Codex-style "add / plugins").
    fn render_plus_button(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let trigger = Button::new("composer-plus")
            .ghost()
            .icon(IconName::Plus)
            .on_click(cx.listener(|this, _, window, cx| {
                if this.plus_open {
                    this.close_plus_menu();
                } else {
                    this.open_plus_menu(window, cx);
                }
                cx.notify();
            }));

        if !self.plus_open {
            return trigger.into_any_element();
        }
        let Some(menu) = self.plus_menu.clone() else {
            return trigger.into_any_element();
        };
        gpui::div()
            .relative()
            .child(trigger)
            .child(
                gpui::div()
                    .id("plus-dropdown")
                    .absolute()
                    .bottom_full()
                    .left_0()
                    .occlude()
                    .child(menu),
            )
            .into_any_element()
    }

    fn open_plus_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let theme = cx.theme().clone();
        let ws = cx.entity();
        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            let ws = ws.clone();
            build_plus_menu(menu, &theme, move |window, cx| {
                ws.update(cx, |this, cx| {
                    this.close_plus_menu();
                    this.pick_files(window, cx);
                    cx.notify();
                });
            })
        });
        let sub = cx.subscribe(&menu, |this, _menu, _: &DismissEvent, cx| {
            this.close_plus_menu();
            cx.notify();
        });
        self.plus_open = true;
        self.plus_menu = Some(menu);
        self.plus_menu_sub = Some(sub);
    }

    fn close_plus_menu(&mut self) {
        self.plus_open = false;
        self.plus_menu = None;
        self.plus_menu_sub = None;
    }

    /// Open the native file picker and add chosen paths as pending attachments.
    fn pick_files(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = paths.await {
                this.update(cx, |this, cx| {
                    for p in paths {
                        this.pending_attachments.push(PendingAttachment::new(p));
                    }
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    /// Circular icon-only send/stop button.
    ///
    /// Reuses `Button` for built-in focus ring, keyboard activation, and disabled handling;
    /// `.rounded(px(16.))` renders the button as a 32px disc.
    fn render_send_button(&self, running: bool, cx: &mut Context<Self>) -> AnyElement {
        Button::new("send-btn")
            .icon(if running {
                IconName::Pause
            } else {
                IconName::ArrowUp
            })
            .when(running, |b| b.danger())
            .when(!running, |b| b.primary())
            .rounded(px(16.))
            .on_click(cx.listener(|this, _, window, cx| {
                if this.thread.read(cx).is_running() {
                    this.cancel_turn(cx);
                } else {
                    this.submit_input(window, cx);
                }
            }))
            .into_any_element()
    }

    /// The `⁄` command menu overlaid above the composer while `slash_open`.
    fn render_slash_overlay(&self) -> Option<AnyElement> {
        let menu = self.slash_menu.clone()?;
        Some(
            centered(
                gpui::div()
                    .id("slash-dropdown")
                    .absolute()
                    .bottom_full()
                    .left_0()
                    .occlude()
                    .child(menu),
            )
            .into_any_element(),
        )
    }

    /// Pending-attachment chips shown above the composer, each removable.
    fn render_attachments(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.pending_attachments.is_empty() {
            return None;
        }
        let on_remove = cx.listener(|this, ix: &usize, _window, cx| {
            if *ix < this.pending_attachments.len() {
                this.pending_attachments.remove(*ix);
                cx.notify();
            }
        });
        Some(
            centered(render_attachment_chips(
                &self.pending_attachments,
                theme,
                move |ix, window, cx| on_remove(&ix, window, cx),
            ))
            .into_any_element(),
        )
    }

    /// Chip row: a plain `cwd` label on the left, model selector on the right.
    fn render_chip_row(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        // Self-contained centering (not `centered()` helper) so the absolute-positioned
        // model dropdown is not clipped by any `max_w` ancestor.
        h_flex()
            .w_full()
            .justify_center()
            .child(
                h_flex()
                    .w_full()
                    .max_w(px(crate::views::CONTENT_MAX_W))
                    .items_center()
                    .child(self.render_cwd_chip(theme))
                    .child(gpui::div().flex_1())
                    .child(
                        gpui::div()
                            .flex_shrink_0()
                            .child(self.render_model_selector(theme, cx)),
                    ),
            )
            .into_any_element()
    }

    /// Static label of the current working directory's basename.
    ///
    /// Rendered as a plain `div` (not `Button`) until a directory-switcher popover exists; an
    /// unclickable `Button` would invite clicks that do nothing.
    fn render_cwd_chip(&self, theme: &Theme) -> AnyElement {
        let name = self
            .cwd
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| *s != ".")
            .unwrap_or("project");
        gpui::div()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .bg(theme.secondary)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(name.to_string())
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let model_label = self.model_label(cx);
        let running = self.thread.read(cx).is_running();

        self.ensure_ask_inputs(_window, cx);

        let items: Vec<_> = self
            .conversation
            .items()
            .iter()
            .enumerate()
            .map(|(ix, item)| render_item(item, ix, &model_label, &theme))
            .collect();

        let overlay = self
            .render_ask_overlay(&theme, cx)
            .or_else(|| self.render_auth_overlay(&theme, cx));

        let editor_open = self.editor_open;
        let editor_preview = self.editor_preview;
        let editor_width = self.editor_width;
        // The inline composer and the markdown editor are mutually exclusive: while
        // the editor pane is open the footer is hidden and the draft lives in the
        // editor (moved there on open, moved back on close).
        let footer = if !editor_open {
            Some(
                v_flex()
                    .w_full()
                    .py_2()
                    .gap_2()
                    .relative()
                    .children(self.render_slash_overlay())
                    .children(self.render_attachments(&theme, cx))
                    .child(centered(self.render_composer(running, &theme, cx)))
                    .child(self.render_chip_row(&theme, cx)),
            )
        } else {
            None
        };
        // No chrome on the panel: Ctrl-G closes, Cmd-Enter sends, Cmd-Shift-P
        // toggles preview — all keyboard-driven per the no-button constraint.
        // The divider is the visual separator and the drag handle for resizing.
        let editor_divider = gpui::div()
            .id("editor-divider")
            .w(px(EDITOR_DIVIDER_WIDTH))
            .h_full()
            .flex_shrink_0()
            .relative()
            .cursor(CursorStyle::ResizeLeftRight)
            .child(
                gpui::div()
                    .absolute()
                    .left(px(2.5))
                    .w(px(1.))
                    .h_full()
                    .bg(theme.border),
            )
            .on_drag(DraggedEditorDivider, |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DraggedEditorDivider)
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, e: &MouseUpEvent, _, cx| {
                    // Double-click resets the pane to its default width.
                    if e.click_count >= 2 {
                        this.editor_width = px(EDITOR_PANEL_WIDTH);
                        cx.notify();
                    }
                }),
            );
        let editor_pane = v_flex()
            .w(editor_width)
            .h_full()
            .flex_shrink_0()
            .bg(theme.background)
            .child(
                h_flex().w_full().px_2().pt_1().child(
                    TabBar::new("editor-tabs")
                        .underline()
                        .small()
                        .selected_index(if editor_preview { 1 } else { 0 })
                        .on_click(cx.listener(|this, ix: &usize, window, cx| {
                            this.set_editor_preview(*ix == 1, window, cx);
                        }))
                        .child("Write")
                        .child("Preview"),
                ),
            )
            .child(
                gpui::div()
                    .id("editor-content")
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(if editor_preview {
                        // The TextView caches its parsed document per element id and early-returns
                        // in set_text when the source is unchanged, so resizing the pane would leave
                        // the laid-out tree stale. Derive the id from the quantized pane width so a
                        // width change mounts a fresh state and re-parses at the new wrap width.
                        let preview_id =
                            format!("editor-preview-{}", (f32::from(editor_width) as i32) / 8);
                        v_flex()
                            .h_full()
                            .p_4()
                            .text_sm()
                            .child(
                                TextView::markdown(
                                    preview_id,
                                    self.editor_state.read(cx).value().to_string(),
                                )
                                .selectable(true)
                                .scrollable(true)
                                .h_full(),
                            )
                            .into_any_element()
                    } else {
                        Input::new(&self.editor_state)
                            .size_full()
                            .appearance(false)
                            .into_any_element()
                    }),
            );

        h_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(|this, _: &crate::ToggleEditor, window, cx| {
                this.toggle_editor(window, cx);
            }))
            .on_action(
                cx.listener(|this, _: &crate::ToggleEditorPreview, window, cx| {
                    this.toggle_editor_preview(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &crate::CloseEditor, window, cx| {
                this.close_editor(window, cx);
            }))
            // Left sidebar
            .child(self.sidebar.clone())
            // Main column
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .relative()
                    // Title bar (TitleBar handles window dragging via start_window_move)
                    .child(
                        TitleBar::new()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(Icon::new(IconName::Bot).small())
                                    .child(
                                        gpui::div()
                                            .font_weight(gpui::FontWeight::SEMIBOLD)
                                            .child("manox"),
                                    ),
                            )
                            .child(h_flex()),
                    )
                    // Message list (centered, width-capped, scrollable).
                    // `track_scroll` lets us read offset/max and call `scroll_to_bottom`
                    // from event handlers. `on_scroll_wheel` reacts to manual scrolls:
                    // if the user drags the viewport away from the bottom, disengage
                    // auto-follow until they scroll back down.
                    .child(
                        v_flex()
                            .id("messages")
                            .flex_1()
                            .py_5()
                            .gap_5()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .on_scroll_wheel(cx.listener(
                                |this, ev: &ScrollWheelEvent, _window, _cx| {
                                    let line_height = px(20.);
                                    let dy = ev.delta.pixel_delta(line_height).y;
                                    if dy < px(0.) {
                                        // User wheeled up (away from bottom):
                                        // stop following. The check runs before
                                        // gpui's internal scroll handler applies
                                        // the offset, so the offset we read here
                                        // is still the pre-scroll value; treating
                                        // upward intent as "leave" keeps the user
                                        // from being yanked back by the next
                                        // content delta.
                                        this.stick_to_bottom = false;
                                    } else if Self::is_at_bottom(&this.scroll_handle) {
                                        // Wheeled down or stationary; re-engage
                                        // only when the viewport is parked at the
                                        // bottom within `STICK_THRESHOLD`.
                                        this.stick_to_bottom = true;
                                    }
                                    // Wheel-down while not at bottom is just the
                                    // user scrolling through history; leave the
                                    // flag alone so re-engagement happens the
                                    // moment they reach the bottom.
                                },
                            ))
                            .children(items.into_iter().map(centered)),
                    )
                    // Separator line (standalone div so the PopupMenu inside
                    // the footer can render on top of it without z-order issues).
                    .child(gpui::div().w_full().h(px(1.)).bg(theme.border))
                    // Input area: pending attachments + composer + chip row, with the slash
                    // menu overlaid above the composer. Hidden while the markdown editor pane
                    // is open — the draft is moved into the editor on open and back on close.
                    .children(footer)
                    // Approval overlay (if any)
                    .children(overlay),
            )
            .when(editor_open, |this| {
                this.child(editor_divider).child(editor_pane)
            })
            .on_drag_move(cx.listener(
                |this, e: &DragMoveEvent<DraggedEditorDivider>, _window, cx| {
                    // The root fills the window, so its right edge is the
                    // window's right edge and the editor pane's width is the
                    // distance from the cursor to that edge. Clamp both to a
                    // minimum and to leave the main column at least
                    // `MAIN_MIN_WIDTH` (sidebar + main + divider sit left of
                    // the editor), so dragging wide never overflows the window
                    // or collapses the conversation column.
                    let new_w = e.bounds.right() - e.event.position.x;
                    let dynamic_max = e.bounds.size.width
                        - px(SIDEBAR_WIDTH)
                        - px(EDITOR_DIVIDER_WIDTH)
                        - px(MAIN_MIN_WIDTH);
                    let max_w = dynamic_max
                        .min(px(EDITOR_MAX_WIDTH))
                        .max(px(EDITOR_MIN_WIDTH));
                    this.editor_width = new_w.clamp(px(EDITOR_MIN_WIDTH), max_w);
                    cx.notify();
                },
            ))
    }
}

/// Parse an `AskUserQuestion` tool input into a `PendingAsk`. The per-question
/// `InputState` entities are allocated lazily on first render (they need a
/// `Window`, which the event handler lacks). Returns `None` when the input is
/// malformed (the generic approval overlay then takes over as a fallback).
fn parse_pending_ask(id: String, input: serde_json::Value) -> Option<PendingAsk> {
    let questions = input.get("questions")?.as_array()?;
    // An empty questions array renders a button-only card with no way to
    // answer; fall back to the generic approval overlay instead.
    if questions.is_empty() {
        return None;
    }
    let mut parsed: Vec<AskQuestion> = Vec::with_capacity(questions.len());
    let mut selections: Vec<Vec<bool>> = Vec::with_capacity(questions.len());
    for q in questions {
        let question = q.get("question")?.as_str()?.to_string();
        let header = q
            .get("header")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let multi_select = q
            .get("multiSelect")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut opts: Vec<AskOption> = Vec::new();
        if let Some(arr) = q.get("options").and_then(|v| v.as_array()) {
            for o in arr {
                let label = o
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let description = o
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                opts.push(AskOption { label, description });
            }
        }
        selections.push(vec![false; opts.len()]);
        parsed.push(AskQuestion {
            question,
            header,
            multi_select,
            options: opts,
        });
    }
    Some(PendingAsk {
        id,
        questions: parsed,
        selections,
        others: Vec::new(),
    })
}
