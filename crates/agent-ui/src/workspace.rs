//! Top-level workspace view.
//!
//! Holds `Entity<agent::Thread>` + `Entity<Sidebar>`; `cx.subscribe` handles:
//! - `ThreadEvent`: text/thinking/tool deltas go to `ConversationState`; `ToolCallAuthorization` opens an approval overlay;
//!   the terminal `Stop` (non-ToolUse) triggers `save_thread`.
//! - `SidebarEvent`: new conversation / open history / delete.
//!
//! Enter in the input box → append a user message + run_turn + persist (the sidebar shows the new entry immediately).

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use agent::language_model::StopReason;
use agent::provider::WireApi;
use agent::provider::registry;
use agent::{PermissionDecision, Thread, ThreadEvent, ThreadId, save_thread};
use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, CursorStyle, DismissEvent, DragMoveEvent,
    Entity, FollowMode, ListAlignment, ListSizingBehavior, ListState, MouseButton, MouseUpEvent,
    Pixels, Render, Subscription, Window, ease_out_quint, list, prelude::*, px,
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

use crate::OpenSettings;
use crate::conversation::{ApplyOutcome, ConversationState};
use crate::views::centered;
use crate::views::composer_menu::{
    PendingAttachment, build_plus_menu, build_slash_menu, load_attachment, render_attachment_chips,
};
use crate::views::settings::{SettingsEvent, SettingsView};
use crate::views::sidebar::{Sidebar, SidebarEvent};

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
    conversation: Entity<ConversationState>,
    pub(crate) input_state: Entity<InputState>,
    /// Right-side markdown composer; opened via the `ToggleEditor` shortcut.
    /// Plain-text edit mode by default; `ToggleEditorPreview` switches to a
    /// rendered markdown preview (gpui-component `TextView::markdown`).
    editor_state: Entity<InputState>,
    editor_open: bool,
    editor_preview: bool,
    /// Editor pane width, driven by dragging the divider. In-memory only.
    editor_width: Pixels,
    /// Pending tool-call authorizations, keyed by their (possibly composite)
    /// id. Multiple can be open at once when parallel sub-agents each bubble an
    /// approval request — the overlay shows the most recent and queues the rest,
    /// resolving them one at a time so no `oneshot` is stranded by overwrite.
    pending_auths: Vec<PendingAuth>,
    /// A pending `AskUserQuestion` card; takes precedence over `pending_auths`
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
    /// True while a native directory picker is open from the "Choose project" row.
    /// Guards against the user submitting a message before the picker resolves
    /// (which would make `set_project` a silent no-op once `messages` is non-empty).
    project_picker_pending: bool,
    thread_sub: Option<Subscription>,
    sidebar_sub: Option<Subscription>,
    input_sub: Option<Subscription>,
    editor_sub: Option<Subscription>,
    /// Virtualized, follow-the-tail scroll state for the message list. Replaces
    /// the old `ScrollHandle` + `stick_to_bottom` hand-rolled auto-follow:
    /// `FollowMode::Tail` keeps the viewport pinned to the latest item while the
    /// user is at the bottom and disengages the moment they scroll up.
    list_state: ListState,
    /// Sub-agent task ids whose cards are expanded to show the child
    /// conversation. Toggled by clicking the card header; shared across all
    /// nesting levels so nested agent tasks expand in place.
    pub(crate) expanded_tasks: HashSet<String>,
    /// Top-level view mode. `Settings` replaces the entire window content
    /// with the SettingsView overlay until the user requests exit.
    view_mode: ViewMode,
    /// Set briefly while the Settings overlay is sliding out to the right.
    /// Keeps `view_mode == Settings` mounted so the exit animation can play
    /// before the unmount; cleared when the slide-out completes.
    exiting_settings: bool,
    /// Bumped on every transition into or out of Settings. Embedded in the
    /// slide animation's element id so a fresh tween fires on each direction
    /// change (an old id would replay from the cached delta and visibly
    /// jump), and into the exit spawn so a stale unmount can be no-op'd
    /// when a new enter supersedes it.
    settings_transition_gen: u64,
    /// Lazily created on the first `enter_settings` call so we don't pay the
    /// cost when the user never opens Settings.
    settings_view: Option<Entity<SettingsView>>,
    settings_sub: Option<Subscription>,
}

