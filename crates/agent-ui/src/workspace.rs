//! Top-level workspace view.
//!
//! Holds `Entity<agent::Thread>` + `Entity<Sidebar>`; `cx.subscribe` handles:
//! - `ThreadEvent`: text/thinking/tool deltas go to `ConversationState`; `ToolCallAuthorization` opens an approval overlay;
//!   the terminal `Stop` (non-ToolUse) triggers `save_thread`.
//! - `SidebarEvent`: new conversation / open history / delete.
//!
//! Enter in the input box → append a user message + run_turn + persist (the sidebar shows the new entry immediately).

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use agent::language_model::StopReason;
use agent::provider::WireApi;
use agent::provider::registry;
use agent::thread::ApprovalMode;
use agent::{
    PermissionDecision, PlanApprovalResponse, ReasoningEffort, Thread, ThreadEvent, ThreadId, i18n,
    save_thread,
};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, ClickEvent, Context, CursorStyle, DismissEvent,
    DragMoveEvent, Entity, FollowMode, ListAlignment, ListOffset, ListState, MouseButton,
    MouseUpEvent, Pixels, Render, SharedString, Subscription, WeakEntity, Window, deferred,
    ease_out_quint, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, StyledExt as _,
    TITLE_BAR_HEIGHT, Theme, TitleBar,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{PopupMenu, PopupMenuItem},
    tab::{Tab, TabBar},
    tag::{Tag, TagVariant},
    v_flex,
};
use manox_components::markdown::{HeadingMode, Markdown};

use composer::{ComposerEvent, ComposerInput};

use crate::conversation::{ApplyOutcome, ConvItem, ConversationState, UserImage, UserTurnMeta};
use crate::views::centered;
use crate::views::composer_menu::{
    PendingAttachment, build_plus_menu, build_slash_menu, load_attachment, render_attachment_chips,
};
use crate::views::member_panel::MemberPanel;
use crate::views::plugin_manager::{PluginManagerEvent, PluginManagerView};
use crate::views::settings::{SettingsEvent, SettingsView};
use crate::views::sidebar::{Sidebar, SidebarEvent};
use crate::{
    AskCancel, AskNext, AskPrev, CloseTerminalTab, FocusConversation, FocusTerminal,
    NewTerminalTab, OpenSettings,
};
use terminal::Terminal;
use terminal_ui::TerminalView;

/// A tab in the right observation pane. `Editor` is the markdown composer
/// (Write/Preview); `Member(name)` is a read-only [`MemberPanel`] over a team
/// worker's conversation + tasks.
#[derive(Clone, Debug)]
enum RightTab {
    Editor,
    Member(String),
}

/// A pending tool-call authorization prompted by `ThreadEvent::ToolCallAuthorization`.
///
/// `reason` is populated by the `AutoReview` approval agent when it returns
/// `Ask` for a tool call. It is rendered as a one-line muted note under the
/// tool title in the auth overlay so the user can see why the reviewer
/// escalated the call rather than auto-approving it.
struct PendingAuth {
    id: String,
    tool_name: String,
    summary: String,
    reason: Option<String>,
}

/// A pending plan approval prompted by `ThreadEvent::PlanProposed`. The plan
/// text is carried both here (for the approval overlay body) and backfilled
/// into the matching ToolCall item (for the chat view).
struct PendingPlan {
    id: String,
    plan_text: String,
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
    /// Free-form dismiss input; non-empty text is sent as the `response`
    /// field of `ToolAuthorizationResponse::AskUserQuestion`, overriding
    /// all per-question answers.
    response_input: Option<Entity<InputState>>,
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
    /// Threads that were running when the user switched away. Holding strong
    /// references keeps their `run_turn_loop` tasks alive so they can finish
    /// in the background and persist via the spawned-task save backstop.
    background_threads: Vec<Entity<Thread>>,
    sidebar: Entity<Sidebar>,
    pub(crate) conversation: Entity<ConversationState>,
    pub(crate) input_state: Entity<ComposerInput>,
    /// Right-side markdown composer; opened via the `ToggleEditor` shortcut.
    /// Plain-text edit mode by default; `ToggleEditorPreview` switches to a
    /// rendered markdown preview (`Markdown`).
    editor_state: Entity<InputState>,
    /// Whether the Editor tab is the active right-pane tab. Drives the inline
    /// composer hide (writing happens in the side panel) and the env/hero
    /// gates. A member tab being active leaves this `false` so the inline
    /// composer stays usable for talking to the leader.
    editor_open: bool,
    editor_preview: bool,
    /// Right-pane tabs: the markdown Editor plus one Member tab per observed
    /// worker. The pane renders while non-empty; `editor_open` tracks whether
    /// the Editor tab specifically is active.
    right_tabs: Vec<RightTab>,
    active_right_tab: usize,
    /// Lazily-built MemberPanel entities, keyed by member name. A member tab
    /// keeps its panel across tab switches; dropped when the tab closes.
    member_panels: BTreeMap<String, Entity<MemberPanel>>,
    /// Editor pane width, driven by dragging the divider. In-memory only.
    editor_width: Pixels,
    /// Sidebar width, driven by dragging the divider on its right edge.
    /// In-memory only; never persisted so the user's drag state stays
    /// session-local.
    sidebar_width: Pixels,
    /// Pending tool-call authorizations, keyed by their (possibly composite)
    /// id. Multiple can be open at once when parallel sub-agents each bubble an
    /// approval request — the overlay shows the most recent and queues the rest,
    /// resolving them one at a time so no `oneshot` is stranded by overwrite.
    pending_auths: Vec<PendingAuth>,
    /// A pending `AskUserQuestion` card; replaces the composer footer with
    /// the ask drawer while open.
    pending_ask: Option<PendingAsk>,
    /// Current question index in the ask drawer (0-based).
    ask_step: usize,
    /// Animation generation counter for the ask drawer slide, bumped on every
    /// open/close so a fresh tween fires rather than replaying a cached delta.
    ask_transition_gen: u64,
    pub(crate) model_open: bool,
    /// PopupMenu entity for the open model selector; created on open, destroyed on close.
    model_menu: Option<Entity<PopupMenu>>,
    model_menu_sub: Option<Subscription>,
    plus_open: bool,
    plus_menu: Option<Entity<PopupMenu>>,
    plus_menu_sub: Option<Subscription>,
    /// Access-chip dropdown (Normal / YOLO mode). Mirrors the model selector pattern.
    access_open: bool,
    /// Reasoning-effort dropdown (Low / Medium / High / XHigh / Max / Ultracode / Auto).
    effort_open: bool,
    effort_menu: Option<Entity<PopupMenu>>,
    effort_menu_sub: Option<Subscription>,
    /// Project-chip dropdown (recent projects + new project submenu).
    project_chip_open: bool,
    project_chip_menu: Option<Entity<PopupMenu>>,
    project_chip_menu_sub: Option<Subscription>,
    slash_open: bool,
    slash_menu: Option<Entity<PopupMenu>>,
    slash_menu_sub: Option<Subscription>,
    /// Title bar "..." dropdown (conversation menu). Mirrors the
    /// model selector pattern: a button toggles `title_menu_open`; the
    /// `PopupMenu` entity and its dismiss subscription are created on open.
    title_menu_open: bool,
    title_menu: Option<Entity<PopupMenu>>,
    title_menu_sub: Option<Subscription>,
    /// A pending plan approval (model called `exit_plan_mode`). The overlay
    /// takes precedence after the auth overlay.
    pending_plan: Option<PendingPlan>,
    /// Files picked via the `+` menu, not yet sent. Cleared on submit.
    pending_attachments: Vec<PendingAttachment>,
    /// True while a native directory picker is open from the "Choose project" row.
    /// Guards against the user submitting a message before the picker resolves
    /// (which would make `set_project` a silent no-op once `messages` is non-empty).
    project_picker_pending: bool,
    /// Parent directory selected for "Create blank project"; waiting for name input.
    blank_project_parent: Option<PathBuf>,
    /// Input state for the blank project folder name overlay.
    blank_project_name_input: Option<Entity<InputState>>,
    thread_sub: Option<Subscription>,
    sidebar_sub: Option<Subscription>,
    input_sub: Option<Subscription>,
    editor_sub: Option<Subscription>,
    /// Scroll/virtualization state for the message column. The gpui `list`
    /// element only lays out (and only syntax-highlights, via the synchronous
    /// renderer) the items in the viewport plus `MSG_LIST_OVERDRAW` — so a long
    /// thread's first frame pays only for the visible turn, not every block.
    /// `FollowMode::Tail` replaces the hand-rolled stick-to-bottom: it re-pins
    /// to the end each layout while following and re-arms when the user scrolls
    /// back to the bottom. Item heights are deterministic per-frame (sync
    /// markdown), so the per-item height cache the list maintains never falls
    /// out of sync with async parsing — the root cause of the old overlap bug.
    list_state: ListState,
    /// Cached `items().len()`; the event handler reconciles the list count via
    /// `splice` whenever the conversation grows or shrinks.
    list_count: usize,
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
    /// Whether the goal status popover is open (toggled by the `◎ /goal active`
    /// chip or the bare `/goal` command).
    goal_popover_open: bool,
    /// Whether the team roster drawer is open (toggled by the `👥 team · N`
    /// chip when the leader has formed a team). Each row opens that member's
    /// observation tab in the right pane.
    team_chip_open: bool,
    /// Generation counter for the goal elapsed-time ticker. Incremented when a
    /// goal is cleared or the active thread changes so the prior ticker
    /// self-terminates instead of notifying a stale chip. Mirrors
    /// `settings_transition_gen`.
    goal_ticker_gen: u64,
    /// Lazily created on the first `enter_settings` call so we don't pay the
    /// cost when the user never opens Settings.
    settings_view: Option<Entity<SettingsView>>,
    settings_sub: Option<Subscription>,
    plugin_manager_view: Option<Entity<PluginManagerView>>,
    plugin_manager_sub: Option<Subscription>,
    /// The terminal tab's view, lazily created on the first `FocusTerminal` /
    /// `NewTerminalTab`. `None` until then. Dropped on `CloseTerminalTab`.
    terminal_view: Option<Entity<TerminalView>>,
    /// Ordinal of the outline tick currently under the cursor, if any. Drives
    /// the "wave" hover effect: the hovered tick and its neighbors lengthen and
    /// spread apart, tapering off with distance. `None` when the cursor is off
    /// the rail.
    outline_hover: Option<usize>,
}

/// Top-level rendering mode of the Workspace window. `Settings` and
/// `Terminal` are full-pane switches off the default `Workspace` (conversation)
/// mode; future overlays can extend this enum rather than carrying parallel
/// `bool` flags.
#[derive(Default)]
enum ViewMode {
    #[default]
    Workspace,
    Settings,
    Plugins,
    Terminal,
}

/// Right-side composer width. Wide enough for rendered markdown
/// (headings, lists, code blocks) alongside the 1100px window.
const EDITOR_PANEL_WIDTH: f32 = 640.;
const EDITOR_MIN_WIDTH: f32 = 320.;
const EDITOR_MAX_WIDTH: f32 = 960.;
/// Width of the drag handle between the main column and the editor pane.
const EDITOR_DIVIDER_WIDTH: f32 = 6.;
// Mirrors `views/sidebar.rs` (`Sidebar` renders at `w(px(SIDEBAR_WIDTH))`).
// Kept here so the editor pane's resize clamp can reserve space for the
// sidebar + main column without depending on the sidebar's internals.
const SIDEBAR_WIDTH: f32 = 260.;
const SIDEBAR_MIN_WIDTH: f32 = 200.;
const SIDEBAR_MAX_WIDTH: f32 = 480.;
const SIDEBAR_DIVIDER_WIDTH: f32 = 6.;
/// Floor for the main column width when the editor pane is dragged wide.
const MAIN_MIN_WIDTH: f32 = 160.;

/// Environment info card floating at the top-right of the conversation area.
/// Wide enough to hold the two-line per-model usage block: model id + in/out
/// on line one, cache read/cache creation on line two, each with `↑↓ cached`
/// markers.
const ENV_CARD_WIDTH: f32 = 380.;
const ENV_CONTENT_INSET: f32 = ENV_CARD_WIDTH + 36.;
/// Minimum main-column width for the env card to mount at all. Below this the
/// card stays hidden and the messages + composer take the full body width, so
/// a narrow window never crowds the conversation. The card is otherwise
/// default-off (editor closed, thread interacted with, past first screen).
const ENV_CARD_MIN_MAIN_W: f32 = 900.;
/// Longest model id rendered in full, calibrated to "MiniMax/MiniMax-M3[1m]"
/// (22 chars). Longer ids are cut to this width then trimmed by 3 and given a
/// "..." suffix, so the result stays one line at `ENV_MODEL_ID_MAX` chars
/// (e.g. "MiniMax/MiniMax-M3[...").
const ENV_MODEL_ID_MAX: usize = 22;

/// User-turn outline rail geometry. The rail is a fixed-width gutter between
/// the sidebar divider and the message list; every tick is the same length so
/// it reads as a pure navigation anchor, not a length-encoded minimap.
const OUTLINE_RAIL_WIDTH: f32 = 40.;
const OUTLINE_TICK_WIDTH: f32 = 16.;
const OUTLINE_TICK_HEIGHT: f32 = 2.;
/// Vertical gap between ticks.
const OUTLINE_TICK_GAP: f32 = 8.;
/// Hover card max width; the summary wraps within it.
const OUTLINE_CARD_WIDTH: f32 = 260.;
/// Wave hover displacement: at the crest a tick grows this much wider and its
/// row this much taller, tapering to zero at the wave's edge. Neighbors bulge
/// out around the cursor like the outline rail.
const OUTLINE_WAVE_EXTRA_WIDTH: f32 = 12.;
const OUTLINE_WAVE_EXTRA_GAP: f32 = 6.;

/// How far above/below the viewport the message `list` still measures items.
/// Larger = smoother fast-scroll at the cost of less virtualization; one extra
/// screenful is enough that a normal wheel flick never shows an unmeasured gap.
const MSG_LIST_OVERDRAW: f32 = 800.;

/// Settings overlay slide duration. The enter animation glides the panel in
/// from the left edge, the exit animation glides it out to the right.
const SLIDE_MS: u64 = 180;
/// The Exit handler in `subscribe_settings` waits this long before flipping
/// `view_mode` back to `Workspace`, giving the exit animation time to play.
/// Set slightly above `SLIDE_MS` so the last frame is not popped mid-tween.
const SLIDE_OUT_MS: u64 = 200;

/// Drag payload for the editor pane divider. Doubles as the invisible drag
/// ghost view, mirroring the `DraggedDock` drag-ghost pattern.
struct DraggedEditorDivider;

