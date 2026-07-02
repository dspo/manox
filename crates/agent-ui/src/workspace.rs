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
use agent::provider::registry;
use agent::{PermissionDecision, Thread, ThreadEvent, ThreadId, save_thread};
use gpui::{AnyElement, Context, Entity, Focusable, Render, Subscription, Window, prelude::*, px};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme, TitleBar,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    list::ListItem,
    popover::Popover,
    tree::{TreeItem, TreeState, tree},
    v_flex,
};
use gpui_rich_text::{RichTextEditor, RichTextState};

use crate::conversation::ConversationState;
use crate::editor::try_apply_markdown_shortcut;
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
    /// Right-side WYSIWYG markdown composer; opened via the `ToggleEditor` shortcut.
    editor_state: Entity<RichTextState>,
    editor_open: bool,
    pending_auth: Option<PendingAuth>,
    model_open: bool,
    /// Two-level tree (Provider → Model) for the model selector popover.
    tree_state: Entity<TreeState>,
    thread_sub: Option<Subscription>,
    sidebar_sub: Option<Subscription>,
    input_sub: Option<Subscription>,
    editor_sub: Option<Subscription>,
}

/// Right-side WYSIWYG composer width. Wide enough for rendered markdown
/// (headings, lists) alongside the 1100px window without crowding the
/// conversation thread.
const EDITOR_PANEL_WIDTH: f32 = 640.;

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let thread = {
            let id = ThreadId(uuid::Uuid::new_v4().to_string());
            Thread::new(id, cwd.clone(), cx)
        };

        let input_state = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("markdown")
                .line_number(false)
                .folding(false)
                .rows(3)
                .submit_on_enter(true)
                .placeholder("给 agent 发消息…（Enter 发送，Shift+Enter 换行，Ctrl-G 打开编辑器）")
        });

        let editor_state = cx.new(|cx| RichTextState::new(window, cx).default_value(""));

        let sidebar = cx.new(Sidebar::new);

        let tree_state = Self::build_tree_state(
            cx.new(|cx| TreeState::new(cx)),
            thread.read(cx).model().map(|m| m.provider_name()),
            cx,
        );

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
            tree_state,
            thread_sub: None,
            sidebar_sub: None,
            input_sub: None,
            editor_sub: None,
        };
        ws.thread_sub = Some(ws.subscribe_thread(cx));
        ws.sidebar_sub = Some(ws.subscribe_sidebar(cx));
        ws.input_sub = Some(ws.subscribe_input(window, cx));
        ws.editor_sub = Some(ws.observe_editor(window, cx));
        let id = ws.thread.read(cx).id.0.clone();
        ws.sidebar.update(cx, |s, cx| s.set_selected(Some(id), cx));
        ws
    }

    /// Group registry models by provider into a two-level tree.
    /// The provider holding the current model is expanded by default.
    fn build_tree_state(
        state: Entity<TreeState>,
        expanded_provider: Option<String>,
        cx: &mut Context<Self>,
    ) -> Entity<TreeState> {
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

        let items: Vec<TreeItem> = providers
            .into_iter()
            .map(|(provider, models)| {
                let is_open = expanded_provider.as_deref() == Some(provider.as_str());
                let mut node = TreeItem::new(provider.clone(), provider).expanded(is_open);
                for m in models {
                    node = node.child(TreeItem::new(m.id(), m.name()));
                }
                node
            })
            .collect();

        state.update(cx, |s, cx| s.set_items(items, cx));
        state
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
        cx.subscribe(&sidebar, |this, _sidebar, ev: &SidebarEvent, cx| match ev {
            SidebarEvent::NewThread => this.start_new_thread(cx),
            SidebarEvent::OpenThread(id) => this.open_thread(id.clone(), cx),
            SidebarEvent::DeleteThread(id) => this.delete_thread(id.clone(), cx),
        })
    }

    fn subscribe_input(&self, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        let input = self.input_state.clone();
        cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { shift, .. } = ev
                && !shift
            {
                this.submit_input(window, cx);
            }
        })
    }

    /// Observe the right-side editor: every state change fires the markdown
    /// shortcut detector (see `editor::try_apply_markdown_shortcut`).
    fn observe_editor(&self, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        let editor = self.editor_state.clone();
        cx.observe_in(&editor, window, move |_this, editor, window, cx| {
            // The closure receives a `&mut Window` per call; pass it through to
            // the entity via `Entity::update`, capturing the reborrowed window.
            // `Context<Workspace>` doesn't implement `VisualContext`, so we
            // can't use `update_in` here.
            let window: &mut Window = window;
            editor.update(cx, |state, cx| {
                try_apply_markdown_shortcut(state, window, cx);
            });
        })
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

    fn submit_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.input_state.read(cx).value().to_string();
        if text.trim().is_empty() || self.thread.read(cx).is_running() {
            return;
        }
        self.conversation.push_user(text.clone());
        self.thread.update(cx, |thread, cx| {
            thread.insert_user_message(text, cx);
            thread.run_turn(cx);
        });
        // Persist on submit so the sidebar shows the new entry immediately.
        save_thread(self.thread.clone(), cx);
        self.input_state
            .update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();
    }

    fn toggle_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor_open = !self.editor_open;
        if self.editor_open {
            self.editor_state.focus_handle(cx).focus(window, cx);
        } else {
            self.input_state.update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }

    /// Submit the editor text to the thread, then close the panel and return
    /// focus to the inline input.
    fn submit_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor_state.read(cx).markdown();
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
        self.input_state.update(cx, |s, cx| s.focus(window, cx));
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

    /// Model selector: a button showing the current model, whose popover hosts a
    /// two-level tree (Provider folders → Model leaves). Clicking a leaf picks
    /// the model and closes the popover (wired via the leaf `on_click`).
    fn render_model_selector(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let label = self.model_label(cx);
        let current_id = self.thread.read(cx).model().map(|m| m.id());
        let open = self.model_open;
        let theme_clone = theme.clone();
        let view = cx.entity();

        let state = self.tree_state.clone();
        let tree = tree(&state, move |ix, entry, selected, _window, _cx| {
            let depth = entry.depth();
            let item = entry.item();
            let is_folder = entry.is_folder();
            let is_current = !is_folder && current_id.as_deref() == Some(item.id.as_ref());

            let chevron = if is_folder {
                Some(Icon::new(if entry.is_expanded() {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                }))
            } else {
                None
            };

            let mut li = ListItem::new(ix)
                .w_full()
                .rounded(theme_clone.radius)
                .px_2()
                .pl(px(16.) * depth as f32 + px(4.))
                .py_1p5()
                .gap_2()
                .items_center()
                .selected(selected || is_current)
                .when(is_folder, |this| {
                    this.font_weight(gpui::FontWeight::SEMIBOLD)
                });

            if let Some(ch) = chevron {
                li = li.child(ch.xsmall().text_color(theme_clone.muted_foreground));
            } else {
                li = li.child(gpui::div().w(px(16.)));
            }

            li = li.child(
                gpui::div()
                    .flex_1()
                    .text_sm()
                    .text_color(if is_current {
                        theme_clone.accent
                    } else {
                        theme_clone.foreground
                    })
                    .child(item.label.clone()),
            );

            // Leaves: pick the model and close on click (mouse only, so keyboard
            // navigation within the tree does not auto-select).
            if !is_folder {
                let id = item.id.clone();
                let view = view.clone();
                li = li.on_click(move |_, _window, cx: &mut gpui::App| {
                    view.update(cx, |this, cx| {
                        if let Some(m) = registry::global().get_model(id.as_ref()) {
                            this.thread.update(cx, |t, cx| t.set_model(m, cx));
                        }
                        this.model_open = false;
                        cx.notify();
                    });
                });
            }

            li
        })
        .w(px(300.))
        .max_h(px(440.));

        Popover::new("model-selector")
            .open(open)
            .on_open_change(cx.listener(|this, open: &bool, _window, cx| {
                this.model_open = *open;
                cx.notify();
            }))
            .trigger(
                Button::new("model-trigger")
                    .ghost()
                    .small()
                    .label(label)
                    .icon(IconName::ChevronDown),
            )
            .child(tree)
            .into_any_element()
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
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let model_label = self.model_label(cx);
        let running = self.thread.read(cx).is_running();
        let model_selector = self.render_model_selector(&theme, cx);

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
            .w(px(EDITOR_PANEL_WIDTH))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(RichTextEditor::new(&self.editor_state).h_full());

        h_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_action(cx.listener(|this, _: &crate::ToggleEditor, window, cx| {
                this.toggle_editor(window, cx);
            }))
            .on_action(cx.listener(|this, _: &crate::SubmitEditor, window, cx| {
                this.submit_editor(window, cx);
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
                    // Input area: statusline (model selector + running indicator) + editor row
                    .child(
                        v_flex()
                            .w_full()
                            .py_2()
                            .gap_2()
                            .border_t_1()
                            .border_color(theme.border)
                            // Statusline: model chip on the left, running indicator on the right
                            .child(centered(
                                h_flex()
                                    .w_full()
                                    .items_center()
                                    .child(model_selector)
                                    .child(gpui::div().flex_1())
                                    .when(running, |this| {
                                        this.child(
                                            h_flex()
                                                .gap_1()
                                                .items_center()
                                                .child(
                                                    Icon::new(IconName::LoaderCircle)
                                                        .xsmall()
                                                        .text_color(theme.muted_foreground),
                                                )
                                                .child(
                                                    gpui::div()
                                                        .text_xs()
                                                        .text_color(theme.muted_foreground)
                                                        .child("思考中…"),
                                                ),
                                        )
                                    }),
                            ))
                            // Editor row
                            .child(centered(
                                h_flex()
                                    .w_full()
                                    .gap_2()
                                    .items_end()
                                    .child(
                                        gpui::div().flex_1().child(Input::new(&self.input_state)),
                                    )
                                    .child(if running {
                                        Button::new("stop")
                                            .ghost()
                                            .small()
                                            .label("停止")
                                            .icon(IconName::Pause)
                                            .on_click(cx.listener(|this, _, _window, cx| {
                                                this.cancel_turn(cx);
                                            }))
                                            .into_any_element()
                                    } else {
                                        Button::new("send")
                                            .primary()
                                            .small()
                                            .label("发送")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.submit_input(window, cx);
                                            }))
                                            .into_any_element()
                                    }),
                            )),
                    )
                    // Approval overlay (if any)
                    .children(overlay),
            )
            .when(editor_open, |this| this.child(editor_panel))
    }
}