/// Top-level rendering mode of the Workspace window. The Settings overlay is
/// the only non-default mode today; future overlays (e.g. About) can extend
/// this enum rather than carrying parallel `bool` flags.
#[derive(Default)]
enum ViewMode {
    #[default]
    Workspace,
    Settings,
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

/// Settings overlay slide duration. The enter animation glides the panel in
/// from the left edge, the exit animation glides it out to the right.
const SLIDE_MS: u64 = 180;
/// The Exit handler in `subscribe_settings` waits this long before flipping
/// `view_mode` back to `Workspace`, giving the exit animation time to play.
/// Set slightly above `SLIDE_MS` so the last frame is not popped mid-tween.
const SLIDE_OUT_MS: u64 = 200;

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
                .auto_grow(5, 12)
                .submit_on_enter(true)
                .placeholder("输入消息，点击发送以开始使用")
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

        let list_state = ListState::new(0, ListAlignment::Top, px(2048.));
        list_state.set_follow_mode(FollowMode::Tail);

        let mut ws = Self {
            cwd,
            thread,
            sidebar,
            conversation: cx.new(|_| ConversationState::new()),
            input_state,
            editor_state,
            editor_open: false,
            editor_preview: false,
            editor_width: px(EDITOR_PANEL_WIDTH),
            pending_auths: Vec::new(),
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
            project_picker_pending: false,
            thread_sub: None,
            sidebar_sub: None,
            input_sub: None,
            editor_sub: None,
            list_state,
            expanded_tasks: HashSet::new(),
            view_mode: ViewMode::default(),
            exiting_settings: false,
            settings_transition_gen: 0,
            settings_view: None,
            settings_sub: None,
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
                    this.pending_auths.push(PendingAuth {
                        id: id.clone(),
                        tool_name: tool_name.clone(),
                        summary: summary.clone(),
                    });
                    cx.notify();
                }
                ThreadEvent::Stop(reason) => {
                    let weak = cx.weak_entity();
                    let role = this.model_label(cx);
                    let outcome = this
                        .conversation
                        .update(cx, |c, cx| c.apply(ev, &role, weak, cx));
                    // Stop flips every streaming flag off, so finalized bodies
                    // switch to `TextView::markdown` and need their real height
                    // measured across the whole list.
                    if matches!(outcome, ApplyOutcome::All) {
                        let count = this.conversation.read(cx).items().len();
                        this.list_state.remeasure_items(0..count);
                    }
                    // Persist on terminal state (not the ToolUse mid-state).
                    if !matches!(reason, StopReason::ToolUse) {
                        save_thread(this.thread.clone(), cx);
                    }
                    cx.notify();
                }
                _ => {
                    let weak = cx.weak_entity();
                    let role = this.model_label(cx);
                    let outcome = this
                        .conversation
                        .update(cx, |c, cx| c.apply(ev, &role, weak, cx));
                    // Sub-agent tool results carry the child conversation in
                    // their JSON envelope; feed it into the matching AgentTask
                    // card's expandable panel. The envelope is the single
                    // source of truth (also used on reload).
                    let remeasure_sub = if let ThreadEvent::ToolResult { id, output, .. } = ev
                        && let Some(msgs) = agent::tools::agent::agent_sub_messages(output)
                    {
                        this.conversation
                            .update(cx, |c, cx| c.set_agent_sub_messages(id, msgs, cx))
                    } else {
                        None
                    };
                    let count = this.conversation.read(cx).items().len();
                    match outcome {
                        ApplyOutcome::None => {}
                        ApplyOutcome::Remeasure(ix) => {
                            this.list_state.remeasure_items(ix..ix + 1);
                        }
                        // A new item appended at the end; grow the list count.
                        // FollowMode::Tail keeps the viewport pinned to the new
                        // tail automatically when the user is at the bottom.
                        ApplyOutcome::Appended if count > 0 => {
                            this.list_state.splice(count - 1..count - 1, 1);
                        }
                        ApplyOutcome::All => {
                            this.list_state.remeasure_items(0..count);
                        }
                        ApplyOutcome::Appended => {}
                    }
                    if let Some(ix) = remeasure_sub {
                        this.list_state.remeasure_items(ix..ix + 1);
                    }
                    cx.notify();
                }
            }
        })
    }

    /// `ListState` handle, shared with `AgentTask` cards so an expand/collapse
    /// toggle can invalidate the cached per-item height.
    pub(crate) fn list_state(&self) -> &ListState {
        &self.list_state
    }

    fn subscribe_sidebar(&self, cx: &mut Context<Self>) -> Subscription {
        let sidebar = self.sidebar.clone();
        cx.subscribe(&sidebar, |this, _sidebar, ev: &SidebarEvent, cx| match ev {
            SidebarEvent::NewThread => this.start_new_thread(cx),
            SidebarEvent::OpenThread(id) => this.open_thread(id.clone(), cx),
            SidebarEvent::DeleteThread(id) => this.delete_thread(id.clone(), cx),
        })
    }

    /// Switch into the Settings overlay. The Settings view is created lazily on
    /// first entry; from then on the entity + subscription are reused so the
    /// user's last selection (and any scroll position) survives re-entry.
    pub fn enter_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_view.is_none() {
            let settings = cx.new(|cx| SettingsView::new(window, cx));
            let sub = self.subscribe_settings(&settings, cx);
            self.settings_view = Some(settings);
            self.settings_sub = Some(sub);
        }
        self.view_mode = ViewMode::Settings;
        // Clear any pending exit animation: clicking Settings… while the
        // panel is still sliding out re-opens the overlay. Bumping the
        // transition generation also retires the old exit spawn (it carries
        // the previous gen and no-ops on stale state), and forces the slide
        // animation to replay from the left edge.
        self.exiting_settings = false;
        self.settings_transition_gen = self.settings_transition_gen.wrapping_add(1);
        cx.notify();
    }

    fn subscribe_settings(
        &self,
        settings: &Entity<SettingsView>,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe(settings, |this, _settings, ev: &SettingsEvent, cx| {
            if matches!(ev, SettingsEvent::Exit) && !this.exiting_settings {
                // Start the slide-out animation; the actual mode flip and
                // unmount happen once the animation has finished. The
                // captured transition gen is the watermark for this exit
                // attempt — if a new enter supersedes it before the timer
                // fires, the spawn's update is a no-op.
                this.exiting_settings = true;
                this.settings_transition_gen = this.settings_transition_gen.wrapping_add(1);
                cx.notify();
                let entity = cx.entity().clone();
                let exit_gen = this.settings_transition_gen;
                cx.spawn(async move |_workspace, cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(SLIDE_OUT_MS + 20))
                        .await;
                    entity.update(cx, |this, cx| {
                        if this.settings_transition_gen != exit_gen {
                            return;
                        }
                        this.view_mode = ViewMode::default();
                        this.exiting_settings = false;
                        cx.notify();
                    });
                })
                .detach();
            }
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
        let role = self.model_label(cx);
        let weak = cx.weak_entity();
        let new_conv =
            cx.new(|cx| ConversationState::rebuild_from_messages(&messages, &role, weak, cx));
        let count = new_conv.read(cx).items().len();
        self.conversation = new_conv;
        // The list state held a measured tree for the previous thread's items;
        // reset to the new count so it re-measures from scratch instead of
        // carrying stale heights.
        self.list_state.reset(count);
        self.pending_auths.clear();
        self.pending_ask = None;
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
        if (text.trim().is_empty() && attachments.is_empty())
            || self.thread.read(cx).is_running()
            // Block submit while the project picker is open: setting the
            // project after a message lands is a no-op (set_project guards on
            // !messages.is_empty()), so the project would be silently dropped.
            || self.project_picker_pending
        {
            self.pending_attachments = attachments;
            return;
        }
        self.input_state
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.close_slash_menu();

        // A leading `/` with no leading space is a slash command. Route it to
        // the command engine rather than the model: the command body (with
        // `$ARGUMENTS` substituted) becomes the user message, and the command's
        // `allowed-tools` whitelist narrows the turn's tool set. Attachments are
        // not combined with a command — the command body is the whole turn.
        if attachments.is_empty() {
            if let Some(rest) = text.strip_prefix('/')
                && !rest.starts_with(' ')
            {
                self.send_command_turn(text.clone(), rest, cx);
                return;
            }
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

    /// Run a slash command turn. The original text (e.g. `/gitwork:deliver …`)
    /// is shown to the user as the user bubble; the command engine injects the
    /// rendered body as the model-facing user message. An unknown command emits
    /// an error and drops the turn.
    fn send_command_turn(&mut self, display_text: String, rest: &str, cx: &mut Context<Self>) {
        let role = self.model_label(cx);
        let weak = cx.weak_entity();
        self.conversation
            .update(cx, |c, cx| c.push_user(display_text, &role, weak, cx));
        let count = self.conversation.read(cx).items().len();
        if count > 0 {
            self.list_state.splice(count - 1..count - 1, 1);
        }
        // Split `<command> <args>` — the first whitespace separates the command
        // name from the raw arguments. Everything after the first space is the
        // verbatim `$ARGUMENTS` payload, including further spaces.
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a),
            None => (rest, ""),
        };
        let hit = self
            .thread
            .update(cx, |thread, cx| thread.submit_command(name, args, cx));
        if !hit {
            self.thread.update(cx, |_, cx| {
                cx.emit(agent::ThreadEvent::Error(anyhow::anyhow!(
                    "未知命令：/{name}（用 `/` 菜单查看已安装命令）"
                )));
            });
        }
        save_thread(self.thread.clone(), cx);
        cx.notify();
    }

    /// Append the user turn (text plus any image content) to the thread and start the run.
    fn send_user_turn(
        &mut self,
        text: String,
        images: Vec<agent::language_model::MessageContent>,
        cx: &mut Context<Self>,
    ) {
        use agent::language_model::MessageContent;
        let role = self.model_label(cx);
        let weak = cx.weak_entity();
        self.conversation
            .update(cx, |c, cx| c.push_user(text.clone(), &role, weak, cx));
        // Splice the new user bubble into the list count; FollowMode::Tail
        // scrolls it into view when the user is parked at the bottom.
        let count = self.conversation.read(cx).items().len();
        if count > 0 {
            self.list_state.splice(count - 1..count - 1, 1);
        }
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
        let role = self.model_label(cx);
        let weak = cx.weak_entity();
        self.conversation
            .update(cx, |c, cx| c.push_user(text.clone(), &role, weak, cx));
        let count = self.conversation.read(cx).items().len();
        if count > 0 {
            self.list_state.splice(count - 1..count - 1, 1);
        }
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
        // When an AskUserQuestion card is open its "取消" button calls this; the
        // generic approval overlay is suppressed while a card is open, so the
        // card is the only caller in that state. Resolve the card's specific id
        // rather than the queue tail, so a non-ask auth queued behind the card
        // is not accidentally dismissed.
        let id = match self.pending_ask.as_ref() {
            Some(ask) => ask.id.clone(),
            None => match self.pending_auths.last() {
                Some(a) => a.id.clone(),
                None => return,
            },
        };
        self.pending_auths.retain(|a| a.id != id);
        if self.pending_ask.as_ref().is_some_and(|a| a.id == id) {
            self.pending_ask = None;
        }
        self.thread.update(cx, |thread, cx| {
            thread.respond_authorization(
                &id,
                agent::ToolAuthorizationResponse::Decision(decision),
                cx,
            );
        });
        cx.notify();
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
        // Remove the matching entry from the pending-auth queue (it was pushed
        // alongside the ask card) so it doesn't resurface after the ask resolves.
        self.pending_auths.retain(|a| a.id != id);
        self.pending_ask = None;
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
        let auth = self.pending_auths.last()?;
        let summary = auth.summary.clone();
        let tool_name = auth.tool_name.clone();
        // When several auths are queued behind the visible one, signal that
        // dismissing this card will surface the next.
        let queued = self.pending_auths.len().saturating_sub(1);

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
                        .children(if queued > 0 {
                            Some(
                                gpui::div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("（队列中还有 {queued} 个待审批）")),
                            )
                        } else {
                            None
                        })
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

    /// Composer card: an auto-growing text area above a single toolbar row.
    /// The card's fill matches the page background (`theme.background`) so only
    /// the 1px border outlines it — the user-perceived "底纹" disappears.
    /// The `Input` renders bare (no appearance/border of its own) so there is
    /// no double-layered fill.
    fn render_composer(
        &mut self,
        running: bool,
        first_screen: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let plus = self.render_plus_button(cx);
        let cwd = self.render_cwd_chip(theme, cx);
        let access = self.render_access_placeholder(theme);
        let model = self.render_model_selector(theme, cx);
        let send = self.render_send_button(running, cx);
        // "Choose project" binds the thread's project; only offered before the
        // conversation starts (the empty first screen), matching the reference.
        let choose_project = first_screen.then(|| self.render_choose_project_row(theme, cx));

        v_flex()
            .w_full()
            .gap_2()
            .p_2()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .child(
                Input::new(&self.input_state)
                    .appearance(false)
                    .bordered(false)
                    .focus_bordered(false),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .child(plus)
                            .child(cwd)
                            .child(access),
                    )
                    .child(h_flex().items_center().gap_1().child(model).child(send)),
            )
            .children(choose_project)
            .into_any_element()
    }

    /// Visual-only "full access" affordance mirroring Codex's permission chip.
    /// Not wired to any approval mode; manox still gates tools via the runtime
    /// authorization prompt.
    fn render_access_placeholder(&self, theme: &Theme) -> AnyElement {
        h_flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .child(gpui::div().size(px(6.)).rounded_full().bg(theme.success))
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("完全访问"),
            )
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

    /// Open the native directory picker and bind the chosen folder to the thread
    /// as its project. Tools then resolve paths against it. Offered only on the
    /// empty first screen; `set_project` no-ops once the conversation has started.
    fn choose_project(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.project_picker_pending {
            return;
        }
        self.project_picker_pending = true;
        let dir = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let result = dir.await;
            this.update(cx, |this, cx| {
                this.project_picker_pending = false;
                if let Ok(Ok(Some(paths))) = result
                    && let Some(path) = paths.into_iter().next()
                {
                    this.thread.update(cx, |t, cx| t.set_project(path, cx));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The "Choose project" row inside the composer card, shown only on the empty
    /// first screen. Displays the bound project's basename once one is chosen.
    fn render_choose_project_row(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let project = self.thread.read(cx).project().cloned();
        let (icon, label, color) = match &project {
            Some(dir) => {
                let name = dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("project")
                    .to_string();
                (IconName::FolderOpen, name, theme.foreground)
            }
            None => (
                IconName::Folder,
                "Choose project".to_string(),
                theme.muted_foreground,
            ),
        };
        h_flex()
            .id("choose-project")
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.accent.opacity(0.08)))
            .child(Icon::new(icon).small().text_color(color))
            .child(gpui::div().text_xs().text_color(color).child(label))
            .on_click(cx.listener(|this, _, window, cx| this.choose_project(window, cx)))
            .into_any_element()
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

    /// Static label of the thread's working directory basename. Reflects the
    /// chosen project once one is bound (the thread's `cwd` follows the project).
    ///
    /// Rendered as a plain `div` (not `Button`): the project is picked on the
    /// empty first screen via the "Choose project" row and fixed thereafter, so
    /// a clickable chip would invite clicks that do nothing.
    fn render_cwd_chip(&self, theme: &Theme, cx: &Context<Self>) -> AnyElement {
        let cwd = self.thread.read(cx).cwd().to_path_buf();
        let name = cwd
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| *s != ".")
            .unwrap_or("project");
        gpui::div()
            .px_2()
            .py_1()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(name.to_string())
            .into_any_element()
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Settings overlay replaces the entire window content; the underlying
        // Workspace state (sidebar, conversation, composer) is preserved and
        // returns unchanged when the user clicks 返回应用.
        if matches!(self.view_mode, ViewMode::Settings) {
            let settings = self
                .settings_view
                .as_ref()
                .expect("enter_settings must have created the SettingsView")
                .clone();
            // Horizontal slide: enter glides the panel in from the left edge
            // (offset -PANEL_W → 0), exit glides it out to the right
            // (offset 0 → +PANEL_W). The animation id mixes the current
            // transition generation into the per-direction tag so a fresh
            // tween fires on every direction change (a stable id would
            // replay from the cached delta and visibly jump, and a
            // direction change with the same id would not animate at all).
            let (anim_id, sign) = if self.exiting_settings {
                (
                    format!("settings-exit-{}", self.settings_transition_gen),
                    1.0,
                )
            } else {
                (
                    format!("settings-enter-{}", self.settings_transition_gen),
                    -1.0,
                )
            };
            let panel_w = px(280.0);
            let anim_el = gpui::div().size_full().child(settings).with_animation(
                anim_id,
                Animation::new(Duration::from_millis(SLIDE_MS)).with_easing(ease_out_quint()),
                move |el, delta| {
                    let offset = panel_w * sign * (1.0 - delta);
                    el.relative().ml(offset)
                },
            );
            return h_flex().size_full().child(anim_el);
        }
        let theme = cx.theme().clone();
        let running = self.thread.read(cx).is_running();

        self.ensure_ask_inputs(_window, cx);

        let overlay = self
            .render_ask_overlay(&theme, cx)
            .or_else(|| self.render_auth_overlay(&theme, cx));

        let editor_open = self.editor_open;
        let editor_preview = self.editor_preview;
        let editor_width = self.editor_width;
        // Empty first screen: no messages and nothing streaming. The composer is
        // hoisted into a vertically-centered hero (heading + composer + "Choose
        // project"); once the conversation starts it drops to the bottom footer.
        let first_screen = self.conversation.read(cx).is_empty() && !running;
        // The inline composer and the markdown editor are mutually exclusive: while
        // the editor pane is open the footer is hidden and the draft lives in the
        // editor (moved there on open, moved back on close).
        let footer = if editor_open || first_screen {
            None
        } else {
            Some(
                v_flex()
                    .w_full()
                    .py_2()
                    .gap_2()
                    .relative()
                    .children(self.render_slash_overlay())
                    .children(self.render_attachments(&theme, cx))
                    .child(centered(self.render_composer(running, false, &theme, cx))),
            )
        };
        // Hero occupies the message-list region on the first screen.
        let hero = if editor_open || !first_screen {
            None
        } else {
            Some(
                v_flex()
                    .flex_1()
                    .w_full()
                    .justify_center()
                    .items_center()
                    .relative()
                    .child(
                        centered(
                            v_flex()
                                .w_full()
                                .gap_5()
                                .items_center()
                                .child(
                                    gpui::div()
                                        .text_2xl()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme.foreground)
                                        .child("我们该做什么？"),
                                )
                                .children(self.render_attachments(&theme, cx))
                                .child(self.render_composer(running, true, &theme, cx)),
                        )
                        .relative()
                        .children(self.render_slash_overlay()),
                    ),
            )
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
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.enter_settings(window, cx);
            }))
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
                    // Empty first screen shows the centered hero in place of the
                    // (empty) message list; otherwise the virtualized, tail-
                    // following conversation list. Each item is its own
                    // `Entity<MessageItem>`, so a streaming delta re-renders only
                    // that item; the list only re-invokes the closure for visible
                    // items and reuses cached element subtrees for the rest.
                    .children(hero)
                    .children((!first_screen).then(|| {
                        let conv = self.conversation.clone();
                        list(self.list_state.clone(), move |ix, _window, cx| {
                            conv.read(cx)
                                .items()
                                .get(ix)
                                .cloned()
                                .map(|item| v_flex().py_2().child(item).into_any_element())
                                .unwrap_or_else(|| gpui::Empty.into_any_element())
                        })
                        .with_sizing_behavior(ListSizingBehavior::Auto)
                        .flex_1()
                        .into_any_element()
                    }))
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