impl Render for DraggedEditorDivider {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

/// Drag payload for the sidebar divider. Same shape as the editor divider's
/// payload; the two are distinguished by type so their drag-move handlers
/// can each run only on the matching payload.
struct DraggedSidebarDivider;

impl Render for DraggedSidebarDivider {
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
            ComposerInput::new(window, cx)
                .multi_line(true)
                .auto_grow(4, 12)
                .submit_on_enter(true)
                .placeholder(i18n::t("workspace-input-placeholder"))
        });

        let editor_state = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("markdown")
                .line_number(true)
                .folding(false)
                .soft_wrap(true)
                .submit_on_enter(false)
                .placeholder(i18n::t("workspace-composer-placeholder"))
        });

        let sidebar = cx.new(|cx| Sidebar::new(px(SIDEBAR_WIDTH), cx));

        let mut ws = Self {
            cwd,
            thread,
            background_threads: Vec::new(),
            sidebar,
            conversation: cx.new(|_| ConversationState::new()),
            input_state,
            editor_state,
            editor_open: false,
            editor_preview: false,
            right_tabs: Vec::new(),
            active_right_tab: 0,
            member_panels: BTreeMap::new(),
            editor_width: px(EDITOR_PANEL_WIDTH),
            sidebar_width: px(SIDEBAR_WIDTH),
            pending_auths: Vec::new(),
            pending_ask: None,
            ask_step: 0,
            ask_transition_gen: 0,
            model_open: false,
            model_menu: None,
            model_menu_sub: None,
            plus_open: false,
            plus_menu: None,
            plus_menu_sub: None,
            access_open: false,
            effort_open: false,
            effort_menu: None,
            effort_menu_sub: None,
            project_chip_open: false,
            project_chip_menu: None,
            project_chip_menu_sub: None,
            slash_open: false,
            slash_menu: None,
            slash_menu_sub: None,
            title_menu_open: false,
            title_menu: None,
            title_menu_sub: None,
            pending_plan: None,
            pending_attachments: Vec::new(),
            project_picker_pending: false,
            blank_project_parent: None,
            blank_project_name_input: None,
            thread_sub: None,
            sidebar_sub: None,
            input_sub: None,
            editor_sub: None,
            list_state: ListState::new(0, ListAlignment::Top, px(MSG_LIST_OVERDRAW)),
            list_count: 0,
            expanded_tasks: HashSet::new(),
            view_mode: ViewMode::default(),
            exiting_settings: false,
            settings_transition_gen: 0,
            goal_popover_open: false,
            team_chip_open: false,
            goal_ticker_gen: 0,
            settings_view: None,
            settings_sub: None,
            plugin_manager_view: None,
            plugin_manager_sub: None,
            terminal_view: None,
            outline_hover: None,
        };
        ws.thread_sub = Some(ws.subscribe_thread(cx));
        ws.sidebar_sub = Some(ws.subscribe_sidebar(cx));
        ws.input_sub = Some(ws.subscribe_input(window, cx));
        ws.editor_sub = Some(ws.subscribe_editor(window, cx));
        // Focus the composer so typing works immediately on the hero screen.
        ws.input_state.update(cx, |s, cx| s.focus(window, cx));
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
                        this.ask_step = 0;
                        this.ask_transition_gen = this.ask_transition_gen.wrapping_add(1);
                    }
                    // The `AutoReview` approval agent attaches a one-line reason
                    // to every tool it escalates back to the overlay; pull it
                    // out here so the user can see *why* the reviewer did not
                    // auto-approve. We snapshot-and-clear on the Thread side
                    // because each reason is single-use — a stale reason on
                    // the next tool call would mislead the user.
                    let reason = this
                        .thread
                        .update(cx, |t, _cx| t.take_approval_ask_reason(id.as_str()));
                    this.pending_auths.push(PendingAuth {
                        id: id.clone(),
                        tool_name: tool_name.clone(),
                        summary: summary.clone(),
                        reason,
                    });
                    cx.notify();
                }
                ThreadEvent::PlanProposed { id, plan_text } => {
                    this.pending_plan = Some(PendingPlan {
                        id: id.clone(),
                        plan_text: plan_text.clone(),
                    });
                    // Delegate to ConversationState to backfill the plan text
                    // into the matching ToolCall item for markdown rendering.
                    let weak = cx.weak_entity();
                    let role = this.model_label(cx);
                    let outcome = this
                        .conversation
                        .update(cx, |c, cx| c.apply(ev, &role, None, weak, cx));
                    this.apply_list_outcome(outcome, cx);
                    cx.notify();
                }
                ThreadEvent::ApprovalModeChanged { .. } => {
                    // Refresh the access chip + YOLO badge; no conversation item.
                    cx.notify();
                }
                ThreadEvent::ModelChanged { from, to } => {
                    // Persist a model_change event to the thread's event stream.
                    // The conversation view itself stays unchanged (no item).
                    let thread_id = this.thread.read(cx).id.0.clone();
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| {
                        s.record_model_change(&thread_id, from.as_deref(), to, cx);
                    });
                    cx.notify();
                }
                ThreadEvent::ReasoningEffortChanged { .. } => {
                    // Persist effort change to the thread record immediately.
                    save_thread(this.thread.clone(), false, cx);
                    cx.notify();
                }
                ThreadEvent::TokenUsageUpdated(_) => {
                    cx.notify();
                }
                ThreadEvent::TurnStarted => {
                    // Light up the sidebar running indicator immediately —
                    // before the first streaming delta arrives (model warm-up,
                    // network latency). Terminal `Stop`/`Error` below clear it.
                    let thread_id = this.thread.read(cx).id.0.clone();
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| s.mark_running(&thread_id, cx));
                }
                ThreadEvent::Stop(reason) => {
                    // A terminal state ends any pending plan approval (the
                    // oneshot was resolved or cancelled on the thread side).
                    this.pending_plan = None;
                    let weak = cx.weak_entity();
                    let role = this.model_label(cx);
                    let usage = this.thread.read(cx).last_request_token_usage();
                    let outcome = this
                        .conversation
                        .update(cx, |c, cx| c.apply(ev, &role, usage, weak, cx));
                    // Stop flips streaming flags off, so finalized bodies switch
                    // to full `Markdown` layout and may grow a frame or two later;
                    // `apply_list_outcome` remeasures every item (Absolute anchor)
                    // so the growth doesn't overlap. Tail-follow, if still engaged,
                    // re-pins to the end across that growth.
                    this.apply_list_outcome(outcome, cx);
                    // Persist on terminal state (not the ToolUse mid-state).
                    if !matches!(reason, StopReason::ToolUse) {
                        let thread_id = this.thread.read(cx).id.0.clone();
                        save_thread(this.thread.clone(), true, cx);
                        // Terminal stop → this thread is no longer running.
                        let store = agent::thread_store_global();
                        store.update(cx, |s, cx| s.mark_idle(&thread_id, cx));
                        // Clean up background reference if this thread was parked.
                        this.background_threads
                            .retain(|t| t.read(cx).id.0 != thread_id);
                    }
                    cx.notify();
                }
                ThreadEvent::PrefixStability { .. } => {
                    // Per-turn cache stability signal. The composer chip that
                    // used to render this was removed in #62; the event stays
                    // emitted for any future telemetry/debug subscriber.
                    cx.notify();
                }
                ThreadEvent::GoalChanged { active } => {
                    // Bump the ticker generation so any prior ticker
                    // self-terminates; start a fresh ticker only on activation.
                    this.goal_ticker_gen = this.goal_ticker_gen.wrapping_add(1);
                    if *active {
                        let entity = cx.entity().clone();
                        let ticker_gen = this.goal_ticker_gen;
                        cx.spawn(async move |_this, cx| {
                            loop {
                                cx.background_executor()
                                    .timer(std::time::Duration::from_secs(1))
                                    .await;
                                let still = entity.read_with(cx, |this, cx| {
                                    this.goal_ticker_gen == ticker_gen
                                        && this.thread.read(cx).goal().is_some()
                                });
                                if !still {
                                    break;
                                }
                                entity.update(cx, |_, cx| cx.notify());
                            }
                        })
                        .detach();
                    }
                    cx.notify();
                }
                ThreadEvent::GoalEvaluated { .. } => {
                    // Refresh the status popover's last-reason / evaluations
                    // rows; no conversation item is produced.
                    cx.notify();
                }
                _ => {
                    // `Error` is a terminal signal symmetric to a terminal
                    // `Stop`: the turn aborted, so this thread is no longer
                    // running. Pulled out of the catch-all rather than given a
                    // dedicated arm because the conversation still needs the
                    // generic `apply` below to render the error item.
                    if let ThreadEvent::Error(_) = ev {
                        let thread_id = this.thread.read(cx).id.0.clone();
                        let store = agent::thread_store_global();
                        store.update(cx, |s, cx| s.mark_idle(&thread_id, cx));
                        this.background_threads
                            .retain(|t| t.read(cx).id.0 != thread_id);
                    }
                    let weak = cx.weak_entity();
                    let role = this.model_label(cx);
                    let usage = this.thread.read(cx).last_request_token_usage();
                    let outcome = this
                        .conversation
                        .update(cx, |c, cx| c.apply(ev, &role, usage, weak, cx));
                    // Sub-agent tool results carry the child conversation in
                    // their JSON envelope; feed it into the matching AgentTask
                    // card's expandable panel. The envelope is the single
                    // source of truth (also used on reload). The card's height
                    // may grow, so remeasure its index.
                    if let ThreadEvent::ToolResult { id, output, .. } = ev
                        && let Some(msgs) = agent::tools::agent::agent_sub_messages(output)
                        && let Some(ix) = this
                            .conversation
                            .update(cx, |c, cx| c.set_agent_sub_messages(id, msgs, cx))
                    {
                        this.list_state.remeasure_items(ix..ix + 1);
                    }
                    // Splice on count change, remeasure the mutated index on
                    // in-place mutation, so the per-item height cache never goes
                    // stale (the old overlap bug under async markdown).
                    this.apply_list_outcome(outcome, cx);
                    cx.notify();
                }
            }
        })
    }

    /// The outline rail: one equal-length tick per user turn,
    /// mounted between the sidebar divider and the message list. Ticks for the
    /// turns currently on screen are highlighted; hovering a tick reveals a
    /// summary card and clicking it scrolls that turn into view.
    ///
    /// Returns `None` when there are no user turns yet, so the first screen
    /// stays clean.
    fn render_outline(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::views::outline;

        let turns = outline::user_turns_from(
            self.conversation
                .read(cx)
                .items()
                .iter()
                .map(|e| e.read(cx).kind()),
        );
        if turns.is_empty() {
            return None;
        }
        let total = self.conversation.read(cx).items().len();
        // Which turn is on screen is queried live from the list each frame, so
        // programmatic scrolls (click-to-reveal) highlight correctly, not just
        // user wheel scrolls.
        //
        // Tail-follow is a special case: while pinned to the bottom the list
        // forces its scroll top past the last item, which makes the positional
        // queries below meaningless. The viewport then shows the end of the
        // conversation, so the last turn is the visible one.
        let following = self.list_state.is_following_tail();
        let last_ordinal = turns.len() - 1;
        // Fallback for the pre-layout frame, before the list has captured item
        // bounds to answer positional queries.
        let fallback_top = self.list_state.logical_scroll_top().item_ix;
        // Viewport box in window coordinates. `bounds_for_item` already returns
        // window-coordinate (scroll-adjusted) bounds, so a span is off-screen
        // when its last item's bottom is above the viewport top, or its first
        // item's top is below the viewport bottom — no manual offset needed.
        let vp = self.list_state.viewport_bounds();

        let hovered = self.outline_hover;
        let ticks = turns.iter().map(|turn| {
            let span = outline::turn_span(&turns, turn.ordinal, total);
            // A turn is visible unless its whole span sits above or below the
            // viewport. `bounds_for_item` returns `None` before layout; then
            // fall back to the logical scroll top intersecting the span.
            let last = span.end.saturating_sub(1);
            let active = if following {
                turn.ordinal == last_ordinal
            } else {
                match (
                    self.list_state.bounds_for_item(last),
                    self.list_state.bounds_for_item(span.start),
                ) {
                    // `last`'s bottom above the viewport top ⇒ the whole span
                    // sits above; `span.start`'s top at/below the viewport
                    // bottom ⇒ the whole span sits below. Visible otherwise.
                    (Some(lb), Some(sb)) => {
                        let above = lb.bottom() <= vp.top();
                        let below = sb.top() >= vp.bottom();
                        !above && !below
                    }
                    // Pre-layout (bounds not captured yet): fall back to the
                    // top visible item intersecting the span.
                    _ => span.contains(&fallback_top),
                }
            };
            let target = turn.item_ix;
            let ordinal = turn.ordinal;
            let has_summary = !turn.summary.is_empty();

            // Wave displacement: the hovered tick and its neighbors grow wider
            // and their rows taller, so the rail bulges around the cursor.
            let weight = outline::wave_weight(ordinal, hovered);
            let tick_width = OUTLINE_TICK_WIDTH + weight * OUTLINE_WAVE_EXTRA_WIDTH;
            let row_height =
                OUTLINE_TICK_HEIGHT + OUTLINE_TICK_GAP + weight * OUTLINE_WAVE_EXTRA_GAP;

            // On-screen turns read at full strength; the wave lifts the rest
            // toward the foreground as the cursor nears them.
            let tick_color = if active {
                theme.foreground.opacity(0.8)
            } else {
                theme.muted_foreground.opacity(0.35 + weight * 0.45)
            };
            let tick = gpui::div()
                .w(px(tick_width))
                .h(px(OUTLINE_TICK_HEIGHT))
                .rounded_full()
                .bg(tick_color);

            // The card is driven by `outline_hover`, not `group_hover`: every
            // hover fires `on_hover` → re-render, which rebuilds the tree and
            // resets any `group_hover` state, so a group-driven card would flash
            // and vanish. Keying off the persisted hover ordinal keeps it up.
            let card = (has_summary && hovered == Some(ordinal)).then(|| {
                // `deferred` paints the card after the whole workspace tree, so
                // it floats above the message list instead of being occluded by
                // the chat bubbles it overlaps.
                //
                // A fixed width is required: an absolutely-positioned box with
                // only `max_w` collapses to its min-content width, which for CJK
                // text is one glyph per line. `w` + `flex_shrink_0` pins it so
                // the summary wraps at the card edge, not at every character.
                deferred(
                    gpui::div()
                        .absolute()
                        .left_full()
                        .ml_2()
                        .w(px(OUTLINE_CARD_WIDTH))
                        .flex_shrink_0()
                        .px_3()
                        .py_2()
                        .rounded(theme.radius)
                        .bg(theme.popover)
                        .border_1()
                        .border_color(theme.border)
                        .text_color(theme.popover_foreground)
                        .text_sm()
                        .shadow_md()
                        .child(turn.summary.clone()),
                )
                .with_priority(1)
            });

            h_flex()
                .id(target)
                .relative()
                .h(px(row_height))
                .w_full()
                .justify_center()
                .items_center()
                .cursor_pointer()
                .child(tick)
                .children(card)
                .on_hover(cx.listener(move |this, entered: &bool, _window, cx| {
                    let next = if *entered { Some(ordinal) } else { None };
                    // Clear only if the cursor left *this* tick; a newer tick's
                    // enter has already overwritten `outline_hover`.
                    if *entered || this.outline_hover == Some(ordinal) {
                        this.outline_hover = next;
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    // Pin the turn to the top of the viewport. Disengage
                    // tail-follow first, otherwise the list would re-pin to the
                    // bottom on the next layout and the reveal would be
                    // overwritten (the click would appear to do nothing).
                    this.list_state.set_follow_mode(FollowMode::Normal);
                    this.list_state.scroll_to(ListOffset {
                        item_ix: target,
                        offset_in_item: px(0.),
                    });
                    cx.notify();
                }))
        });

        // `overflow_hidden` + `min_h_0` clip the tick column when a long
        // conversation's ticks exceed the rail height, instead of overflowing
        // the layout. Widened wave ticks stay within the fixed width (28px max
        // < 40px rail), and the hover card is `deferred` — painted outside this
        // subtree — so neither is affected by the clip.
        Some(
            v_flex()
                .flex_shrink_0()
                .w(px(OUTLINE_RAIL_WIDTH))
                .h_full()
                .min_h_0()
                .overflow_hidden()
                .justify_center()
                .items_center()
                .children(ticks)
                .into_any_element(),
        )
    }

    fn subscribe_sidebar(&self, cx: &mut Context<Self>) -> Subscription {
        let sidebar = self.sidebar.clone();
        cx.subscribe(&sidebar, |this, _sidebar, ev: &SidebarEvent, cx| match ev {
            SidebarEvent::NewThread => this.start_new_thread(cx),
            SidebarEvent::OpenPlugins => this.enter_plugins(cx),
            SidebarEvent::OpenThread(id) => this.open_thread(id.clone(), cx),
            SidebarEvent::ArchiveThread(id, archived) => {
                let store = agent::thread_store_global();
                store.update(cx, |s, cx| s.archive_thread(id, *archived, cx));
                // Sync the in-memory flag so the title-bar menu label stays
                // fresh when the sidebar archives the currently active thread.
                if this.thread.read(cx).id.0 == *id {
                    this.thread
                        .update(cx, |t, cx| t.set_archived(*archived, cx));
                }
            }
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

    /// Switch into the plugin/marketplace/skill/MCP management pane.
    pub fn enter_plugins(&mut self, cx: &mut Context<Self>) {
        self.view_mode = ViewMode::Plugins;
        cx.notify();
    }

    fn ensure_plugins(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.plugin_manager_view.is_some() {
            return;
        }
        let plugins = cx.new(|cx| PluginManagerView::new(window, cx));
        let sub = self.subscribe_plugins(&plugins, cx);
        self.plugin_manager_view = Some(plugins);
        self.plugin_manager_sub = Some(sub);
    }

    fn subscribe_plugins(
        &self,
        plugins: &Entity<PluginManagerView>,
        cx: &mut Context<Self>,
    ) -> Subscription {
        cx.subscribe(plugins, |this, _plugins, ev: &PluginManagerEvent, cx| {
            if matches!(ev, PluginManagerEvent::Exit) {
                this.view_mode = ViewMode::default();
                cx.notify();
            }
        })
    }

    /// Switch to the conversation pane.
    pub fn focus_conversation(&mut self, cx: &mut Context<Self>) {
        self.view_mode = ViewMode::Workspace;
        cx.notify();
    }

    /// Switch to the terminal pane, creating the terminal tab on first focus.
    /// The terminal runs in the workspace's cwd with the user's shell.
    pub fn focus_terminal(&mut self, cx: &mut Context<Self>) {
        if self.terminal_view.is_none() {
            let id = uuid::Uuid::new_v4().to_string();
            let terminal = match Terminal::new(id, self.cwd.clone(), 80, 24, cx) {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = ?e, "failed to spawn terminal");
                    return;
                }
            };
            self.terminal_view = Some(TerminalView::new(terminal, cx));
        }
        self.view_mode = ViewMode::Terminal;
        cx.notify();
    }

    /// Open a fresh terminal tab (cmd-t). If one already exists it is reused
    /// rather than replaced, so an in-flight session isn't killed.
    pub fn open_terminal_tab(&mut self, cx: &mut Context<Self>) {
        self.focus_terminal(cx);
    }

    /// Close the terminal tab and return to the conversation pane. Dropping
    /// the `TerminalView` drops the underlying `Terminal`, whose `PtyHandle`
    /// kills the child and joins the reader/waiter threads.
    pub fn close_terminal_tab(&mut self, cx: &mut Context<Self>) {
        self.terminal_view = None;
        self.focus_conversation(cx);
    }

    fn subscribe_input(&self, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        let input = self.input_state.clone();
        cx.subscribe_in(
            &input,
            window,
            |this, _, ev: &ComposerEvent, window, cx| match ev {
                ComposerEvent::PressEnter => this.submit_input(window, cx),
                ComposerEvent::Change => this.sync_slash_menu(window, cx),
                ComposerEvent::PastedImage(image) => {
                    this.handle_pasted_image(image.clone(), cx);
                }
                ComposerEvent::Focus | ComposerEvent::Blur => {}
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

    /// Open the `⁄` command menu when the input is exactly `/`, close it otherwise.
    /// Selecting a registered command inserts `/name ` into the composer for the
    /// user to complete and submit; the memory/skills rows remain static decoration.
    fn sync_slash_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.input_state.read(cx).value().to_string();
        // Open the `⁄` popover only when the input is exactly `/`; selecting a
        // command replaces the value with `/name ` (see `on_select` below).
        let should_open = value == "/";
        if should_open && !self.slash_open {
            let theme = cx.theme().clone();
            let on_select = cx.listener(|this, name: &str, window, cx| {
                // Insert `/name ` into the composer so the user can add args
                // and submit. Replacing the whole value keeps the leading `/`
                // consistent (the popover only opens for input == "/").
                let text = format!("/{name} ");
                this.input_state
                    .update(cx, |state, cx| state.set_value(text, window, cx));
                this.close_slash_menu();
                cx.notify();
            });
            let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
                build_slash_menu(menu, &theme, move |name, window, cx| {
                    on_select(name, window, cx);
                })
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

    /// Close the title bar "..." dropdown, dropping the menu entity + subscription.
    fn close_title_menu(&mut self) {
        self.title_menu_open = false;
        self.title_menu = None;
        self.title_menu_sub = None;
    }

    /// Close the access-chip dropdown, dropping the menu entity + subscription.
    fn close_access_menu(&mut self) {
        self.access_open = false;
    }

    fn close_effort_menu(&mut self) {
        self.effort_open = false;
        self.effort_menu = None;
        self.effort_menu_sub = None;
    }

    /// Close the project-chip dropdown.
    fn close_project_chip_menu(&mut self) {
        self.project_chip_open = false;
        self.project_chip_menu = None;
        self.project_chip_menu_sub = None;
    }

    /// Reconcile the `list_state` item count with the live conversation length
    /// via `splice`, which preserves scroll position. Append (the common case,
    /// every user/assistant/tool item) splices new tail items in as Unmeasured;
    /// a tail removal (a `Retry` badge popped without replacement) splices the
    /// dangling slot out. Call after any direct conversation mutation that the
    /// `ApplyOutcome` path does not already cover (e.g. `push_user`/`push_notice`,
    /// which bypass `apply`).
    fn sync_list_count(&mut self, cx: &App) {
        let new_count = self.conversation.read(cx).items().len();
        if new_count == self.list_count {
            return;
        }
        if new_count > self.list_count {
            self.list_state.splice(
                self.list_count..self.list_count,
                new_count - self.list_count,
            );
        } else {
            self.list_state.splice(new_count..self.list_count, 0);
        }
        self.list_count = new_count;
    }

    /// Reconcile the `list_state` with a conversation mutation: splice the
    /// count (append/remove) and remeasure the affected index/indices. Call
    /// after any `ConversationState::apply` (the outcome tells which path) so
    /// the virtualized list's per-item height cache never goes stale.
    fn apply_list_outcome(&mut self, outcome: ApplyOutcome, cx: &App) {
        self.sync_list_count(cx);
        match outcome {
            ApplyOutcome::Remeasure(ix) => self.list_state.remeasure_items(ix..ix + 1),
            ApplyOutcome::All => {
                let n = self.conversation.read(cx).items().len();
                if n > 0 {
                    self.list_state.remeasure_items(0..n);
                }
            }
            // `None` touched no item; `Appended`/`RemovedTail` only changed the
            // count, which `sync_list_count` already spliced.
            ApplyOutcome::None | ApplyOutcome::Appended | ApplyOutcome::RemovedTail => {}
        }
    }

    /// Switch to a new thread: persist the current one, build/load the new one, re-subscribe, and rebuild the conversation view.
    fn attach_thread(&mut self, new_thread: Entity<Thread>, cx: &mut Context<Self>) {
        let old_thread = self.thread.clone();
        let old_id = old_thread.read(cx).id.0.clone();
        let new_id = new_thread.read(cx).id.0.clone();

        // If the old thread is still running a turn, park it in the background
        // so its `run_turn_loop` task stays alive (the entity is otherwise only
        // held by `self.thread`; overwriting that field would drop it and
        // silently kill the turn via `WeakEntity::upgrade() -> None`).
        if old_thread.read(cx).is_running() && old_id != new_id {
            self.background_threads.push(old_thread);
        }

        // If the new thread was previously parked in the background, reclaim it
        // so it becomes the foreground thread and is no longer double-held.
        self.background_threads
            .retain(|t| t.read(cx).id.0 != new_id);

        // Persist the old thread's current state before switching away. The
        // spawned-task save backstop in `run_turn` will persist again when the
        // turn actually finishes, capturing the final assistant messages.
        save_thread(self.thread.clone(), false, cx);

        self.thread = new_thread;
        let id = self.thread.read(cx).id.0.clone();
        let messages: Vec<agent::Message> = self.thread.read(cx).messages().to_vec();
        let usage = self.thread.read(cx).request_token_usage().clone();
        let role = self.model_label(cx);
        let weak = cx.weak_entity();
        let new_conv = cx
            .new(|cx| ConversationState::rebuild_from_messages(&messages, &usage, &role, weak, cx));
        self.conversation = new_conv;
        // Rebuild the list state for the new thread: `reset` drops the old
        // thread's measured heights and scroll position, then reveal the latest
        // turn once. A running thread keeps following the tail; a completed
        // history thread stays put once revealed so scrolling up is not snapped
        // back (the list auto-disengages tail on any upward scroll anyway).
        let count = self.conversation.read(cx).items().len();
        self.list_state.reset(count);
        self.list_count = count;
        self.list_state.scroll_to_end();
        self.list_state
            .set_follow_mode(if self.thread.read(cx).is_running() {
                FollowMode::Tail
            } else {
                FollowMode::Normal
            });
        // Hover is tied to the old thread's tick ordinals; drop it. The
        // visible-turn highlight needs no reset — it is queried live from the
        // list each frame.
        self.outline_hover = None;
        self.pending_auths.clear();
        self.pending_ask = None;
        self.pending_plan = None;
        self.thread_sub = Some(self.subscribe_thread(cx));
        // If the new thread has pending authorizations (e.g. it was parked
        // while waiting for tool approval), re-surface them so the overlay
        // appears immediately upon switching back.
        self.resurface_pending_auths(cx);
        self.sidebar
            .update(cx, |s, cx| s.set_selected(Some(id), cx));
        cx.notify();
    }

    /// Re-surface any pending authorizations on the current thread that were
    /// emitted while the thread was in the background (no subscription). Called
    /// after switching threads so the overlay appears without requiring the
    /// user to wait for the next event.
    fn resurface_pending_auths(&mut self, cx: &mut Context<Self>) {
        // Query the thread for any pending authorization metadata that was
        // stored when the auth event was originally emitted. If the thread was
        // parked waiting for user approval while in the background, re-surface
        // the events so the overlay appears immediately upon switching back.
        let entries: Vec<(String, String, String)> = self
            .thread
            .read(cx)
            .pending_auth_entries()
            .into_iter()
            .map(|(id, meta)| (id, meta.tool_name.clone(), meta.summary.clone()))
            .collect();
        for (id, tool_name, summary) in entries {
            let reason = self
                .thread
                .update(cx, |t, _cx| t.take_approval_ask_reason(&id));
            self.pending_auths.push(PendingAuth {
                id,
                tool_name,
                summary,
                reason,
            });
        }
        if !self.pending_auths.is_empty() {
            cx.notify();
        }
    }

    fn start_new_thread(&mut self, cx: &mut Context<Self>) {
        let id = ThreadId(uuid::Uuid::new_v4().to_string());
        let new = Thread::new(id, self.cwd.clone(), cx);
        self.attach_thread(new, cx);
    }

    fn open_thread(&mut self, id: String, cx: &mut Context<Self>) {
        // If the thread is already running in the background, reclaim it
        // instead of loading a stale snapshot from the db.
        if let Some(pos) = self
            .background_threads
            .iter()
            .position(|t| t.read(cx).id.0 == id)
        {
            let thread = self.background_threads.remove(pos);
            self.attach_thread(thread, cx);
            return;
        }
        let store = self.sidebar.read(cx).store();
        let Some(loaded) = store.update(cx, |s, cx| s.load_thread(&id, cx)) else {
            return;
        };
        self.attach_thread(loaded, cx);
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

        // Slash commands (line-initial `/name [args]`) are intercepted before
        // sending a normal user turn. A recognized command fully handles the
        // input (Handled), asks to inject text as a user turn (InjectUserTurn),
        // or declines (NoOp → fall through to the normal path). Slash parsing
        // only applies to text-only input; attachments force the normal path.
        // Markdown prompt-macro commands (`/gitwork:deliver …`) are registered
        // into the same registry as `MarkdownSlashCommand` adapters and dispatch
        // into `run_command_turn` → `Thread::submit_command`, which substitutes
        // `$ARGUMENTS` and applies the command's `allowed-tools` filter.
        if attachments.is_empty()
            && let Some(parsed) = crate::slash_command::parse(&text)
        {
            let result = crate::slash_command::dispatch(&parsed, self, window, cx);
            match result {
                crate::slash_command::SlashResult::Handled => return,
                crate::slash_command::SlashResult::InjectUserTurn(msg) => {
                    self.send_user_turn(msg, Vec::new(), cx);
                    return;
                }
                crate::slash_command::SlashResult::NoOp => {}
            }
        }

        if attachments.is_empty() {
            self.send_user_turn(text, Vec::new(), cx);
            return;
        }

        // Reading attachment bytes is blocking IO; do it off the UI thread, then start the turn.
        cx.spawn(async move |this, cx| {
            let (text, extra, failed) = cx
                .background_spawn(async move {
                    let mut text = text;
                    let mut extra = Vec::new();
                    let mut failed = 0usize;
                    for att in &attachments {
                        match att {
                            PendingAttachment::ClipboardImage(img) => {
                                match agent::image::gpui_image_to_message_content(
                                    std::sync::Arc::new(img.clone()),
                                ) {
                                    Some(content) => extra.push(content),
                                    None => failed += 1,
                                }
                            }
                            PendingAttachment::File { .. } => {
                                if let Some(content) = load_attachment(att, &mut text) {
                                    extra.push(content);
                                }
                            }
                        }
                    }
                    (text, extra, failed)
                })
                .await;
            this.update(cx, |this, cx| {
                this.send_user_turn(text, extra, cx);
                if failed > 0 {
                    let weak = cx.weak_entity();
                    this.conversation.update(cx, |c, cx| {
                        c.push_notice(
                            i18n::t("composer-image-process-failed").to_string(),
                            weak,
                            cx,
                        );
                    });
                    this.sync_list_count(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Stage a clipboard image as a pending attachment chip. Resize happens
    /// off-thread on submit; here we only record the image and re-render.
    fn handle_pasted_image(&mut self, image: gpui::Image, cx: &mut Context<Self>) {
        self.pending_attachments
            .push(PendingAttachment::ClipboardImage(image));
        cx.notify();
    }

    /// Run a markdown prompt-macro slash command turn. The display text
    /// (`/name args`) is shown to the user as the user bubble; `Thread::submit_command`
    /// substitutes `$ARGUMENTS` into the command body and applies the command's
    /// `allowed-tools` whitelist for the turn. An unknown command (adapter
    /// registered but the data registry miss — shouldn't normally happen)
    /// surfaces an error and drops the turn.
    pub(crate) fn run_command_turn(&mut self, name: &str, args: &str, cx: &mut Context<Self>) {
        let display_text = if args.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {args}")
        };
        let meta = self.user_turn_meta(cx);
        let weak = cx.weak_entity();
        self.conversation.update(cx, |c, cx| {
            c.push_user(display_text, Vec::new(), meta, weak, cx)
        });
        // Splice the new user item into the list, then re-engage tail-follow so
        // the streaming reply stays in view.
        self.sync_list_count(cx);
        self.list_state.set_follow_mode(FollowMode::Tail);
        let hit = self
            .thread
            .update(cx, |thread, cx| thread.submit_command(name, args, cx));
        if !hit {
            self.thread.update(cx, |_, cx| {
                cx.emit(agent::ThreadEvent::Error(anyhow::anyhow!(
                    "{}",
                    i18n::t_str("workspace-unknown-command", &[("name", name)])
                )));
            });
        }
        // Persist on command submit so the sidebar shows the new entry immediately.
        save_thread(self.thread.clone(), true, cx);
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
        let meta = self.user_turn_meta(cx);
        let ui = Self::message_ui_metadata(&meta);
        let weak = cx.weak_entity();
        let user_images = Self::decode_user_images(&images);
        self.conversation.update(cx, |c, cx| {
            c.push_user(text.clone(), user_images, meta, weak, cx)
        });
        // Splice the new user item in and re-engage tail-follow so the streaming
        // reply stays pinned as it grows.
        self.sync_list_count(cx);
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.thread.update(cx, |thread, cx| {
            if images.is_empty() {
                thread.insert_user_message_with_ui_metadata(text, Some(ui), cx);
            } else {
                let mut content = Vec::with_capacity(images.len() + 1);
                if !text.trim().is_empty() {
                    content.push(MessageContent::Text(text));
                }
                content.extend(images);
                thread.insert_user_message_with_content_and_ui_metadata(content, Some(ui), cx);
            }
            thread.run_turn(cx);
        });
        // Persist on submit so the sidebar shows the new entry immediately.
        save_thread(self.thread.clone(), true, cx);
        cx.notify();
    }

    /// Decode provider-bound image contents into UI-preview `UserImage`s. The
    /// canonical `MessageContent::Image` bytes still go to the thread; this
    /// only rebuilds a gpui image for the user bubble.
    fn decode_user_images(images: &[agent::language_model::MessageContent]) -> Vec<UserImage> {
        use agent::language_model::MessageContent;
        use base64::Engine as _;
        images
            .iter()
            .filter_map(|c| match c {
                MessageContent::Image { data, mime_type } => {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(data.as_bytes())
                        .ok()?;
                    let fmt = gpui::ImageFormat::from_mime_type(mime_type.as_str())?;
                    Some(UserImage(std::sync::Arc::new(gpui::Image::from_bytes(
                        fmt, bytes,
                    ))))
                }
                _ => None,
            })
            .collect()
    }

    fn toggle_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor_open {
            self.close_editor(window, cx);
        } else {
            self.open_editor(window, cx);
        }
    }

    /// Whether the right pane has any tab (Editor or Member) showing.
    fn right_pane_open(&self) -> bool {
        !self.right_tabs.is_empty()
    }

    /// Index of the Editor tab, if present.
    fn editor_tab_ix(&self) -> Option<usize> {
        self.right_tabs
            .iter()
            .position(|t| matches!(t, RightTab::Editor))
    }

    /// Make `ix` the active right-pane tab and sync `editor_open` to whether it
    /// is the Editor tab (the Editor tab hides the inline composer; a Member
    /// tab leaves it usable).
    fn set_active_right_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix < self.right_tabs.len() {
            self.active_right_tab = ix;
        }
        self.editor_open = self
            .right_tabs
            .get(self.active_right_tab)
            .is_some_and(|t| matches!(t, RightTab::Editor));
        cx.notify();
    }

    /// Open the markdown editor: hide the inline composer and transfer its draft
    /// into the editor so writing continues there. If an Editor tab is already
    /// present, just focus it. Submit from the editor with Cmd-Enter; close with
    /// Ctrl-G / Cmd-W to move the draft back.
    fn open_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Close any open inline menus so they don't linger behind the hidden footer.
        self.close_slash_menu();
        self.close_plus_menu();
        if let Some(ix) = self.editor_tab_ix() {
            self.set_active_right_tab(ix, cx);
            return;
        }
        let draft = self.input_state.read(cx).value().to_string();
        self.right_tabs.push(RightTab::Editor);
        let ix = self.right_tabs.len() - 1;
        self.editor_preview = false;
        self.editor_state.update(cx, |s, cx| {
            s.set_value(draft, window, cx);
            s.focus(window, cx);
        });
        self.input_state
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.set_active_right_tab(ix, cx);
    }

    /// Close the Editor tab without submitting: move the draft back into the
    /// inline composer and reveal it again. No-op when no Editor tab is present.
    fn close_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.editor_tab_ix() else {
            return;
        };
        let draft = self.editor_state.read(cx).value().to_string();
        self.right_tabs.remove(ix);
        self.editor_preview = false;
        self.input_state.update(cx, |s, cx| {
            s.set_value(draft, window, cx);
            s.focus(window, cx);
        });
        self.editor_state
            .update(cx, |s, cx| s.set_value("", window, cx));
        if self.active_right_tab >= self.right_tabs.len() {
            self.active_right_tab = self.right_tabs.len().saturating_sub(1);
        }
        self.editor_open = self
            .right_tabs
            .get(self.active_right_tab)
            .is_some_and(|t| matches!(t, RightTab::Editor));
        cx.notify();
    }

    /// Focus a member's observation tab, creating it if absent. The panel is
    /// built from the member's `Thread` + role, read off the leader's active
    /// team; no-op if no team or no such member.
    fn open_member_tab(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(ix) = self
            .right_tabs
            .iter()
            .position(|t| matches!(t, RightTab::Member(n) if n == name))
        {
            self.set_active_right_tab(ix, cx);
            return;
        }
        let Some(team) = self.thread.read(cx).team().cloned() else {
            return;
        };
        let Some(member_thread) = team.read(cx).thread_for(name).cloned() else {
            return;
        };
        let role = team
            .read(cx)
            .members()
            .get(name)
            .map(|m| m.role().to_string())
            .unwrap_or_else(|| name.to_string());
        let weak = cx.weak_entity();
        let panel = MemberPanel::new(
            member_thread,
            name.to_string(),
            role,
            team.downgrade(),
            weak,
            cx,
        );
        self.member_panels.insert(name.to_string(), panel);
        self.right_tabs.push(RightTab::Member(name.to_string()));
        let ix = self.right_tabs.len() - 1;
        self.set_active_right_tab(ix, cx);
    }

    /// Close a right-pane tab by index. The Editor tab routes through
    /// `close_editor` to preserve draft-transfer semantics; a Member tab drops
    /// its panel.
    fn close_right_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.right_tabs.get(ix), Some(RightTab::Editor)) {
            self.close_editor(window, cx);
            return;
        }
        if let Some(RightTab::Member(name)) = self.right_tabs.get(ix).cloned() {
            self.right_tabs.remove(ix);
            self.member_panels.remove(&name);
            if self.active_right_tab >= self.right_tabs.len() {
                self.active_right_tab = self.right_tabs.len().saturating_sub(1);
            }
            self.editor_open = self
                .right_tabs
                .get(self.active_right_tab)
                .is_some_and(|t| matches!(t, RightTab::Editor));
            cx.notify();
        }
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
        let meta = self.user_turn_meta(cx);
        let ui = Self::message_ui_metadata(&meta);
        let weak = cx.weak_entity();
        self.conversation.update(cx, |c, cx| {
            c.push_user(text.clone(), Vec::new(), meta, weak, cx)
        });
        self.sync_list_count(cx);
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.thread.update(cx, |thread, cx| {
            thread.insert_user_message_with_ui_metadata(text, Some(ui), cx);
            thread.run_turn(cx);
        });
        save_thread(self.thread.clone(), true, cx);
        self.editor_state.update(cx, |state, cx| {
            state.set_value("", window, cx);
        });
        // Drop the Editor tab (the turn is submitted); re-anchor to any
        // surviving Member tab and clear the draft-backed editor state.
        if let Some(ix) = self.editor_tab_ix() {
            self.right_tabs.remove(ix);
        }
        if self.active_right_tab >= self.right_tabs.len() {
            self.active_right_tab = self.right_tabs.len().saturating_sub(1);
        }
        self.editor_open = self
            .right_tabs
            .get(self.active_right_tab)
            .is_some_and(|t| matches!(t, RightTab::Editor));
        self.editor_preview = false;
        self.input_state.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    fn user_turn_meta(&self, cx: &mut Context<Self>) -> UserTurnMeta {
        let approval_mode = self.thread.read(cx).approval_mode();
        UserTurnMeta::new(
            chrono::Utc::now().timestamp(),
            self.model_label(cx),
            Some(approval_mode),
        )
    }

    fn message_ui_metadata(meta: &UserTurnMeta) -> agent::MessageUiMetadata {
        agent::MessageUiMetadata {
            model_id: (!meta.model_id.is_empty()).then(|| meta.model_id.clone()),
            approval_mode: meta.approval_mode.map(|mode| mode.as_i64()),
        }
    }

    pub(crate) fn model_label(&self, cx: &mut Context<Self>) -> String {
        self.thread
            .read(cx)
            .model()
            .map(|m| m.name().to_string())
            .unwrap_or_else(|| i18n::t("workspace-no-model").to_string())
    }

    /// Pin / unpin the active thread. The DB write + sidebar refresh runs
    /// through `ThreadStore::pin_thread`; the in-memory `Thread` mirror is
    /// flipped first so the menu label updates immediately on the next
    /// re-open. Notifies the workspace so the menu trigger re-renders.
    fn title_menu_toggle_pin(&mut self, cx: &mut Context<Self>) {
        let id = self.thread.read(cx).id.0.clone();
        let next = !self.thread.read(cx).is_pinned();
        self.thread.update(cx, |t, cx| t.set_pinned(next, cx));
        let store = agent::thread_store_global();
        store.update(cx, |s, cx| s.pin_thread(&id, next, cx));
        let msg = if next {
            i18n::t("titlebar-pinned-notice")
        } else {
            i18n::t("titlebar-unpinned-notice")
        };
        self.add_info_message(msg.to_string(), cx);
    }

    /// Archive / unarchive the active thread. Mirrors the sidebar archive
    /// action: toggle the thread's archived flag, persist via
    /// `ThreadStore::archive_thread` (which drops its row from the list when
    /// archived, default `include_archived=false`), and notice the user.
    /// The in-memory `Thread` mirror is flipped first so the menu label
    /// updates immediately on the next re-open. Switching to a new thread
    /// is left to the sidebar — the menu just persists the toggle.
    fn title_menu_archive(&mut self, cx: &mut Context<Self>) {
        let id = self.thread.read(cx).id.0.clone();
        let next = !self.thread.read(cx).archived();
        self.thread.update(cx, |t, cx| t.set_archived(next, cx));
        let store = agent::thread_store_global();
        store.update(cx, |s, cx| s.archive_thread(&id, next, cx));
        let msg = if next {
            i18n::t("titlebar-archive-notice")
        } else {
            i18n::t("titlebar-unarchive-notice")
        };
        self.add_info_message(msg.to_string(), cx);
    }

    /// Copy a string to the system clipboard, then push a localized notice
    /// so the user sees what landed in the clipboard. Single funnel for all
    /// `titlebar-copy-*` actions so the notice phrasing stays consistent.
    fn title_menu_copy(&mut self, label_key: &str, value: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(value));
        self.add_info_message(i18n::t(label_key).to_string(), cx);
    }

    /// Push a system-styled notice into the conversation (no thread message,
    /// no model turn). Used by slash commands to report outcomes — e.g. the
    /// `/yolo` toggle acknowledging the mode change. Renders as a neutral-toned
    /// `ConvItem::Notice` card (distinct from the red `ConvItem::Error`).
    pub fn add_info_message(&mut self, text: String, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        self.conversation.update(cx, |c, cx| {
            c.push_notice(text, weak, cx);
        });
        // Splice the notice item in. If tail-follow is engaged the list reveals
        // it; if the user scrolled up, `FollowMode::Tail` has already
        // disengaged so the viewport stays put.
        self.sync_list_count(cx);
        cx.notify();
    }

    /// Toggle YOLO mode on the current thread. Pushes a notice so the user
    /// sees the state change in the conversation. Called by the `/yolo` slash
    /// command and the access-chip dropdown.
    pub(crate) fn toggle_yolo(&mut self, cx: &mut Context<Self>) {
        // `/yolo` (no args) flips between full access and request-approval —
        // `AutoReview` is its own state and is only reachable via the chip
        // popover, since slash-command users explicitly want "the other
        // extreme" rather than the middle tier.
        let next = if self.thread.read(cx).approval_mode() == ApprovalMode::Yolo {
            ApprovalMode::OnRequest
        } else {
            ApprovalMode::Yolo
        };
        self.apply_approval_mode(next, cx);
    }

    /// Enable full access and immediately send `prompt` as a user turn (the
    /// `/yolo [prompt]` form). If full access is already on it stays on;
    /// the prompt still runs.
    pub(crate) fn start_yolo_turn(&mut self, prompt: String, cx: &mut Context<Self>) {
        if self.thread.read(cx).approval_mode() != ApprovalMode::Yolo {
            self.apply_approval_mode(ApprovalMode::Yolo, cx);
        }
        self.send_user_turn(prompt, Vec::new(), cx);
    }

    fn resolve_auth(&mut self, decision: PermissionDecision, cx: &mut Context<Self>) {
        // When an AskUserQuestion card is open its "Cancel" button calls this; the
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
            self.ask_step = 0;
            self.ask_transition_gen = self.ask_transition_gen.wrapping_add(1);
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

    /// Allocate the per-question `InputState` entities for the ask drawer on
    /// first render. `InputState::new` needs a `Window`, which the event
    /// handler lacks, so creation is deferred to here.
    fn ensure_ask_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ask) = self.pending_ask.as_mut() else {
            return;
        };
        if ask.others.len() == ask.questions.len() && ask.response_input.is_some() {
            return;
        }
        ask.others = (0..ask.questions.len())
            .map(|_| {
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .multi_line(true)
                        .auto_grow(2, 6)
                        .placeholder(i18n::t("workspace-clarify-other"))
                })
            })
            .collect();
        ask.response_input = Some(cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(2, 6)
                .placeholder(i18n::t("workspace-ask-response"))
        }));
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

    fn ask_prev(&mut self, cx: &mut Context<Self>) {
        if self.ask_step > 0 {
            self.ask_step -= 1;
            cx.notify();
        }
    }

    fn ask_next(&mut self, cx: &mut Context<Self>) {
        if let Some(ask) = self.pending_ask.as_ref()
            && self.ask_step < ask.questions.len() - 1
        {
            self.ask_step += 1;
            cx.notify();
        }
    }

    fn on_ask_prev(&mut self, _: &AskPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.ask_prev(cx);
    }

    fn on_ask_next(&mut self, _: &AskNext, _: &mut Window, cx: &mut Context<Self>) {
        self.ask_next(cx);
    }

    /// Submit the ask drawer: gather answers (per-question "Other" text
    /// overrides option selections). If the free-form response field has
    /// content, it overrides all per-question answers.
    fn resolve_ask(&mut self, cx: &mut Context<Self>) {
        let ask = match self.pending_ask.take() {
            Some(a) => a,
            None => return,
        };
        let response_text = ask
            .response_input
            .as_ref()
            .map(|s| s.read(cx).value().trim().to_string())
            .unwrap_or_default();
        let response = if response_text.is_empty() {
            None
        } else {
            Some(response_text)
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
        self.ask_step = 0;
        self.ask_transition_gen = self.ask_transition_gen.wrapping_add(1);
        self.thread.update(cx, |thread, cx| {
            thread.respond_authorization(
                &id,
                agent::ToolAuthorizationResponse::AskUserQuestion { answers, response },
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

    /// Resolve the pending plan approval (approve/reject from the overlay).
    fn respond_plan(&mut self, approve: bool, cx: &mut Context<Self>) {
        let Some(plan) = self.pending_plan.take() else {
            return;
        };
        self.thread.update(cx, |thread, cx| {
            thread.respond_plan_approval(
                &plan.id,
                if approve {
                    PlanApprovalResponse::Approve
                } else {
                    PlanApprovalResponse::Reject
                },
                cx,
            );
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
        let reason = auth.reason.clone();
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
                // Scrim must use the dark foreground, not `background`. A white
                // veil over a white conversation does not dim, so the page shows
                // through and the modal reads as transparent.
                .bg(theme.foreground.opacity(0.6))
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
                                        .child(i18n::t("workspace-approval-title")),
                                ),
                        )
                        .child(
                            gpui::div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(i18n::t_str(
                                    "workspace-approval-tool",
                                    &[("name", tool_name.as_str())],
                                )),
                        )
                        // Auto-review reason: a one-line muted note saying why the
                        // reviewer escalated the call. Sourced from the thread's
                        // `approval_ask_reasons` map; absent for tools that came
                        // through `OnRequest` or `Yolo` paths.
                        .children(reason.as_deref().map(|reason| {
                            gpui::div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(i18n::t_str(
                                    "workspace-approval-auto-review-note",
                                    &[("reason", reason)],
                                ))
                        }))
                        .children(if queued > 0 {
                            Some(
                                gpui::div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(i18n::t_count("workspace-queued", queued as i64)),
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
                                        .label(i18n::t("workspace-deny"))
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
                                        .label(i18n::t("workspace-always-allow"))
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
                                        .label(i18n::t("workspace-allow-once"))
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

    /// Plan approval overlay (model called `exit_plan_mode`). Renders the
    /// submitted plan as markdown so the user can read it before deciding;
    /// Approve exits plan mode and starts execution, Reject ends the turn
    /// and stays in plan mode awaiting the user's next message. Auth/ask
    /// overlays take precedence so they never compete.
    fn render_plan_approval_overlay(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.pending_ask.is_some() || !self.pending_auths.is_empty() {
            return None;
        }
        self.pending_plan.as_ref()?;

        let plan_text = self.pending_plan.as_ref()?.plan_text.clone();

        Some(
            gpui::div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                // Scrim must use the dark foreground, not `background`. A white
                // veil over a white conversation does not dim, so the page shows
                // through and the modal reads as transparent.
                .bg(theme.foreground.opacity(0.6))
                .child(
                    v_flex()
                        .w(px(560.))
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
                                .child(
                                    Icon::new(IconName::LayoutDashboard)
                                        .small()
                                        .text_color(theme.accent),
                                )
                                .child(
                                    gpui::div()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child(i18n::t("workspace-plan-approval-title")),
                                ),
                        )
                        .child(
                            gpui::div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(i18n::t("workspace-plan-approval-question")),
                        )
                        .child(
                            // The submitted plan body. Scrollable so a long plan
                            // never overflows the viewport; the wrapper is
                            // stateful so `overflow_y_scroll` can keep scroll
                            // state, and `Markdown` additionally scrolls its
                            // own surface.
                            gpui::div()
                                .id("plan-approval-body-scroll")
                                .flex_1()
                                .min_h_0()
                                .max_h(px(420.))
                                .overflow_y_scroll()
                                .rounded(theme.radius)
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.secondary)
                                .p_3()
                                .child(
                                    Markdown::new("plan-approval-body", plan_text)
                                        .theme(theme)
                                        .selectable(true)
                                        .scrollable(true)
                                        .heading_mode(HeadingMode::Uniform),
                                ),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("plan-continue")
                                        .ghost()
                                        .small()
                                        .label(i18n::t("workspace-plan-continue"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.respond_plan(false, cx);
                                        })),
                                )
                                .child(
                                    Button::new("plan-approve")
                                        .primary()
                                        .small()
                                        .label(i18n::t("workspace-plan-approve"))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.respond_plan(true, cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Ask drawer: replaces the composer footer while an `AskUserQuestion`
    /// card is open. Shows one question at a time with stepper navigation,
    /// checkbox/radio indicators, per-question "Other" input, and a free-form
    /// response field.
    fn render_ask_drawer(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let ask = self
            .pending_ask
            .as_ref()
            .expect("render_ask_drawer called without pending_ask");
        let step = self.ask_step.min(ask.questions.len() - 1);
        let q = &ask.questions[step];
        let sel = ask.selections.get(step);
        let other = ask.others.get(step).cloned();
        let response_input = ask.response_input.clone();
        let total = ask.questions.len();
        let transition_gen = self.ask_transition_gen;

        // --- Header: title + stepper ---
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

        // --- Question row: header tag + question text ---
        let question_row = h_flex()
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
            );

        // --- Options with checkbox/radio indicators ---
        let mut options_block = v_flex().gap_1p5();
        for (oi, opt) in q.options.iter().enumerate() {
            let selected = sel.and_then(|s| s.get(oi).copied()).unwrap_or(false);
            let indicator_size = px(16.);
            let indicator = if q.multi_select {
                // Checkbox: square with check mark when selected
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
            } else {
                // Radio: filled dot when selected, hollow circle otherwise
                if selected {
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
                }
            };
            let option_row = h_flex()
                .gap_2()
                .items_start()
                .id(gpui::SharedString::from(format!("ask-opt-{step}-{oi}")))
                .cursor(CursorStyle::PointingHand)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_ask_option(step, oi, cx);
                }))
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

        // --- "Other" input ---
        let mut other_block = v_flex().gap_1();
        if let Some(state) = other {
            other_block = other_block
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(i18n::t("workspace-clarify-other")),
                )
                .child(Input::new(&state));
        }

        // --- Free-form response input ---
        let response_block = if let Some(state) = response_input {
            v_flex()
                .gap_1()
                .child(gpui::div().h(px(1.)).w_full().bg(theme.border).mt_1())
                .child(Input::new(&state))
        } else {
            v_flex()
        };

        // --- Navigation bar: prev/next + cancel/submit ---
        let can_prev = step > 0;
        let can_next = step < total - 1;
        let nav = h_flex()
            .gap_2()
            .items_center()
            .justify_between()
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("ask-prev")
                            .ghost()
                            .small()
                            .icon(IconName::ChevronLeft)
                            .label(i18n::t("workspace-ask-prev"))
                            .when(!can_prev, |b| b.disabled(true))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.ask_prev(cx);
                            })),
                    )
                    .child(
                        Button::new("ask-next")
                            .ghost()
                            .small()
                            .icon(IconName::ChevronRight)
                            .label(i18n::t("workspace-ask-next"))
                            .when(!can_next, |b| b.disabled(true))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.ask_next(cx);
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("ask-cancel")
                            .ghost()
                            .small()
                            .label(i18n::t("workspace-cancel"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.resolve_auth(PermissionDecision::Deny, cx);
                            })),
                    )
                    .child(
                        Button::new("ask-submit")
                            .primary()
                            .small()
                            .label(i18n::t("workspace-submit"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.resolve_ask(cx);
                            })),
                    ),
            );

        // --- Assemble card with slide-up animation ---
        let anim_id = format!("ask-slide-{transition_gen}");
        let card = v_flex()
            .w_full()
            .gap_3()
            .p_3()
            .rounded(theme.radius)
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .shadow_lg()
            .child(header)
            .child(question_row)
            .child(options_block)
            .child(other_block)
            .child(response_block)
            .child(nav)
            .with_animation(
                anim_id,
                Animation::new(Duration::from_millis(SLIDE_MS)).with_easing(ease_out_quint()),
                |el, delta| {
                    // Slide up: offset goes from +120px (below) → 0 (in place).
                    let offset = px(120.) * (1.0 - delta);
                    el.mt(offset)
                },
            );

        // Keyboard context for AskPrev/AskNext/Cancel actions.
        v_flex()
            .id("ask-drawer")
            .key_context("AskDrawer")
            .on_action(cx.listener(Workspace::on_ask_prev))
            .on_action(cx.listener(Workspace::on_ask_next))
            .on_action(cx.listener(|this, _: &AskCancel, _, cx| {
                this.resolve_auth(PermissionDecision::Deny, cx);
            }))
            .child(card)
            .into_any_element()
    }

    fn render_reasoning_effort_selector(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.effort_open;
        let selected = self.thread.read(cx).reasoning_effort();
        let workspace = cx.entity().downgrade();
        // Effort enum values are provider wire literals (low/medium/high/...),
        // not UI chrome — they are not localized.
        let label = selected.wire_value();

        let trigger = h_flex()
            .id("reasoning-effort-chip")
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.accent.opacity(0.08)))
            .cursor_pointer()
            .child(
                Icon::new(IconName::Cpu)
                    .xsmall()
                    .text_color(theme.muted_foreground),
            )
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.foreground)
                    .child(label),
            )
            .child(
                Icon::new(if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(theme.muted_foreground),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                if this.effort_open {
                    this.close_effort_menu();
                    cx.notify();
                    return;
                }

                let current = this.thread.read(cx).reasoning_effort();
                this.effort_open = true;
                let menu_workspace = workspace.clone();
                let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
                    let mut menu = menu
                        .max_w(gpui::px(220.))
                        .label(i18n::t("workspace-effort-section"));
                    for effort in ReasoningEffort::ALL {
                        let ws = menu_workspace.clone();
                        menu = menu.item(
                            PopupMenuItem::new(effort.wire_value())
                                .checked(effort == current)
                                .on_click(move |_, _window, cx| {
                                    let _ = ws.update(cx, |this, cx| {
                                        this.thread
                                            .update(cx, |t, cx| t.set_reasoning_effort(effort, cx));
                                        this.close_effort_menu();
                                        cx.notify();
                                    });
                                }),
                        );
                    }
                    menu
                });
                let sub = cx.subscribe(
                    &menu,
                    |this: &mut Workspace,
                     _menu: Entity<PopupMenu>,
                     _: &DismissEvent,
                     cx: &mut Context<Workspace>| {
                        this.close_effort_menu();
                        cx.notify();
                    },
                );
                this.effort_menu = Some(menu);
                this.effort_menu_sub = Some(sub);
                cx.notify();
            }));

        if !open {
            return trigger.into_any_element();
        }

        let menu = self
            .effort_menu
            .clone()
            .expect("effort_menu exists when open");
        gpui::div()
            .relative()
            .child(trigger)
            .child(
                gpui::div()
                    .id("reasoning-effort-dropdown")
                    .absolute()
                    .bottom_full()
                    .right_0()
                    .occlude()
                    .child(menu),
            )
            .into_any_element()
    }

    /// Cascading model selector using PopupMenu with Provider → Model submenus.
    ///
    /// Closed: a ghost button showing the current model with a chevron.
    /// Open: an absolute-positioned PopupMenu; hovering a Provider row expands
    /// a flyout submenu listing its Models. PopupMenu handles all hover,
    /// click-outside, and keyboard-dismiss behavior internally.
    fn render_model_selector(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let label = self.model_label(cx);
        let open = self.model_open;

        let trigger = h_flex()
            .id("model-trigger")
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.accent.opacity(0.08)))
            .cursor_pointer()
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.foreground)
                    .child(label),
            )
            .child(
                Icon::new(if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(theme.muted_foreground),
            )
            .on_click(cx.listener(|this, _, window, cx| {
                if this.model_open {
                    this.model_open = false;
                    this.model_menu = None;
                    this.model_menu_sub = None;
                } else {
                    this.model_open = true;
                    let workspace = cx.entity().downgrade();
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
        workspace: WeakEntity<Workspace>,
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
                            let _ = ws.update(cx, |this, cx| {
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

    /// Title bar "..." trigger + dropdown (conversation menu).
    ///
    /// Closed: a small ghost icon button (horizontal ellipsis) next to the
    /// session title. Open: an absolute-positioned PopupMenu anchored under
    /// the button. Mirrors the model selector pattern: the menu entity and
    /// its dismiss subscription are created lazily on open, dropped on close.
    fn render_title_menu_trigger(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use crate::views::title_menu::{TitleMenuCallbacks, build_title_menu};

        let open = self.title_menu_open;
        let is_pinned = self.thread.read(cx).is_pinned();
        let is_archived = self.thread.read(cx).archived();

        let trigger = Button::new("titlebar-trigger")
            .ghost()
            .small()
            .icon(IconName::Ellipsis)
            .on_click(cx.listener(move |this, _, window, cx| {
                if this.title_menu_open {
                    this.close_title_menu();
                    cx.notify();
                    return;
                }
                this.title_menu_open = true;
                let workspace = cx.entity().downgrade();
                let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
                    let cb = TitleMenuCallbacks {
                        on_pin: {
                            let ws = workspace.clone();
                            Rc::new(move |_, _, cx| {
                                let _ = ws.update(cx, |this, cx| this.title_menu_toggle_pin(cx));
                            })
                        },
                        on_archive: {
                            let ws = workspace.clone();
                            Rc::new(move |_, _, cx| {
                                let _ = ws.update(cx, |this, cx| this.title_menu_archive(cx));
                            })
                        },
                        on_copy_id: {
                            let ws = workspace.clone();
                            Rc::new(move |_, _, cx| {
                                let _ = ws.update(cx, |this, cx| {
                                    let id = this.thread.read(cx).id.0.clone();
                                    this.title_menu_copy("titlebar-copied-id", id, cx);
                                });
                            })
                        },
                        on_copy_markdown: {
                            let ws = workspace.clone();
                            Rc::new(move |_, _, cx| {
                                let _ = ws.update(cx, |this, cx| {
                                    let md = this.thread.read(cx).to_markdown();
                                    this.title_menu_copy("titlebar-copied-markdown", md, cx);
                                });
                            })
                        },
                        on_copy_cwd: {
                            let ws = workspace.clone();
                            Rc::new(move |_, _, cx| {
                                let _ = ws.update(cx, |this, cx| {
                                    let cwd = this.thread.read(cx).cwd().display().to_string();
                                    this.title_menu_copy("titlebar-copied-cwd", cwd, cx);
                                });
                            })
                        },
                        on_copy_deeplink: {
                            let ws = workspace.clone();
                            Rc::new(move |_, _, cx| {
                                let _ = ws.update(cx, |this, cx| {
                                    let id = this.thread.read(cx).id.0.clone();
                                    let link = format!("manox://thread/{id}");
                                    this.title_menu_copy("titlebar-copied-deeplink", link, cx);
                                });
                            })
                        },
                        on_schedule: {
                            let ws = workspace.clone();
                            Rc::new(move |_, _, cx| {
                                let _ = ws.update(cx, |this, cx| {
                                    this.add_info_message(
                                        i18n::t("titlebar-not-implemented").to_string(),
                                        cx,
                                    );
                                });
                            })
                        },
                        on_new_window: {
                            let ws = workspace.clone();
                            Rc::new(move |_, _, cx| {
                                let _ = ws.update(cx, |this, cx| {
                                    this.add_info_message(
                                        i18n::t("titlebar-not-implemented").to_string(),
                                        cx,
                                    );
                                });
                            })
                        },
                        is_pinned,
                        is_archived,
                    };
                    build_title_menu(menu, window, cx, cb)
                });
                let sub = cx.subscribe(
                    &menu,
                    |this: &mut Workspace,
                     _menu: Entity<PopupMenu>,
                     _: &DismissEvent,
                     cx: &mut Context<Workspace>| {
                        this.close_title_menu();
                        cx.notify();
                    },
                );
                this.title_menu = Some(menu);
                this.title_menu_sub = Some(sub);
                cx.notify();
            }));

        // Color the trigger when open so the affordance matches the dropdown's
        // presence (a clicked "..." otherwise looks identical to a hovered one).
        let trigger = if open {
            trigger.text_color(theme.accent)
        } else {
            trigger
        };

        if !open {
            return trigger.into_any_element();
        }

        let menu = self
            .title_menu
            .clone()
            .expect("title_menu exists when open");

        gpui::div()
            .relative()
            .child(trigger)
            .child(
                gpui::div()
                    .id("titlebar-dropdown")
                    .absolute()
                    .top_full()
                    .left_0()
                    .occlude()
                    .child(menu),
            )
            .into_any_element()
    }

    /// Floating environment info card at the top-right of the conversation
    /// area. Shows project, branch, model, per-model token usage (animated),
    /// approval modes, and sources. Only rendered once the thread has been
    /// interacted with and the editor pane is closed.
    fn render_environment_panel(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let model_label = truncate_env_model_id(self.model_label(cx));
        let (project, approval_mode, per_model) = {
            let thread = self.thread.read(cx);
            (
                thread.project().cloned(),
                thread.approval_mode(),
                thread.per_model_token_usage().clone(),
            )
        };
        let local_label = self
            .cwd
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_else(|| self.cwd.to_str().unwrap_or("workspace"))
            .to_string();
        let branch_label = if project.is_some() {
            "main".to_string()
        } else {
            i18n::t("workspace-env-no-project").to_string()
        };
        let muted = theme.muted_foreground;

        // Build per-model token rows, sorted by total usage descending.
        let mut model_rows: Vec<_> = per_model
            .into_iter()
            .filter(|(_, u)| u.total_tokens() > 0)
            .collect();
        model_rows.sort_by_key(|b| std::cmp::Reverse(b.1.total_tokens()));

        let token_section = v_flex().gap_1().child(
            h_flex()
                .items_center()
                .gap_2()
                .child(Icon::new(IconName::MemoryStick).xsmall().text_color(muted))
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(i18n::t("workspace-env-usage")),
                ),
        );
        // Each model renders as two rows: input/output on line one, cache
        // read/cache creation on line two (indented under the model id). Static
        // text keeps the four-column layout readable without an odometer fight.
        let token_model_rows: Vec<AnyElement> = model_rows
            .into_iter()
            .map(|(model_name, usage)| {
                let model_display = truncate_env_model_id(model_name);
                let line_one = h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(muted)
                    .child(gpui::div().w(px(80.)).truncate().child(model_display))
                    .child(counter_text("↑", usage.input_tokens))
                    .child(counter_text("↓", usage.output_tokens));
                let line_two = h_flex()
                    .pl(px(80. + 8.))
                    .gap_2()
                    .text_xs()
                    .text_color(muted)
                    .child(counter_text_cached("↑", usage.cache_read_input_tokens))
                    .child(counter_text_cached("↓", usage.cache_creation_input_tokens));
                v_flex()
                    .gap_0p5()
                    .child(line_one)
                    .child(line_two)
                    .into_any_element()
            })
            .collect();

        // The env panel floats over the conversation area (absolute, top-right),
        // sitting below the title-bar overlay. `ENV_CONTENT_INSET` in the body
        // wrapper's `pr()` prevents content from being hidden behind the card.
        v_flex()
            .absolute()
            .top(TITLE_BAR_HEIGHT + px(16.))
            .right(px(16.))
            .w(px(ENV_CARD_WIDTH))
            .occlude()
            .child(
                v_flex()
                    .w_full()
                    .p_4()
                    .gap_3()
                    // Directional drop shadow (offset down-left) so the card
                    // reads as floating above the conversation — the offset
                    // pushes the shadow to the left and bottom edges, leaving
                    // the top/right lit, like a card lifted from the top-right.
                    .shadow(std::vec![
                        gpui::BoxShadow::new(px(-3.), px(6.), gpui::hsla(0., 0., 0., 0.22))
                            .blur_radius(px(10.))
                    ])
                    .rounded(theme.radius)
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                gpui::div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme.foreground)
                                    .child(i18n::t("workspace-env-title")),
                            )
                            .child(Button::new("env-add").ghost().xsmall().icon(IconName::Plus)),
                    )
                    .child(env_row(
                        IconName::Frame,
                        i18n::t("workspace-env-changes"),
                        Some(
                            h_flex()
                                .gap_1()
                                .text_xs()
                                .child(
                                    gpui::div()
                                        .text_color(theme.success)
                                        .child(if project.is_some() { "+0" } else { "--" }),
                                )
                                .child(
                                    gpui::div()
                                        .text_color(theme.danger)
                                        .child(if project.is_some() { "-0" } else { "" }),
                                )
                                .into_any_element(),
                        ),
                        theme,
                    ))
                    .child(env_row(
                        IconName::HardDrive,
                        i18n::t_str("workspace-env-local", &[("name", local_label.as_str())]),
                        None,
                        theme,
                    ))
                    .child(env_row(IconName::Github, branch_label.into(), None, theme))
                    .child(env_row(
                        IconName::Cpu,
                        i18n::t_str("workspace-env-model", &[("name", model_label.as_str())]),
                        None,
                        theme,
                    ))
                    .child(token_section)
                    .children(token_model_rows)
                    .child(gpui::div().h(px(1.)).w_full().bg(theme.border))
                    .child(
                        v_flex()
                            .gap_2()
                            .child(
                                gpui::div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(i18n::t("workspace-env-modes")),
                            )
                            .child(h_flex().gap_1().flex_wrap().child(mode_tag(
                                match approval_mode {
                                    ApprovalMode::OnRequest => i18n::t("workspace-env-yolo-off"),
                                    ApprovalMode::AutoReview => {
                                        i18n::t("workspace-env-auto-review")
                                    }
                                    ApprovalMode::Yolo => i18n::t("workspace-env-yolo-on"),
                                },
                                true,
                                theme,
                            ))),
                    )
                    .child(gpui::div().h(px(1.)).w_full().bg(theme.border))
                    .child(
                        v_flex()
                            .gap_2()
                            .child(
                                gpui::div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(i18n::t("workspace-env-sources")),
                            )
                            .child(
                                gpui::div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(i18n::t("workspace-env-no-sources")),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Composer: an auto-growing text area above a single toolbar row.
    /// Rendered bare — no card border, fill, or rounding — so it shares the
    /// page background with the message list and reads as the same layer.
    /// The `Input` has no appearance of its own; the only visual separator
    /// from the messages above is the hairline injected by the footer caller.
    fn render_composer(
        &mut self,
        running: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let plus = self.render_plus_button(cx);
        let project_chip = self.render_project_chip(theme, cx);
        let worktree_chip = self.render_worktree_chip(theme, cx);
        let plan_chip = self.render_plan_chip(theme, cx);
        let goal_chip = self.render_goal_chip(theme, cx);
        let team_chip = self.render_team_chip(theme, cx);
        let access = self.render_access_placeholder(theme, cx);
        let effort = self.render_reasoning_effort_selector(theme, cx);
        let model = self.render_model_selector(theme, cx);
        let send = self.render_send_button(running, cx);

        v_flex()
            .w_full()
            .gap_2()
            .child(self.input_state.clone())
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .justify_between()
                    .child(
                        // `min_w_0` lets this group flex-shrink when the row is
                        // narrow; `overflow_hidden` is deliberately NOT set so
                        // the chips' popovers (project picker, approval menu, `+`
                        // menu) can overflow upward. `MIN_WINDOW_W` keeps the row
                        // wide enough that the chips themselves never overflow.
                        h_flex()
                            .items_center()
                            .gap_1()
                            .min_w_0()
                            .child(plus)
                            .child(project_chip)
                            .when_some(worktree_chip, |el, chip| el.child(chip))
                            .when_some(plan_chip, |el, chip| el.child(chip))
                            .when_some(goal_chip, |el, chip| el.child(chip))
                            .when_some(team_chip, |el, chip| el.child(chip))
                            .child(access),
                    )
                    // Effort lives next to the model selector — both describe
                    // how the model reasons, so they read as one group.
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .flex_shrink_0()
                            .child(effort)
                            .child(model)
                            .child(send),
                    ),
            )
            .into_any_element()
    }

    /// Access chip + 3-tier approval popover.
    ///
    /// The chip is a mode-aware pill rendered next to the composer send button.
    /// Each `ApprovalMode` gets its own icon + accent color (green thumbs-up for
    /// `OnRequest`, blue bot for `AutoReview`, red triangle for `Yolo`) so the
    /// current permission posture is legible at a glance — a 1-line summary of
    /// what the model is allowed to do without prompting.
    ///
    /// Clicking the chip opens a `PopupMenu` mirroring the header:
    /// a question row with a "Learn more" link, three selectable rows (icon +
    /// title + subtitle, check on the right), a hairline, and a 4th non-clickable
    /// row pointing at `config.toml` for users who want a fully custom policy.
    /// The popover is `max_w(360)` to fit the longest bilingual subtitle
    /// ("Unrestricted access to the internet and any file on your computer")
    /// without wrapping.
    /// Worktree status chip — shown only while the thread is inside a git
    /// worktree. Displays the branch name; clicking exits the worktree with
    /// `action=keep` (cwd restored, worktree + branch left on disk for
    /// re-entry). For removal, the model calls `exit_worktree` with
    /// `action=remove` directly.
    fn render_worktree_chip(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let branch = self.thread.read(cx).worktree().map(|w| w.branch.clone())?;
        let label: SharedString = branch.into();
        let theme_bg = theme.secondary;
        let theme_border = theme.border;
        let theme_fg = theme.foreground;
        let theme_muted = theme.muted_foreground;

        Some(
            h_flex()
                .id("worktree-chip")
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .bg(theme_bg)
                .border_1()
                .border_color(theme_border)
                .cursor_pointer()
                .child(Icon::new(IconName::Github).xsmall().text_color(theme_muted))
                .child(gpui::div().text_xs().text_color(theme_fg).child(label))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.thread.update(cx, |t, cx| {
                        let _ = t.exit_worktree(cx);
                    });
                }))
                .into_any_element(),
        )
    }

    /// Plan-mode chip — shown only while the thread is in plan mode. A
    /// highlighted accent pill next to the access chip so the read-only
    /// research posture is legible at a glance. Clicking exits plan mode
    /// (mirrors the `+` menu toggle).
    fn render_plan_chip(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.thread.read(cx).plan_mode() {
            return None;
        }
        let accent = theme.accent;
        let label: SharedString = i18n::t("workspace-chip-plan-mode");
        Some(
            h_flex()
                .id("plan-chip")
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .bg(theme.secondary)
                .border_1()
                .border_color(accent)
                .cursor_pointer()
                .child(
                    Icon::new(IconName::LayoutDashboard)
                        .xsmall()
                        .text_color(accent),
                )
                .child(gpui::div().text_xs().text_color(accent).child(label))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.thread.update(cx, |t, cx| t.set_plan_mode(false, cx));
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    /// Goal-mode chip — shown only while the thread has an active goal. Renders
    /// `◎ Goal active · {elapsed}` in accent colors so the autonomous-loop
    /// posture is legible at a glance. Clicking toggles the status popover
    /// (condition / elapsed / evaluations / last reason / Clear).
    fn render_goal_chip(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let g = self.thread.read(cx).goal()?;
        let accent = theme.accent;
        let muted = theme.muted_foreground;
        let fg = theme.foreground;
        let elapsed = format_elapsed(g.started_at.elapsed());
        let label: SharedString =
            format!("◎ {} · {}", i18n::t("workspace-chip-goal-active"), elapsed).into();
        let open = self.goal_popover_open;

        let trigger = h_flex()
            .id("goal-chip")
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .bg(theme.secondary)
            .border_1()
            .border_color(accent)
            .cursor_pointer()
            .child(gpui::div().text_xs().text_color(accent).child(label))
            .child(
                Icon::new(if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(muted),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.goal_popover_open = !this.goal_popover_open;
                cx.notify();
            }));

        if !open {
            return Some(trigger.into_any_element());
        }

        // Status popover: condition / elapsed / evaluations / last reason /
        // Clear. Mirrors the access chip's `popover_style` dropdown pattern.
        let condition = g.condition.clone();
        let evaluations = g.evaluations;
        let last_reason = g.last_reason.clone();
        let condition_label = i18n::t("goal-popover-condition");
        let elapsed_label = i18n::t("goal-popover-elapsed");
        let evals_label = i18n::t("goal-popover-evaluations");
        let reason_label = i18n::t("goal-popover-last-reason");
        let clear_label = i18n::t("goal-popover-clear");
        let title_label = i18n::t("goal-popover-title");
        let popover = v_flex()
            .w_full()
            .gap_1()
            .p_3()
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(accent)
                    .child(format!("◎ {title_label}")),
            )
            .child(goal_popover_row(&condition_label, &condition, fg, muted))
            .child(goal_popover_row(&elapsed_label, &elapsed, fg, muted))
            .child(goal_popover_row(
                &evals_label,
                &evaluations.to_string(),
                fg,
                muted,
            ))
            .child(goal_popover_row(
                &reason_label,
                if last_reason.is_empty() {
                    "—"
                } else {
                    &last_reason
                },
                fg,
                muted,
            ))
            .child(
                h_flex().justify_end().child(
                    Button::new("goal-clear")
                        .small()
                        .label(clear_label)
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.thread.update(cx, |t, cx| t.clear_goal(cx));
                            this.goal_popover_open = false;
                            cx.notify();
                        })),
                ),
            );

        Some(
            gpui::div()
                .relative()
                .child(trigger)
                .child(
                    gpui::div()
                        .id("goal-dropdown")
                        .absolute()
                        .bottom_full()
                        .left_0()
                        .occlude()
                        .w(gpui::px(360.))
                        .popover_style(cx)
                        .child(popover)
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.goal_popover_open = false;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    /// Open the goal status popover (from the bare `/goal` command).
    pub fn open_goal_popover(&mut self, cx: &mut Context<Self>) {
        self.goal_popover_open = true;
        cx.notify();
    }

    /// Team roster chip — shown only while the leader has formed a team. Renders
    /// `👥 team · N` in accent colors; clicking opens a thin drawer listing
    /// each worker member (name / role / status dot / task count). Clicking a
    /// row opens that member's observation tab in the right pane. The leader is
    /// not listed (it is the main conversation).
    fn render_team_chip(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let team = self.thread.read(cx).team().cloned()?;
        // Collect member roster metadata in one pass so we never hold a borrow
        // on the `Team` entity across the render closures below. `Member` is not
        // `Clone`, so we lift only the cheap, 'static fields the rows need.
        let (count, rows): (usize, Vec<(String, String, bool, usize)>) = {
            let t = team.read(cx);
            let members = t.members();
            let tasks = t.tasks().read(cx).tasks();
            let rows = members
                .iter()
                .map(|(name, m)| {
                    let running = m.thread().read(cx).is_running();
                    let owned = tasks
                        .iter()
                        .filter(|tk| tk.owner.as_deref() == Some(name.as_str()))
                        .count();
                    (name.clone(), m.role().to_string(), running, owned)
                })
                .collect();
            (members.len(), rows)
        };
        let accent = theme.accent;
        let muted = theme.muted_foreground;
        let fg = theme.foreground;
        let open = self.team_chip_open;
        let label: SharedString = i18n::t_str("team-chip", &[("count", &count.to_string())]);

        let trigger = h_flex()
            .id("team-chip")
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .bg(theme.secondary)
            .border_1()
            .border_color(accent)
            .cursor_pointer()
            .child(Icon::new(IconName::User).xsmall().text_color(accent))
            .child(gpui::div().text_xs().text_color(accent).child(label))
            .child(
                Icon::new(if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(muted),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.team_chip_open = !this.team_chip_open;
                cx.notify();
            }));

        if !open {
            return Some(trigger.into_any_element());
        }

        let title = i18n::t("team-drawer-title");
        let empty = i18n::t("team-drawer-empty");

        // Build a row per worker. The on_click opens (or focuses) that member's
        // tab and closes the drawer. The row data is pre-collected above so the
        // render closures only capture cheap, 'static data plus the workspace
        // handle.
        let roster = if rows.is_empty() {
            v_flex()
                .w_full()
                .p_3()
                .child(gpui::div().text_xs().text_color(muted).child(empty))
        } else {
            v_flex().w_full().gap_1().p_2().children(
                rows.into_iter()
                    .enumerate()
                    .map(|(ix, (name, role, running, owned))| {
                        let dot_color = if running {
                            theme.accent
                        } else {
                            theme.muted_foreground
                        };
                        let tasks_label =
                            i18n::t_str("team-drawer-tasks", &[("count", &owned.to_string())]);
                        let name_for_click = name.clone();
                        h_flex()
                            .id(("team-member-row", ix))
                            .items_center()
                            .gap_2()
                            .px_2()
                            .py_1()
                            .rounded(theme.radius)
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.accent.opacity(0.08)))
                            .child(gpui::div().w(px(8.)).h(px(8.)).rounded_full().bg(dot_color))
                            .child(
                                gpui::div()
                                    .text_xs()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(fg)
                                    .child(name),
                            )
                            .child(gpui::div().text_xs().text_color(muted).child(role))
                            .child(gpui::div().flex_1())
                            .child(gpui::div().text_xs().text_color(muted).child(tasks_label))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                                this.open_member_tab(&name_for_click, cx);
                                this.team_chip_open = false;
                                cx.notify();
                            }))
                            .into_any_element()
                    })
                    .collect::<Vec<_>>(),
            )
        };

        let popover = v_flex()
            .w_full()
            .gap_1()
            .p_2()
            .child(
                gpui::div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(accent)
                    .child(title),
            )
            .child(roster);

        Some(
            gpui::div()
                .relative()
                .child(trigger)
                .child(
                    gpui::div()
                        .id("team-dropdown")
                        .absolute()
                        .bottom_full()
                        .left_0()
                        .occlude()
                        .w(px(320.))
                        .popover_style(cx)
                        .child(popover)
                        .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                            this.team_chip_open = false;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    fn render_access_placeholder(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mode = self.thread.read(cx).approval_mode();
        let open = self.access_open;
        // Pre-extract chip visuals so the click handler closure doesn't
        // capture `theme` (which only lives for the method body) — closures
        // passed to `cx.listener` must be `'static`.
        let (chip_label, chip_color, chip_icon) = mode_chip_visual(mode, theme);
        let workspace = cx.entity().downgrade();

        let trigger = h_flex()
            .id("access-chip")
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .min_w(px(96.))
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.accent.opacity(0.08)))
            .cursor_pointer()
            .child(Icon::new(chip_icon).xsmall().text_color(chip_color))
            .child(
                gpui::div()
                    .flex_1()
                    .text_xs()
                    .text_color(chip_color)
                    .child(chip_label),
            )
            .child(
                Icon::new(if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(theme.muted_foreground),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                if this.access_open {
                    this.close_access_menu();
                } else {
                    this.access_open = true;
                }
                cx.notify();
            }));

        if !open {
            return trigger.into_any_element();
        }

        // The popover is a plain `div` with `popover_style` (opaque card
        // chrome: bg + border + shadow + rounded). We don't route it through
        // `PopupMenu` because `PopupMenuItem::element` wraps every row in
        // `h_flex().flex_1().min_h(26)`, which both leaked vertical space
        // and — in the single-item case — clipped the v_flex content to
        // 26px. Doing it ourselves gives a content-sized, opaque popover.
        //
        // `w(360)` (not `max_w`) — with `min_w_0` on every text div, the
        // v_flex's intrinsic min-content is tiny (just icon widths + padding),
        // so `max_w` alone leaves the popover at ~140px and the subtitles
        // wrap into single-word lines. A fixed 360px width gives the
        // subtitles room to wrap at word boundaries.
        let content = build_approval_content(workspace.clone(), mode, cx);
        gpui::div()
            .relative()
            .child(trigger)
            .child(
                gpui::div()
                    .id("access-dropdown")
                    .absolute()
                    .bottom_full()
                    .left_0()
                    .occlude()
                    .w(gpui::px(360.))
                    .popover_style(cx)
                    .child(content)
                    .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                        this.close_access_menu();
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// The composer `+` button and its popup menu ("add / plugins").
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
        let ws = cx.entity().downgrade();
        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            let ws_files = ws.clone();
            let ws_plan = ws.clone();
            let ws_goal = ws.clone();
            build_plus_menu(
                menu,
                &theme,
                move |window, cx| {
                    let _ = ws_files.update(cx, |this, cx| {
                        this.close_plus_menu();
                        this.pick_files(window, cx);
                        cx.notify();
                    });
                },
                move |_window, cx| {
                    let _ = ws_plan.update(cx, |this, cx| {
                        this.close_plus_menu();
                        let on = this.thread.read(cx).plan_mode();
                        this.thread.update(cx, |t, cx| t.set_plan_mode(!on, cx));
                        cx.notify();
                    });
                },
                move |window, cx| {
                    let _ = ws_goal.update(cx, |this, cx| {
                        this.close_plus_menu();
                        // Insert `/goal ` so the user types the completion
                        // condition and submits — same pattern as the `⁄` menu
                        // inserting `/name ` for a slash command.
                        this.input_state.update(cx, |state, cx| {
                            state.set_value("/goal ".to_string(), window, cx);
                        });
                        cx.notify();
                    });
                },
            )
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

    /// Project chip: a clickable control showing the current project basename
    /// (or "Choose project" when unbound). Opens a dropdown listing recent
    /// projects followed by "Create blank project" / "Select folder" actions.
    /// Mirrors the access-chip pattern.
    fn render_project_chip(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let project = self.thread.read(cx).project().cloned();
        let open = self.project_chip_open;
        let workspace = cx.entity().downgrade();

        let (icon, label): (Option<IconName>, SharedString) = match &project {
            Some(dir) => {
                let name = dir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("project")
                    .to_string();
                (Some(IconName::FolderOpen), name.into())
            }
            None => (
                Some(IconName::FolderOpen),
                i18n::t("workspace-project-choose"),
            ),
        };

        let trigger = h_flex()
            .id("project-chip")
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.accent.opacity(0.08)))
            .cursor_pointer()
            .when_some(icon.clone(), |el, ic| {
                el.child(Icon::new(ic).xsmall().text_color(theme.muted_foreground))
            })
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.foreground)
                    .child(label),
            )
            .child(
                Icon::new(if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(theme.muted_foreground),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                if this.project_chip_open {
                    this.close_project_chip_menu();
                    cx.notify();
                    return;
                }
                // Only allow project selection on empty threads.
                let can_set = this.thread.read(cx).messages().is_empty();
                if !can_set {
                    return;
                }
                this.project_chip_open = true;

                // Fetch recent projects from the store.
                let store = agent::thread_store_global();
                let recent = store.update(cx, |s, cx| s.fetch_recent_projects(20, cx));

                let ws = workspace.clone();
                let theme = cx.theme().clone();

                // Build menu synchronously with whatever recent list we have
                // cached; the async fetch will refresh on the next open.
                let ws_blank = ws.clone();
                let ws_folder = ws.clone();

                let menu = PopupMenu::build(window, cx, move |menu, _window, cx| {
                    let mut menu = menu.max_w(gpui::px(320.)).scrollable(true);
                    menu = menu.label(i18n::t("sidebar-section-projects"));

                    let ws_recent = ws.clone();
                    let theme_recent = theme.clone();

                    // Fetch synchronously from the store's cached summaries.
                    let store = agent::thread_store_global();
                    let summaries = store.read(cx).summaries();
                    let mut seen = std::collections::HashSet::new();
                    let mut recent_projects: Vec<String> = Vec::new();
                    for s in summaries {
                        if !s.project.is_empty() && seen.insert(s.project.clone()) {
                            recent_projects.push(s.project.clone());
                        }
                        if recent_projects.len() >= 20 {
                            break;
                        }
                    }

                    for path_str in &recent_projects {
                        let path = std::path::PathBuf::from(path_str);
                        let name = path
                            .file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or(path_str)
                            .to_string();
                        let display_path = path_str.clone();
                        let click_path = path_str.clone();
                        let ws_sel = ws_recent.clone();
                        let themed = theme_recent.clone();
                        menu = menu.item(
                            PopupMenuItem::element(move |_window, _cx| {
                                h_flex()
                                    .items_center()
                                    .gap_2()
                                    .child(
                                        Icon::new(IconName::FolderOpen)
                                            .xsmall()
                                            .text_color(themed.muted_foreground),
                                    )
                                    .child(
                                        gpui::div()
                                            .text_sm()
                                            .text_color(themed.foreground)
                                            .child(name.clone()),
                                    )
                                    .child(
                                        gpui::div()
                                            .flex_1()
                                            .text_xs()
                                            .text_color(themed.muted_foreground)
                                            .child(display_path.clone()),
                                    )
                            })
                            .on_click(
                                move |_, _, cx: &mut gpui::App| {
                                    let p = std::path::PathBuf::from(&click_path);
                                    let _ = ws_sel.update(cx, |this, cx| {
                                        this.close_project_chip_menu();
                                        this.thread.update(cx, |t, cx| t.set_project(p, cx));
                                        cx.notify();
                                    });
                                },
                            ),
                        );
                    }

                    menu = menu.separator();

                    // "New project" actions as top-level items.
                    // PopupMenu submenus are clipped by overflow_y_scroll, so they
                    // cannot coexist with scrollable(true) — which the recent-projects
                    // list above needs. Flatten instead of nesting under a submenu.
                    menu = menu.label(i18n::t("workspace-project-new"));

                    let themed_blank = theme.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_window, _cx| {
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::Plus)
                                        .xsmall()
                                        .text_color(themed_blank.muted_foreground),
                                )
                                .child(
                                    gpui::div()
                                        .text_sm()
                                        .text_color(themed_blank.foreground)
                                        .child(i18n::t("workspace-project-blank")),
                                )
                        })
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            let _ = ws_blank.update(cx, |this, cx| {
                                this.close_project_chip_menu();
                                this.open_blank_project(cx);
                            });
                        }),
                    );

                    let themed_folder = theme.clone();
                    menu = menu.item(
                        PopupMenuItem::element(move |_window, _cx| {
                            h_flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    Icon::new(IconName::FolderOpen)
                                        .xsmall()
                                        .text_color(themed_folder.muted_foreground),
                                )
                                .child(
                                    gpui::div()
                                        .text_sm()
                                        .text_color(themed_folder.foreground)
                                        .child(i18n::t("workspace-project-select-folder")),
                                )
                        })
                        .on_click(move |_, _, cx: &mut gpui::App| {
                            let _ = ws_folder.update(cx, |this, cx| {
                                this.close_project_chip_menu();
                                this.choose_project_inner(cx);
                            });
                        }),
                    );

                    // Suppress unused-variable warning for the async fetch.
                    drop(recent);

                    menu
                });
                let sub = cx.subscribe(
                    &menu,
                    |this: &mut Workspace,
                     _menu: Entity<PopupMenu>,
                     _: &DismissEvent,
                     cx: &mut Context<Workspace>| {
                        this.close_project_chip_menu();
                        cx.notify();
                    },
                );
                this.project_chip_menu = Some(menu);
                this.project_chip_menu_sub = Some(sub);
                cx.notify();
            }));

        if !open {
            return trigger.into_any_element();
        }

        debug_assert!(self.project_chip_open);
        debug_assert!(self.project_chip_menu.is_some());
        let menu = self
            .project_chip_menu
            .clone()
            .expect("project_chip_menu exists when open");
        gpui::div()
            .relative()
            .child(trigger)
            .child(
                gpui::div()
                    .id("project-chip-dropdown")
                    .absolute()
                    .bottom_full()
                    .left_0()
                    .occlude()
                    .child(menu),
            )
            .into_any_element()
    }

    /// Open the blank-project flow: pick a parent directory, then prompt for name.
    fn open_blank_project(&mut self, cx: &mut Context<Self>) {
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
                    && let Some(parent) = paths.into_iter().next()
                {
                    this.blank_project_parent = Some(parent);
                    this.blank_project_name_input = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Lazily create the blank-project name input (needs a Window).
    fn ensure_blank_project_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.blank_project_parent.is_none() {
            return;
        }
        if self.blank_project_name_input.is_some() {
            return;
        }
        self.blank_project_name_input = Some(cx.new(|cx| InputState::new(window, cx)));
    }

    /// Submit the blank project: create the directory and bind it.
    fn confirm_blank_project(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(parent) = self.blank_project_parent.take() else {
            return;
        };
        let name = self
            .blank_project_name_input
            .as_ref()
            .map(|s| s.read(cx).value().trim().to_string())
            .unwrap_or_default();
        if name.is_empty() {
            self.blank_project_parent = Some(parent);
            return;
        }
        let new_path = parent.join(&name);
        if let Err(e) = std::fs::create_dir_all(&new_path) {
            tracing::warn!(error = %e, "failed to create project directory");
            cx.notify();
            return;
        }
        self.thread.update(cx, |t, cx| t.set_project(new_path, cx));
        self.blank_project_name_input = None;
        cx.notify();
    }

    /// Cancel the blank project overlay.
    fn cancel_blank_project(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.blank_project_parent = None;
        self.blank_project_name_input = None;
        cx.notify();
    }

    /// Shared inner logic for "Select folder" (directory picker → bind project).
    fn choose_project_inner(&mut self, cx: &mut Context<Self>) {
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

    /// Overlay prompting for the blank-project folder name.
    fn render_blank_project_overlay(
        &self,
        _window: &mut Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.pending_ask.is_some()
            || !self.pending_auths.is_empty()
            || self.pending_plan.is_some()
        {
            return None;
        }
        self.blank_project_parent.as_ref()?;
        let input = self.blank_project_name_input.as_ref()?;
        let parent_name = self
            .blank_project_parent
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("…")
            .to_string();

        Some(
            gpui::div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                // Scrim must use the dark foreground, not `background`. A white
                // veil over a white conversation does not dim, so the page shows
                // through and the modal reads as transparent.
                .bg(theme.foreground.opacity(0.6))
                .child(
                    v_flex()
                        .w(px(480.))
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
                                .child(
                                    Icon::new(IconName::FolderOpen)
                                        .small()
                                        .text_color(theme.accent),
                                )
                                .child(
                                    gpui::div()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child(i18n::t("workspace-project-blank")),
                                ),
                        )
                        .child(
                            gpui::div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(format!(
                                    "{}: {}",
                                    i18n::t("workspace-project-name-prompt"),
                                    parent_name
                                )),
                        )
                        .child(Input::new(input))
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("blank-project-cancel")
                                        .ghost()
                                        .small()
                                        .label(i18n::t("workspace-cancel"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.cancel_blank_project(window, cx);
                                        })),
                                )
                                .child(
                                    Button::new("blank-project-confirm")
                                        .primary()
                                        .small()
                                        .label(i18n::t("workspace-rename-confirm"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.confirm_blank_project(window, cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Settings overlay replaces the entire window content; the underlying
        // Workspace state (sidebar, conversation, composer) is preserved and
        // returns unchanged when the user clicks "Back to app".
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
        if matches!(self.view_mode, ViewMode::Plugins) {
            self.ensure_plugins(window, cx);
            let plugins = self
                .plugin_manager_view
                .as_ref()
                .expect("view_mode == Plugins implies plugin manager view is set")
                .clone();
            return h_flex().size_full().child(plugins);
        }
        // Terminal pane: sidebar (for tab switching) + a full-bleed terminal
        // view filling the main column. The terminal view owns its PTY and
        // grid; this branch only mounts it. Resize/scrollback/selection are
        // handled inside `TerminalView` / `TerminalElement`.
        if matches!(self.view_mode, ViewMode::Terminal) {
            let theme = cx.theme().clone();
            let title_text = self
                .thread
                .read(cx)
                .project()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("manox")
                .to_string();
            let terminal = self
                .terminal_view
                .clone()
                .expect("view_mode == Terminal implies terminal_view is set");
            return h_flex()
                .size_full()
                .bg(theme.background)
                .text_color(theme.foreground)
                .on_action(cx.listener(|this, _: &FocusConversation, _window, cx| {
                    this.focus_conversation(cx);
                }))
                .on_action(cx.listener(|this, _: &FocusTerminal, _window, cx| {
                    this.focus_terminal(cx);
                }))
                .on_action(cx.listener(|this, _: &NewTerminalTab, _window, cx| {
                    this.open_terminal_tab(cx);
                }))
                .on_action(cx.listener(|this, _: &CloseTerminalTab, _window, cx| {
                    this.close_terminal_tab(cx);
                }))
                .child(self.sidebar.clone())
                .child(
                    v_flex()
                        .flex_1()
                        .h_full()
                        .relative()
                        .child(
                            TitleBar::new().child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .flex_1()
                                    .min_w_0()
                                    .child(Icon::new(IconName::SquareTerminal).small())
                                    .child(
                                        gpui::div()
                                            .text_sm()
                                            .text_left()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .child(title_text),
                                    ),
                            ),
                        )
                        .child(v_flex().flex_1().h_full().w_full().child(terminal)),
                );
        }
        let theme = cx.theme().clone();
        let running = self.thread.read(cx).is_running();

        self.ensure_ask_inputs(window, cx);
        self.ensure_blank_project_input(window, cx);

        let overlay = self
            .render_auth_overlay(&theme, cx)
            .or_else(|| self.render_plan_approval_overlay(&theme, cx))
            .or_else(|| self.render_blank_project_overlay(window, &theme, cx));

        let editor_open = self.editor_open;
        let right_pane_open = !self.right_tabs.is_empty();
        let editor_preview = self.editor_preview;
        let editor_width = self.editor_width;
        // Title text is the active thread's display title (user rename > LLM
        // title > mechanical summary). Falls back to "manox" so an unselected
        // first screen stays branded before any title is generated.
        let title_text: SharedString = {
            let s = self.thread.read(cx).display_title();
            if s.is_empty() { "manox".to_string() } else { s }
        }
        .into();
        // Empty first screen: no messages and nothing streaming. The composer is
        // hoisted into a vertically-centered hero (heading + composer + "Choose
        // project"); once the conversation starts it drops to the bottom footer.
        let first_screen = self.conversation.read(cx).is_empty(cx) && !running;
        // The inline composer and the ask drawer are mutually exclusive: while
        // an ask card is open the composer is replaced by the drawer; while the
        // editor pane is open both are hidden.
        let footer = if editor_open || first_screen {
            None
        } else if self.pending_ask.is_some() {
            Some(
                v_flex()
                    .w_full()
                    .flex_shrink_0()
                    .bg(theme.background)
                    .py_2()
                    .relative()
                    .child(centered(self.render_ask_drawer(&theme, cx))),
            )
        } else {
            Some(
                v_flex()
                    .w_full()
                    .flex_shrink_0()
                    .bg(theme.background)
                    .py_2()
                    .gap_2()
                    .relative()
                    .children(self.render_slash_overlay())
                    .child(centered(gpui::div().w_full().h(px(1.)).bg(theme.border)))
                    .children(self.render_attachments(&theme, cx))
                    .child(centered(self.render_composer(running, &theme, cx))),
            )
        };
        // Hero occupies the message-list region on the first screen.
        // Notice items on the first screen (e.g. YOLO toggle acknowledgement).
        // They are stored in the conversation but hidden behind the hero layout;
        // show them as a temporary banner below the composer so the user sees
        // the feedback without leaving the first-screen view.
        let hero_notices = if first_screen {
            self.conversation
                .read(cx)
                .items()
                .iter()
                .rev()
                .filter_map(|e| {
                    if let ConvItem::Error(msg) | ConvItem::Notice(msg) = e.read(cx).kind() {
                        Some(msg.clone())
                    } else {
                        None
                    }
                })
                .next()
        } else {
            None
        };
        let hero = if editor_open || !first_screen {
            None
        } else if self.pending_ask.is_some() {
            // On the first screen with an ask card open, show the drawer in
            // the hero position instead of the composer.
            Some(
                v_flex()
                    .flex_1()
                    .w_full()
                    .justify_center()
                    .items_center()
                    .relative()
                    .child(centered(self.render_ask_drawer(&theme, cx))),
            )
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
                                        .child(i18n::t("workspace-empty-prompt")),
                                )
                                .children(self.render_attachments(&theme, cx))
                                .child(self.render_composer(running, &theme, cx))
                                .children(hero_notices.map(|msg| {
                                    gpui::div()
                                        .w_full()
                                        .px_3()
                                        .py_1p5()
                                        .rounded(theme.radius)
                                        .bg(theme.accent.opacity(0.1))
                                        .border_1()
                                        .border_color(theme.accent.opacity(0.2))
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(msg)
                                })),
                        )
                        .relative()
                        .children(self.render_slash_overlay()),
                    ),
            )
        };
        // Outline rail sits left of the message list (right of the sidebar
        // divider), so it only shows alongside a live conversation.
        let outline = (!first_screen && !editor_open)
            .then(|| self.render_outline(&theme, cx))
            .flatten();
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
        // Sidebar divider: same shape as the editor divider but lives on the
        // right edge of the sidebar. Double-click resets to the default
        // `SIDEBAR_WIDTH` for symmetry with the editor pane.
        let sidebar_divider = gpui::div()
            .id("sidebar-divider")
            .w(px(SIDEBAR_DIVIDER_WIDTH))
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
            .on_drag(DraggedSidebarDivider, |_, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| DraggedSidebarDivider)
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, e: &MouseUpEvent, _, cx| {
                    if e.click_count >= 2 {
                        let reset = px(SIDEBAR_WIDTH);
                        this.sidebar_width = reset;
                        this.sidebar.update(cx, |s, cx| s.set_width(reset, cx));
                        cx.notify();
                    }
                }),
            );
        // Right pane is a tab container: one Editor slot (the markdown
        // scratchpad) plus one slot per team member (a MemberPanel). The
        // top-level TabBar is built from `right_tabs`; the content below
        // dispatches on the active tab.
        let active_tab = self.right_tabs.get(self.active_right_tab).cloned();
        let right_tab_children: Vec<Tab> = self
            .right_tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let base = match tab {
                    RightTab::Editor => Tab::new().label(i18n::t("member-editor-tab")),
                    RightTab::Member(name) => {
                        Tab::new().label(i18n::t_str("member-tab", &[("name", name)]))
                    }
                };
                // Only member tabs carry a close affordance; the Editor tab
                // keeps its keyboard toggle (`ToggleEditor` / `CloseEditor`).
                match tab {
                    RightTab::Member(_) => base.suffix(
                        gpui::div()
                            .id(("right-tab-close", ix))
                            .cursor_pointer()
                            .child(
                                Icon::new(IconName::Close)
                                    .xsmall()
                                    .text_color(theme.muted_foreground),
                            )
                            // Stop the click from also selecting the tab
                            // underneath the ×.
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.close_right_tab(ix, window, cx);
                            })),
                    ),
                    RightTab::Editor => base,
                }
            })
            .collect();
        let editor_pane = v_flex()
            .w(editor_width)
            .h_full()
            .flex_shrink_0()
            .bg(theme.background)
            .child(
                h_flex().w_full().px_2().pt_1().child(
                    TabBar::new("right-tabs")
                        .underline()
                        .small()
                        .selected_index(self.active_right_tab)
                        .on_click(cx.listener(|this, ix: &usize, _window, cx| {
                            this.set_active_right_tab(*ix, cx);
                        }))
                        .children(right_tab_children),
                ),
            )
            .child(
                gpui::div()
                    .id("right-pane-content")
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(match active_tab {
                        Some(RightTab::Editor) => v_flex()
                            .h_full()
                            .child(
                                h_flex().w_full().px_2().child(
                                    TabBar::new("editor-write-preview")
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
                                    .w_full()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_hidden()
                                    .child(if editor_preview {
                                        // Markdown re-parses mdast and re-lays-out every
                                        // frame, so a pane resize re-wraps at the new width
                                        // with no cached tree to go stale; a stable id keeps
                                        // the scroll position across the resize.
                                        gpui::div()
                                            .h_full()
                                            .p_4()
                                            .text_sm()
                                            .child(
                                                Markdown::new(
                                                    "editor-preview",
                                                    self.editor_state.read(cx).value().to_string(),
                                                )
                                                .theme(cx.theme())
                                                .selectable(true)
                                                .scrollable(true),
                                            )
                                            .into_any_element()
                                    } else {
                                        Input::new(&self.editor_state)
                                            .size_full()
                                            .appearance(false)
                                            .into_any_element()
                                    }),
                            )
                            .into_any_element(),
                        Some(RightTab::Member(name)) => self
                            .member_panels
                            .get(&name)
                            .map(|p| p.clone().into_any_element())
                            .unwrap_or_else(|| gpui::div().into_any_element()),
                        None => gpui::div().into_any_element(),
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
            .on_action(cx.listener(|this, _: &FocusTerminal, _window, cx| {
                this.focus_terminal(cx);
            }))
            .on_action(cx.listener(|this, _: &FocusConversation, _window, cx| {
                this.focus_conversation(cx);
            }))
            .on_action(cx.listener(|this, _: &NewTerminalTab, _window, cx| {
                this.open_terminal_tab(cx);
            }))
            .on_action(cx.listener(|this, _: &CloseTerminalTab, _window, cx| {
                this.close_terminal_tab(cx);
            }))
            // Left sidebar with a draggable divider on its right edge.
            .child(self.sidebar.clone())
            .child(sidebar_divider)
            // Main column
            .child({
                // The env card floats across the top-right of the conversation
                // area and reserves `ENV_CONTENT_INSET` of right gutter on the
                // message list when shown. It is default-off and mounts only
                // once the main-column body is at least `ENV_CARD_MIN_MAIN_W`
                // wide — below that the card stays hidden and the messages +
                // composer use the full width. The body width is derived from
                // the live window size (sidebar + its divider are the only
                // siblings when the editor pane is closed, which the
                // `!editor_open` term already guarantees).
                let main_body_w =
                    window.bounds().size.width - self.sidebar_width - px(SIDEBAR_DIVIDER_WIDTH);
                let show_env = !editor_open
                    && !first_screen
                    && self.thread.read(cx).has_interacted()
                    && main_body_w >= px(ENV_CARD_MIN_MAIN_W);
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .relative()
                    // Body wrapper: hero / list / footer / overlay share a common
                    // horizontal inset so conversation content doesn't kiss the
                    // panel edge. `pt` reserves space for the title-bar overlay
                    // (last child below); the overlay paints after the body so
                    // the "..." menu isn't covered by the conversation list.
                    //
                    // The env-card gutter is NOT applied here. The card floats
                    // only across the top of the conversation area and never
                    // reaches the composer at the bottom, so reserving its width
                    // on the whole body would needlessly starve the composer.
                    // The gutter is applied to the message-list region only,
                    // where the card actually overlaps.
                    .child(
                        v_flex()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .w_full()
                            .overflow_hidden()
                            .pt(TITLE_BAR_HEIGHT)
                            .pb_2()
                            // Empty first screen shows the centered hero in place of
                            // the (empty) message list; otherwise a virtualized, tail-
                            // following conversation column. Each item is its own
                            // `Entity<MessageItem>`, so a streaming delta only marks
                            // that item's entity dirty; the `list` reconciles its
                            // per-item height cache via `remeasure_items` so the
                            // synchronous renderer's deterministic heights never
                            // desync.
                            .children(hero)
                            .children((!first_screen).then(|| {
                                let conv = self.conversation.clone();
                                // Virtualized variable-height message list. The gpui
                                // `list` only lays out — and only syntax-highlights, via
                                // the synchronous renderer — the items in the viewport
                                // plus `MSG_LIST_OVERDRAW`, so a long thread's first frame
                                // pays only for the visible turn, not every code block.
                                // `FollowMode::Tail` on the ListState replaces the
                                // hand-rolled stick-to-bottom: it re-pins to the end each
                                // layout while following, disengages on upward scroll, and
                                // re-arms at the bottom. Item heights are deterministic
                                // per-frame (sync markdown), so the per-item height cache
                                // the list maintains never falls out of sync with async
                                // parsing — the root cause of the old overlap bug. Count
                                // and height changes are reconciled from the ThreadEvent
                                // handler and the `push_*` sites via `splice`/
                                // `remeasure_items`.
                                let list_state = self.list_state.clone();
                                let list_el = gpui::list(list_state, move |ix, _window, cx| {
                                    let item = conv.read(cx).items().get(ix).cloned();
                                    match item {
                                        // `flex_shrink_0` pins each row to its true content
                                        // height so the list's per-item height cache stays
                                        // honest and markdown never paints outside its row.
                                        Some(item) => v_flex()
                                            .w_full()
                                            .pt_1()
                                            .pb_4()
                                            .flex_shrink_0()
                                            .min_w_0()
                                            .child(item)
                                            .into_any_element(),
                                        // Index out of range mid-splice (count changed
                                        // between a layout pass and the render closure):
                                        // render an empty row rather than panic.
                                        None => gpui::div().into_any_element(),
                                    }
                                })
                                .w_full()
                                .h_full()
                                .min_h_0()
                                .min_w_0();
                                // Outer column fills the list region. `h_full()` is load-
                                // bearing: the region is an `h_flex()` row (items_center,
                                // not stretch), so without it the wrapper shrinks to
                                // content height and the list has no room to fill. The
                                // list fills the region; short conversations top-align
                                // (standard virtualized-list behavior) instead of the old
                                // bottom-justify — the trade-off for not measuring every
                                // item up front.
                                let list_wrap = v_flex()
                                    .flex_1()
                                    .h_full()
                                    .min_h_0()
                                    .min_w_0()
                                    .child(list_el);
                                // Outline rail (left) + flat message column (right)
                                // share the list region's height. The env-card
                                // gutter is reserved here (not on the whole body)
                                // so the top messages don't hide behind the card
                                // while the composer below keeps the full width.
                                h_flex()
                                    .flex_1()
                                    .w_full()
                                    .min_h_0()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .when(show_env, |this| this.pr(px(ENV_CONTENT_INSET)))
                                    .children(outline)
                                    .child(list_wrap)
                            }))
                            .children(footer)
                            // Approval overlay (if any)
                            .children(overlay),
                    )
                    // Title-bar overlay: absolute top of the main column,
                    // painted after the body so the "..." menu isn't covered
                    // by the conversation list.
                    .child(
                        gpui::div()
                            .absolute()
                            .top(px(0.))
                            .left(px(0.))
                            .right(px(0.))
                            .h(TITLE_BAR_HEIGHT)
                            .child(
                                TitleBar::new()
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .items_center()
                                            .flex_1()
                                            .min_w_0()
                                            .child(Icon::new(IconName::Bot).small())
                                            .child(
                                                gpui::div()
                                                    .text_sm()
                                                    .text_left()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .child(title_text),
                                            )
                                            .child(self.render_title_menu_trigger(&theme, cx)),
                                    )
                                    .child(h_flex()),
                            ),
                    )
                    // Environment info card: floats absolute, top-right of the
                    // conversation area. Hidden on the empty first screen, while
                    // the editor pane is open, and until the thread has interacted.
                    .children(show_env.then(|| self.render_environment_panel(&theme, cx)))
            })
            .when(right_pane_open, |this| {
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
                    // or collapses the conversation column. `sidebar_width`
                    // is read live so a wide sidebar correctly shrinks the
                    // available editor envelope.
                    let new_w = e.bounds.right() - e.event.position.x;
                    let dynamic_max = e.bounds.size.width
                        - this.sidebar_width
                        - px(EDITOR_DIVIDER_WIDTH)
                        - px(MAIN_MIN_WIDTH);
                    let max_w = dynamic_max
                        .min(px(EDITOR_MAX_WIDTH))
                        .max(px(EDITOR_MIN_WIDTH));
                    this.editor_width = new_w.clamp(px(EDITOR_MIN_WIDTH), max_w);
                    cx.notify();
                },
            ))
            .on_drag_move(cx.listener(
                |this, e: &DragMoveEvent<DraggedSidebarDivider>, _window, cx| {
                    // The root fills the window, so the sidebar's right edge
                    // is the cursor's x position relative to the root's left.
                    // Clamp so the main column (and the editor pane when
                    // open) always retain at least `MAIN_MIN_WIDTH`.
                    let new_w = e.event.position.x - e.bounds.left();
                    let editor_reserve = if this.right_pane_open() {
                        this.editor_width + px(EDITOR_DIVIDER_WIDTH)
                    } else {
                        px(0.)
                    };
                    let dynamic_max = e.bounds.size.width
                        - px(SIDEBAR_DIVIDER_WIDTH)
                        - editor_reserve
                        - px(MAIN_MIN_WIDTH);
                    let max_w = dynamic_max
                        .min(px(SIDEBAR_MAX_WIDTH))
                        .max(px(SIDEBAR_MIN_WIDTH));
                    let clamped = new_w.clamp(px(SIDEBAR_MIN_WIDTH), max_w);
                    this.sidebar_width = clamped;
                    this.sidebar.update(cx, |s, cx| s.set_width(clamped, cx));
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
        response_input: None,
    })
}

/// Map an `ApprovalMode` to the chip's (label, accent color, icon) triple.
///
/// Colors are theme tokens, not raw hsla values, so the chip follows the
/// active theme (light/dark) without bespoke palettes per mode. The
/// `OnRequest` accent uses `success` (green) as a "this is the safe default"
/// signal — staying gray would be visually identical to a disabled state.
fn mode_chip_visual(mode: ApprovalMode, theme: &Theme) -> (SharedString, gpui::Hsla, IconName) {
    match mode {
        ApprovalMode::OnRequest => (
            i18n::t("workspace-chip-mode-on-request"),
            theme.success,
            IconName::ThumbsUp,
        ),
        ApprovalMode::AutoReview => (
            i18n::t("workspace-chip-mode-auto-review"),
            theme.info,
            IconName::Bot,
        ),
        ApprovalMode::Yolo => (
            i18n::t("workspace-chip-mode-yolo"),
            theme.danger,
            IconName::TriangleAlert,
        ),
    }
}

/// Format a `Duration` as a compact elapsed-time string for the goal chip
/// (`42s`, `3m 42s`, `1h 5m`). Not localized — the format is universal.
fn format_elapsed(d: std::time::Duration) -> String {
    let total_secs = d.as_secs();
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    if h > 0 {
        format!("{h}h {m}m")
    } else if m > 0 {
        format!("{m}m {s}s")
    } else {
        format!("{s}s")
    }
}

/// One label/value row in the goal status popover.
fn goal_popover_row(label: &str, value: &str, fg: gpui::Hsla, muted: gpui::Hsla) -> gpui::Div {
    h_flex()
        .w_full()
        .items_start()
        .gap_2()
        .child(
            gpui::div()
                .min_w(px(96.))
                .text_xs()
                .text_color(muted)
                .child(label.to_string()),
        )
        .child(
            gpui::div()
                .min_w_0()
                .flex_1()
                .text_xs()
                .text_color(fg)
                .child(value.to_string()),
        )
}
/// Build the 3-tier approval `PopupMenu`:
///   - title row: localized question + a "Learn more" link on the right
///   - three selectable rows (icon + title + subtitle, check on the right)
///   - hairline separator
///   - non-clickable "Custom (config.toml)" info row
///
/// Width is `360px` to fit the longest bilingual subtitle. Each clickable row
/// routes through `Workspace::apply_approval_mode` so the mode switch +
/// notice + menu close stay in one place.
///
/// `theme` is consumed up front: every value used inside the `'static` row
/// closures is pre-extracted into owned `SharedString`/`Hsla`/`IconName`,
/// so the closures don't capture a short-lived theme reference.
/// Build the popover content for the access chip: a header row (question +
/// "Learn more" link) and three selectable mode rows (icon + title +
/// subtitle, check on the right for the active one). The whole thing is a
/// plain `v_flex` so it sizes to its content with no `flex_1` distribution
/// across items. The chip's dropdown wraps this in a `popover_style` div
/// for the opaque card chrome — that path doesn't go through `PopupMenu`
/// at all, sidestepping the per-`ElementItem` `flex_1`/`min_h(26)` wrapper
/// that was producing both the height-leak bug and the clip-to-26 bug.
fn build_approval_content(
    workspace: WeakEntity<Workspace>,
    current: ApprovalMode,
    cx: &mut gpui::App,
) -> gpui::Div {
    let fg: gpui::Hsla = cx.theme().foreground;
    let muted: gpui::Hsla = cx.theme().muted_foreground;
    let success: gpui::Hsla = cx.theme().success;
    let info: gpui::Hsla = cx.theme().info;
    let danger: gpui::Hsla = cx.theme().danger;

    let make_row = |mode: ApprovalMode,
                    title: SharedString,
                    subtitle: SharedString,
                    icon: IconName,
                    accent: gpui::Hsla,
                    selected: bool| {
        let ws = workspace.clone();
        h_flex()
            .id(("approval-mode-row", mode as usize))
            .w_full()
            .items_center()
            .gap_2()
            .cursor_pointer()
            .child(Icon::new(icon).small().text_color(accent))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        gpui::div()
                            .min_w_0()
                            .text_sm()
                            .text_color(accent)
                            .child(title),
                    )
                    .child(
                        gpui::div()
                            .min_w_0()
                            .text_xs()
                            .text_color(muted)
                            .child(subtitle),
                    ),
            )
            .when(selected, |el| {
                el.child(Icon::new(IconName::Check).small().text_color(accent))
            })
            .on_click(move |_event, _window, cx| {
                let _ = ws.update(cx, |this, cx| this.apply_approval_mode(mode, cx));
            })
    };

    v_flex()
        .w_full()
        .gap_2()
        .p_2()
        .child(
            h_flex()
                .w_full()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(fg)
                        .child(i18n::t("workspace-mode-title").to_string()),
                )
                .child(
                    h_flex()
                        .items_center()
                        .gap_1()
                        .child(
                            gpui::div()
                                .text_xs()
                                .text_color(info)
                                .child(i18n::t("workspace-mode-learn-more").to_string()),
                        )
                        .child(Icon::new(IconName::ArrowRight).xsmall().text_color(info)),
                ),
        )
        .child(make_row(
            ApprovalMode::OnRequest,
            i18n::t("workspace-mode-on-request-title"),
            i18n::t("workspace-mode-on-request-desc"),
            IconName::ThumbsUp,
            success,
            current == ApprovalMode::OnRequest,
        ))
        .child(make_row(
            ApprovalMode::AutoReview,
            i18n::t("workspace-mode-auto-review-title"),
            i18n::t("workspace-mode-auto-review-desc"),
            IconName::Bot,
            info,
            current == ApprovalMode::AutoReview,
        ))
        .child(make_row(
            ApprovalMode::Yolo,
            i18n::t("workspace-mode-yolo-title"),
            i18n::t("workspace-mode-yolo-desc"),
            IconName::TriangleAlert,
            danger,
            current == ApprovalMode::Yolo,
        ))
}

