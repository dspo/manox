//! Top-level workspace view.
//!
//! Holds `Entity<agent::Thread>` + `Entity<Sidebar>`; `cx.subscribe` handles:
//! - `ThreadEvent`: text/thinking/tool deltas go to `ConversationState`; `ToolCallAuthorization` opens an approval overlay;
//!   the terminal `Stop` (non-ToolUse) triggers `save_thread`.
//! - `SidebarEvent`: new conversation / open history / delete.
//!
//! Enter in the input box → append a user message + run_turn + persist (the sidebar shows the new entry immediately).

use std::path::PathBuf;

use agent::{PermissionDecision, Thread, ThreadEvent, ThreadId, save_thread};
use agent::language_model::StopReason;
use agent::provider::config::WireApi;
use agent::provider::registry;
use gpui::{AnyElement, Context, DismissEvent, Entity, Render, Subscription, Window, prelude::*, px};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    input::{Input, InputEvent, InputState},
    menu::{PopupMenu, PopupMenuItem},
    tag::{Tag, TagVariant},
    h_flex, v_flex, TitleBar,
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

pub struct Workspace {
    cwd: PathBuf,
    thread: Entity<Thread>,
    sidebar: Entity<Sidebar>,
    conversation: ConversationState,
    input_state: Entity<InputState>,
    /// Right-side markdown composer; opened via the `ToggleEditor` shortcut.
    editor_state: Entity<InputState>,
    editor_open: bool,
    pending_auth: Option<PendingAuth>,
    model_open: bool,
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
                .rows(4)
                .submit_on_enter(true)
                .placeholder("输入消息…")
        });

        let editor_state = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("markdown")
                .line_number(false)
                .folding(false)
                .rows(20)
                .submit_on_enter(false)
                .placeholder("在此撰写较长的 Markdown 消息，点「发送」提交并关闭…")
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
            pending_auth: None,
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
        };
        ws.thread_sub = Some(ws.subscribe_thread(cx));
        ws.sidebar_sub = Some(ws.subscribe_sidebar(cx));
        ws.input_sub = Some(ws.subscribe_input(window, cx));
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
                    ..
                } => {
                    this.pending_auth = Some(PendingAuth {
                        id: id.clone(),
                        tool_name: tool_name.clone(),
                        summary: summary.clone(),
                    });
                    cx.notify();
                }
                ThreadEvent::Stop(reason) => {
                    this.conversation.apply(ev);
                    // Persist on terminal state (not the ToolUse mid-state).
                    if !matches!(reason, StopReason::ToolUse) {
                        save_thread(this.thread.clone(), cx);
                    }
                    cx.notify();
                }
                _ => {
                    this.conversation.apply(ev);
                    cx.notify();
                }
            }
        })
    }

    fn subscribe_sidebar(&self, cx: &mut Context<Self>) -> Subscription {
        let sidebar = self.sidebar.clone();
        cx.subscribe(&sidebar, |this, _sidebar, ev: &SidebarEvent, cx| {
            match ev {
                SidebarEvent::NewThread => this.start_new_thread(cx),
                SidebarEvent::OpenThread(id) => this.open_thread(id.clone(), cx),
                SidebarEvent::DeleteThread(id) => this.delete_thread(id.clone(), cx),
            }
        })
    }

    fn subscribe_input(&self, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        let input = self.input_state.clone();
        cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
            match ev {
                InputEvent::PressEnter { shift, .. } if !shift => this.submit_input(window, cx),
                InputEvent::Change => this.sync_slash_menu(window, cx),
                _ => {}
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
        let messages: Vec<agent::Message> = self
            .thread
            .read(cx)
            .messages()
            .to_vec();
        self.conversation = ConversationState::rebuild_from_messages(&messages);
        self.pending_auth = None;
        self.thread_sub = Some(self.subscribe_thread(cx));
        self.sidebar.update(cx, |s, cx| s.set_selected(Some(id), cx));
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

    fn submit_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
            this.update(cx, |this, cx| this.send_user_turn(text, extra, cx)).ok();
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
        self.editor_open = !self.editor_open;
        if self.editor_open {
            self.editor_state
                .update(cx, |s, cx| s.focus(window, cx));
        } else {
            self.input_state
                .update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }

    /// Submit the composer text to the thread, then close the panel and return
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
        self.editor_state
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.editor_open = false;
        self.input_state
            .update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn model_label(&self, cx: &mut Context<Self>) -> String {
        self.thread
            .read(cx)
            .model()
            .map(|m| m.name().to_string())
            .unwrap_or_else(|| "未配置模型".to_string())
    }

    fn resolve_auth(&mut self, decision: PermissionDecision, cx: &mut Context<Self>) {
        if let Some(auth) = self.pending_auth.take() {
            self.thread.update(cx, |thread, cx| {
                thread.respond_authorization(&auth.id, decision, cx);
            });
            cx.notify();
        }
    }

    /// Abort the current turn.
    fn cancel_turn(&mut self, cx: &mut Context<Self>) {
        self.thread.update(cx, |thread, cx| {
            thread.cancel(cx);
        });
        cx.notify();
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
            .icon(if open { IconName::ChevronUp } else { IconName::ChevronDown })
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
                    let sub = cx.subscribe(&menu, |this: &mut Workspace, _menu: Entity<PopupMenu>, _: &DismissEvent, cx: &mut Context<Workspace>| {
                        this.model_open = false;
                        this.model_menu = None;
                        this.model_menu_sub = None;
                        cx.notify();
                    });
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
        let mut providers: Vec<(String, Vec<agent::language_model::AnyLanguageModel>)> =
            Vec::new();
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
                                if let Some(m) =
                                    registry::global().get_model(model_id.as_ref())
                                {
                                    this.thread
                                        .update(cx, |t, cx| t.set_model(m, cx));
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

    fn render_auth_overlay(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
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
                                .font_family("monospace")
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

    /// Composer row: a rounded container with the `+` menu button, `Input`, and a circular send/stop button.
    fn render_composer(&mut self, running: bool, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
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
            .icon(if running { IconName::Pause } else { IconName::ArrowUp })
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
        Some(centered(render_attachment_chips(
            &self.pending_attachments,
            theme,
            move |ix, window, cx| on_remove(&ix, window, cx),
        )).into_any_element())
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

        let items: Vec<_> = self
            .conversation
            .items()
            .iter()
            .enumerate()
            .map(|(ix, item)| render_item(item, ix, &model_label, &theme))
            .collect();

        let overlay = self.render_auth_overlay(&theme, cx);

        let editor_open = self.editor_open;
        let editor_panel = v_flex()
            .w(px(440.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.background)
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .child(
                        gpui::div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child("编辑器"),
                    )
                    .child(
                        Button::new("editor-close")
                            .ghost()
                            .small()
                            .icon(IconName::Close)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.toggle_editor(window, cx);
                            })),
                    ),
            )
            .child(
                gpui::div()
                    .flex_1()
                    .min_h_0()
                    .child(Input::new(&self.editor_state)),
            )
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(if running {
                        Button::new("editor-stop")
                            .ghost()
                            .small()
                            .label("停止")
                            .icon(IconName::Pause)
                            .on_click(cx.listener(|this, _, _window, cx| {
                                this.cancel_turn(cx);
                            }))
                            .into_any_element()
                    } else {
                        Button::new("editor-send")
                            .primary()
                            .small()
                            .label("发送")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_editor(window, cx);
                            }))
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
                    // Message list (centered, width-capped, scrollable)
                    .child(
                        v_flex()
                            .id("messages")
                            .flex_1()
                            .py_5()
                            .gap_5()
                            .overflow_y_scroll()
                            .children(items.into_iter().map(centered)),
                    )
                    // Separator line (standalone div so the PopupMenu inside
                    // the footer can render on top of it without z-order issues).
                    .child(
                        gpui::div()
                            .w_full()
                            .h(px(1.))
                            .bg(theme.border),
                    )
                    // Input area: pending attachments + composer + chip row, with the slash
                    // menu overlaid above the composer.
                    .child(
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
                    // Approval overlay (if any)
                    .children(overlay),
            )
            .when(editor_open, |this| this.child(editor_panel))
    }
}