impl Workspace {
    /// Switch the thread's `ApprovalMode`, post a localized notice, and close
    /// the popover. Centralized so slash command, chip click, and the
    /// future settings-panel wiring all funnel through one path.
    pub(crate) fn apply_approval_mode(&mut self, mode: ApprovalMode, cx: &mut Context<Self>) {
        let mode_key = match mode {
            ApprovalMode::OnRequest => "on-request",
            ApprovalMode::AutoReview => "auto-review",
            ApprovalMode::Yolo => "yolo",
        };
        self.thread
            .update(cx, |t, cx| t.set_approval_mode(mode, cx));
        self.add_info_message(
            i18n::t_str("workspace-mode-notice", &[("mode", mode_key)]).to_string(),
            cx,
        );
        self.close_access_menu();
        cx.notify();
    }
}

// Harness shims: pub(crate) wrappers over the private turn-driving methods so
// the in-crate `harness` module (and the MCP dispatcher built on it) can drive
// a Workspace programmatically — without a real `&mut Window` or physical
// input. Each forwards to the existing private method; behavior is unchanged.
// Gated on `debug` so the shims (and their Harness consumers) are absent from
// a default build.
#[cfg(feature = "debug")]
impl Workspace {
    pub(crate) fn harness_send_message(
        &mut self,
        text: String,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.thread.read(cx).is_running() {
            return Err("thread is already running a turn".into());
        }
        self.send_user_turn(text, Vec::new(), cx);
        Ok(())
    }

    pub(crate) fn harness_approve(
        &mut self,
        decision: PermissionDecision,
        cx: &mut Context<Self>,
    ) -> bool {
        let has = self.pending_ask.is_some() || !self.pending_auths.is_empty();
        self.resolve_auth(decision, cx);
        has
    }

    pub(crate) fn harness_plan_respond(&mut self, approve: bool, cx: &mut Context<Self>) -> bool {
        let has = self.pending_plan.is_some();
        self.respond_plan(approve, cx);
        has
    }

    pub(crate) fn harness_new_thread(&mut self, cx: &mut Context<Self>) {
        self.start_new_thread(cx);
    }

    pub(crate) fn harness_open_thread(&mut self, id: String, cx: &mut Context<Self>) -> bool {
        let store = self.sidebar.read(cx).store();
        let Some(loaded) = store.update(cx, |s, cx| s.load_thread(&id, cx)) else {
            return false;
        };
        self.attach_thread(loaded, cx);
        true
    }
}

// ── Environment panel helpers ──────────────────────────────────────────────
//
// Helpers for `Workspace::render_environment_panel`.

fn env_row(
    icon: IconName,
    label: SharedString,
    trailing: Option<AnyElement>,
    theme: &Theme,
) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(Icon::new(icon).xsmall().text_color(theme.muted_foreground))
        .child(
            gpui::div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .text_color(theme.foreground)
                .child(label),
        )
        .children(trailing)
        .into_any_element()
}

/// Clamp a model id so the env card never wraps it. Ids up to
/// `ENV_MODEL_ID_MAX` ("MiniMax/MiniMax-M3[1m]") render in full; longer ones
/// are cut to the cap, trimmed by 3 chars, then suffixed with "..." — so the
/// result is exactly `ENV_MODEL_ID_MAX` chars (e.g. "MiniMax/MiniMax-M3[...").
fn truncate_env_model_id(id: String) -> String {
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= ENV_MODEL_ID_MAX {
        return id;
    }
    let head: String = chars.into_iter().take(ENV_MODEL_ID_MAX - 3).collect();
    format!("{head}...")
}

fn mode_tag(label: SharedString, active: bool, theme: &Theme) -> AnyElement {
    gpui::div()
        .px_2()
        .py_1()
        .rounded_full()
        .bg(if active {
            theme.accent.opacity(0.14)
        } else {
            theme.secondary.opacity(0.35)
        })
        .border_1()
        .border_color(if active {
            theme.accent.opacity(0.24)
        } else {
            theme.border
        })
        .text_xs()
        .text_color(if active {
            theme.foreground
        } else {
            theme.muted_foreground
        })
        .child(label)
        .into_any_element()
}

/// Compact token count display: `1m,357k`, `168k,653`, `999`.
fn format_tokens(n: u64) -> String {
    const MILLION: u64 = 1_000_000;
    const THOUSAND: u64 = 1_000;
    if n >= MILLION {
        let m = n / MILLION;
        let r = (n % MILLION) / THOUSAND;
        if r == 0 {
            format!("{m}m")
        } else {
            format!("{m}m,{r}k")
        }
    } else if n >= THOUSAND {
        let k = n / THOUSAND;
        let r = n % THOUSAND;
        if r == 0 {
            format!("{k}k")
        } else {
            format!("{k}k,{r}")
        }
    } else {
        n.to_string()
    }
}

/// Static token counter: `{arrow}{formatted}` (e.g. `↑1m,357k`).
fn counter_text(arrow: &str, value: u64) -> AnyElement {
    gpui::div()
        .child(SharedString::from(format!(
            "{arrow}{}",
            format_tokens(value)
        )))
        .into_any_element()
}

/// Cached token counter: `{arrow}{formatted} {cached}` — the `cached`
/// marker is localized so the card doesn't mix languages under non-English
/// locales.
fn counter_text_cached(arrow: &str, value: u64) -> AnyElement {
    h_flex()
        .gap_0p5()
        .child(SharedString::from(format!(
            "{arrow}{}",
            format_tokens(value)
        )))
        .child(i18n::t("workspace-env-cached"))
        .into_any_element()
}
