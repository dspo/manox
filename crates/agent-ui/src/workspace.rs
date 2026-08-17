//! Top-level workspace view.
//!
//! Holds `Entity<agent::Thread>` + `Entity<Sidebar>`; `cx.subscribe` handles:
//! - `ThreadEvent`: text/thinking/tool deltas go to `ConversationState`; `ToolCallAuthorization` opens an approval overlay;
//!   the terminal `Stop` (non-ToolUse) triggers `save_thread`.
//! - `SidebarEvent`: new conversation / open history / delete.
//!
//! Enter in the input box → append a user message + run_turn + persist (the sidebar shows the new entry immediately).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent::PermissionDecision;
use agent::collaboration_mode::PlanReviewChoice;
use agent::i18n;
use agent::language_model::StopReason;
use agent::thread::ApprovalMode;
use agent::webview_host::BrowserTabId;
use agent::{Thread, ThreadEvent, ThreadId, save_thread};
use gpui::DismissEvent;
use gpui::{
    Anchor, Animation, AnimationExt as _, AnyElement, App, Context, Entity, FocusHandle,
    FollowMode, ListAlignment, ListOffset, ListState, MouseButton, Pixels, Render, ScrollHandle,
    SharedString, Subscription, WeakEntity, Window, anchored, deferred, ease_out_quint, prelude::*,
    px,
};
use gpui::{ClickEvent, CursorStyle, DragMoveEvent, MouseUpEvent};
/// Shared across both harnesses: workspace struct fields hold
/// `Option<Entity<PopupMenu>>` regardless of feature.
use gpui_component::menu::PopupMenu;
use gpui_component::{
    ActiveTheme as _, Disableable as _, ElementExt as _, Icon, IconName, Sizable as _, Size,
    TITLE_BAR_HEIGHT, Theme, TitleBar,
    button::{Button, ButtonCustomVariant, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, Paste, RopeExt},
    v_flex,
};
use gpui_component::{
    StyledExt as _,
    menu::PopupMenuItem,
    tab::{Tab, TabBar},
    tag::{Tag, TagVariant},
};
/// `WindowExt::push_notification` + `Notification` are shared: the
/// ChatGPT.app launch path (#410) reports outcomes under either harness.
use gpui_component::{WindowExt as _, notification::Notification, tooltip::Tooltip};
use manox_components::markdown::HeadingMode;
use manox_components::markdown::Markdown;

use crate::cockpit::{CockpitPhase, format_elapsed};
use crate::conversation::ConvItem;
use crate::conversation::{ApplyOutcome, ConversationState, UserImage, UserTurnMeta};
use crate::external_session::{
    ExternalSession, ResumeSidecar, SessionKind, list_sidecars, merge_external_summaries,
    remove_sidecar, resume_args, write_sidecar,
};
use crate::views::browser_view::BrowserView;
use crate::views::centered;
use crate::views::completion::{
    CompletionState, SelectHandler, build_replacement, detect, mention_source, render_completion,
    slash_source,
};
use crate::views::composer_menu::{
    PendingAttachment, build_plus_menu, load_attachment, render_attachment_chips,
};
use crate::views::message::MessageItem;
use crate::views::popup_menu;
use crate::views::settings::{SettingsEvent, SettingsView};
use crate::views::sidebar::{Sidebar, SidebarEvent};
use crate::views::turn_navigator::{TurnNavigator, TurnNavigatorEvent, collect_user_turns};
use crate::{
    CloseBrowserTab, CloseTerminalTab, FocusTerminal, NewTerminalTab, OpenBrowserTab,
    ToggleTurnNavigator,
};
use crate::{FocusConversation, OpenSettings};
use terminal::Terminal;
use terminal_ui::TerminalView;

pub(crate) type ThreadEntity = agent::Thread;

/// A tab in the right observation pane. `Editor` is the markdown composer
/// (Write/Preview); `Browser(id)` is an untrusted embedded webview (see
/// [`BrowserView`]).
#[derive(Clone, Debug)]
enum RightTab {
    Editor,
    Browser(BrowserTabId),
    /// A team member's observation panel, keyed by member name.
    Member(String),
    /// A pi sub-agent's observation panel, keyed by Agent tool-call id.
    Subagent(String),
}

/// A parsed `AskUserQuestion` prompt awaiting the user's selections.
struct PendingAsk {
    id: String,
    questions: Vec<AskQuestion>,
    /// Per-question toggled option flags, aligned with `questions[i].options`.
    selections: Vec<Vec<bool>>,
}

#[derive(Clone, PartialEq)]
pub(crate) struct AskCardSnapshot {
    pub id: String,
    pub step: usize,
    pub total: usize,
    pub transition_gen: u64,
    pub question: AskCardQuestion,
    pub selections: Vec<bool>,
}

#[derive(Clone, PartialEq)]
pub(crate) struct AskCardQuestion {
    pub question: String,
    pub header: String,
    pub multi_select: bool,
    pub options: Vec<AskCardOption>,
}

#[derive(Clone, PartialEq)]
pub(crate) struct AskCardOption {
    pub label: String,
    pub description: String,
    pub recommended: bool,
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
    recommended: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ComposerPlaceholderMode {
    Normal,
    FollowUp,
    Ask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ComposerPlacement {
    Hidden,
    Hero,
    Footer,
}

fn composer_placement(editor_open: bool, first_screen: bool) -> ComposerPlacement {
    if editor_open {
        ComposerPlacement::Hidden
    } else if first_screen {
        ComposerPlacement::Hero
    } else {
        ComposerPlacement::Footer
    }
}

fn editor_can_submit(
    history_loading: bool,
    running: bool,
    has_pending_ask: bool,
    text: &str,
) -> bool {
    !history_loading && !running && !has_pending_ask && !text.trim().is_empty()
}

struct DeferredUserTurn {
    text: String,
    images: Vec<agent::language_model::MessageContent>,
    meta: UserTurnMeta,
    ui: agent::MessageUiMetadata,
    user_images: Vec<UserImage>,
}

/// A thread parked in the background while still running a turn. The held
/// `Subscription` is a minimal handler that only tracks terminal `Stop`/`Error`
/// to clear the running indicator, mark the thread unread, and drop it from
/// `background_threads` — it never touches `conversation`/`self.thread`, so a
/// background thread's events cannot be misattributed to the foreground thread.
struct BackgroundThread {
    entity: Entity<ThreadEntity>,
    _sub: Subscription,
}

/// Lifecycle of a follow-up submitted while a turn is running. A queued item
/// renders above the composer; clicking Steer moves an optimistic bubble into
/// the conversation immediately while the canonical message waits for a safe
/// join point in `Thread::pending_steer`.
enum FollowUpState {
    /// Parked, waiting to flush as the next user turn at terminal Stop (or to
    /// be promoted to a steer via the Steer action).
    Queued,
    /// Handed to the thread's steer queue and represented by a pending bubble
    /// in the message list. Hidden from the composer queue while in flight.
    SteerPending { message_id: String },
    /// The running turn exited (Abort/Error) before draining it — stranded.
    /// Carries the steer message id so a later `SteerInjected` (if the drain
    /// actually did fire after the premature `Stop`) can still heal the card
    /// into a real steered bubble instead of leaving a false "failed" marker.
    /// Stays parked, marked red, retryable via the Steer action.
    Failed { message_id: String },
}

/// A follow-up submitted while a turn is running. Every new item starts queued;
/// only an explicit Steer action promotes it to `SteerPending`.
struct QueuedFollowUp {
    turn: DeferredUserTurn,
    state: FollowUpState,
}

/// Which shared registry backs a registry slash turn — a markdown
/// prompt-macro (`agent::command`) or a skill (`agent::skill`).
#[derive(Clone, Copy)]
enum RegistryTurnKind {
    Command,
    Skill,
}

/// A submitted plan file awaiting the user's review verdict. Carries the
/// plan file path, its resolved title, and the file content rendered into
/// the review card.
struct PendingPlanReview {
    plan_file: String,
    title: String,
    content: String,
}

pub struct Workspace {
    pub(crate) cwd: PathBuf,
    pub(crate) thread: Entity<ThreadEntity>,
    /// Threads that were running when the user switched away. Holding strong
    /// references keeps their `run_turn_loop` tasks alive so they can finish
    /// in the background and persist via the spawned-task save backstop. Each
    /// carries a minimal subscription so a terminal `Stop`/`Error` arriving
    /// while parked marks the thread unread for the sidebar red dot.
    background_threads: Vec<BackgroundThread>,
    /// Generation counter for git-status refreshes: bumping it means any
    /// prior in-flight refresh self-cancels instead of overwriting newer
    /// state. The refresh runs on the global tokio runtime and delivers its
    /// result back via `async_channel`, the same bridge the worktree tool uses.
    git_status_gen: u64,
    pub(crate) sidebar: Entity<Sidebar>,
    pub(crate) conversation: Entity<ConversationState>,
    pub(crate) input_state: Entity<InputState>,
    /// Per-thread unsent composer text, keyed by thread id. Saved when
    /// switching away and restored on return, so each thread keeps its own
    /// in-progress draft instead of a single shared input bleeding across.
    drafts: HashMap<String, String>,
    /// Composer history-recall position into the newest-first user-turn
    /// texts; -1 means not recalling. Derived state: any edit that makes
    /// the input value diverge from `turns[recall_index]` leaves recall
    /// mode implicitly.
    recall_index: i64,
    /// Per-thread right-side editor text, keyed by thread id. The editor pane
    /// is a right-side resource of the thread it was written for: switching
    /// away stashes the outgoing text, switching back restores it, so no
    /// thread ever sees another thread's draft and returning recovers the
    /// text. Mirrors `drafts` (the composer's per-thread stash).
    editor_drafts: HashMap<String, String>,
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
    /// Stable markdown preview entity kept across renders so the source is
    /// only re-parsed when the draft changes (not every frame).
    editor_preview_md: Option<Entity<Markdown>>,
    /// Explicit pixel-anchored scroll state for the preview column. Mirrors the
    /// message-list pattern: an explicit handle (not entity-state scroll) keeps
    /// the offset stable and defaulting to the top, and a `flex_1`-sized (not
    /// `h_full`-percentage) scroll container reliably engages `overflow_y_scroll`
    /// instead of letting content overflow and clip.
    editor_preview_scroll: ScrollHandle,
    /// Peer right-pane tabs for the editor, member/sub-agent observers,
    /// browser, and plan preview. The pane renders while non-empty;
    /// `editor_open` tracks whether the Editor tab specifically is active.
    right_tabs: Vec<RightTab>,
    active_right_tab: usize,
    /// Team chip popover open state (composer chip).
    team_chip_open: bool,
    /// Member observation panels keyed by member name; lifetime is the
    /// tab's (dropped when the tab closes).
    member_panels: HashMap<String, Entity<crate::views::member_panel::MemberPanel>>,
    /// Live sub-agent observation panels keyed by Agent tool-call id.
    subagent_panels: HashMap<String, Entity<crate::views::subagent_panel::SubagentPanel>>,
    /// Accumulated child-session events per Agent tool-call id, so a panel
    /// opened mid-run backfills from the start.
    subagent_transcripts: HashMap<String, Vec<agent::SubagentChildEvent>>,
    /// Lazily-built browser tab entities, keyed by `BrowserTabId`. A browser
    /// tab keeps its `BrowserView` (and the underlying native webview) across
    /// tab switches; dropped when the tab closes, which detaches the native
    /// view via [`manox_webview::webview::WebView`]'s `Drop`.
    pub(crate) browser_views: BTreeMap<BrowserTabId, Entity<BrowserView>>,
    /// Editor pane width, driven by dragging the divider. In-memory only.
    editor_width: Pixels,
    /// Sidebar width, driven by dragging the divider on its right edge.
    /// In-memory only; never persisted so the user's drag state stays
    /// session-local.
    sidebar_width: Pixels,
    /// A pending `AskUserQuestion` card rendered inline in the message list.
    pending_ask: Option<PendingAsk>,
    /// Tool row currently carrying the Workspace-derived ask snapshot. This is
    /// synchronized before list construction; the row factory itself remains
    /// a read-only projection during measurement and prepaint.
    ask_snapshot_item: Option<Entity<MessageItem>>,
    /// A completed plan awaiting the user's implement / clear-context verdict,
    /// rendered as the inline plan-review drawer card.
    pending_plan_review: Option<PendingPlanReview>,
    /// Per-thread stash of `pending_plan_review`, keyed by thread id. A
    /// pending plan never enters persisted messages (the `<proposed_plan>`
    /// block is stripped before the assistant text is saved), so without this
    /// stash the verdict card + buttons vanish on a switch-away/switch-back
    /// round-trip. Mirrors `drafts`: populated on switch-away, drained on
    /// switch-back.
    pending_plans: HashMap<String, PendingPlanReview>,
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
    /// Access-chip dropdown (Normal / Danger mode). Mirrors the model selector pattern.
    access_open: bool,
    /// Project-chip dropdown (recent projects + new project submenu).
    project_chip_open: bool,
    project_chip_menu: Option<Entity<PopupMenu>>,
    project_chip_menu_sub: Option<Subscription>,
    /// Composer typeahead completion popover (`/` commands, `@` skills/agents).
    /// `None` when no trigger token is active at the caret. A pure render
    /// overlay — it never grabs focus, so the `InputState` keeps focus and the
    /// query filters live on every keystroke.
    completion: Option<CompletionState>,
    /// Searchable, newest-first snapshot of the active thread's user turns.
    turn_navigator: Option<Entity<TurnNavigator>>,
    turn_navigator_sub: Option<Subscription>,
    turn_navigator_previous_focus: Option<FocusHandle>,
    /// Follow-ups submitted while a turn is running. Steer items are injected
    /// into the running turn at the next safe join point; queue items flush as
    /// the next user turn at `TurnFinished`.
    queued_follow_ups: std::collections::VecDeque<QueuedFollowUp>,
    /// Session-only per-thread queue stash. Switching tasks moves the active
    /// deque here and restores it on return; no database persistence is used.
    queued_follow_ups_by_thread: HashMap<String, std::collections::VecDeque<QueuedFollowUp>>,
    /// Tracks which composer placeholder is installed, so render only mutates
    /// the input state on mode transitions.
    composer_placeholder_mode: ComposerPlaceholderMode,
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
    /// Scroll/virtualization state for the message column, held natively by
    /// `gpui::ListState`. `ListAlignment::Bottom` gives chat-log semantics:
    /// short histories sit at the bottom, long ones scroll. `FollowMode::Tail`
    /// pins to the live end on each layout while following, disengages on an
    /// upward user scroll, and re-arms when a scroll lands back at the bottom.
    /// `MSG_LIST_OVERDRAW` rows below the viewport are pre-measured; a width
    /// change invalidates every cached height, and visible rows re-measure
    /// every frame (so a height change without an explicit signal self-
    /// corrects). Count changes are reconciled via `splice`, in-place
    /// mutations via `remeasure_items`, both driven by `ApplyOutcome`. Only
    /// the visible items render.
    list_state: ListState,
    /// Exact width of the list child from the previous prepaint. Official GPUI
    /// at the pinned revision does not invalidate off-screen row heights when
    /// this changes, so the application explicitly remeasures the cache.
    message_list_width: crate::views::MessageListWidthInvalidator,
    /// Cached `items().len()`; the event handler reconciles the list count via
    /// `splice` whenever the conversation grows or shrinks.
    list_count: usize,
    /// Messages from the thread's mirrored history already rendered into the
    /// conversation. The streaming preview (`ThreadEvent::HistoryProgress`)
    /// appends only `thread.messages()[history_rendered..]`; reset whenever
    /// the conversation is rebuilt from scratch.
    history_rendered: usize,
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
    /// Generation counter for the goal elapsed-time ticker. Incremented when a
    /// goal is cleared or the active thread changes so the prior ticker
    /// self-terminates instead of notifying a stale chip. Mirrors
    /// `settings_transition_gen`.
    goal_ticker_gen: u64,
    /// True while the active thread has a turn in flight, so the Thinking
    /// status row's "for Xs" counter ticks every second. Set on `TurnStarted`,
    /// cleared on a terminal `Stop`/`Error`. The ticker task polls this and
    /// self-terminates when it goes false.
    turn_active: bool,
    /// Generation counter for the thinking elapsed-time ticker. Incremented
    /// on every `TurnStarted` and on thread switch so a prior ticker
    /// self-terminates instead of driving a stale container.
    thinking_ticker_gen: u64,
    /// Lazily created on the first `enter_settings` call so we don't pay the
    /// cost when the user never opens Settings.
    settings_view: Option<Entity<SettingsView>>,
    settings_sub: Option<Subscription>,
    /// The terminal tab's view, lazily created on the first `FocusTerminal` /
    /// `NewTerminalTab`. `None` until then. Dropped on `CloseTerminalTab`.
    terminal_view: Option<Entity<TerminalView>>,
    /// Right-hand context rail. Owns the cockpit state (run phase, the model's
    /// plan snapshot, per-cell counter animation state) that used to live
    /// directly on `Workspace`, plus strong handles to the active thread and conversation
    /// it renders against. Writes flow through `self.context_rail.update`.
    context_rail: Entity<crate::views::context_rail::ContextRail>,
    /// Live external agent CLI sessions (claude / codex / copilot) launched from
    /// the sidebar `+` menu. In-memory only — never persisted. Each owns its
    /// `TerminalView` plus a shared `Arc<SessionHandle>` so the close path can
    /// `kill` the agent explicitly.
    pub(crate) external_sessions: Vec<crate::external_session::ExternalSession>,
    /// Unclosed external sessions from previous runs, restored from their
    /// sidecars at startup. Rendered in the sidebar as resumable rows; clicking
    /// one re-spawns the CLI with its resume flag. Never auto-resumed.
    resumable_external: Vec<ResumeSidecar>,
    /// Ids of resumable rows whose CLI re-spawn is in flight; the sidebar
    /// shows a loading indicator on each such row. A set (not a single slot)
    /// so resuming two rows concurrently cannot steal each other's spinner.
    resuming_external: std::collections::HashSet<String>,
    /// The currently-displayed external session id when
    /// `view_mode == ExternalSession`. Mirrors `terminal_view`'s "one at a
    /// time" model; switching away parks the session (its terminal keeps
    /// running) rather than killing it.
    active_external: Option<String>,
}

/// Top-level rendering mode of the Workspace window. `Settings` and
/// `Terminal` are full-pane switches off the default `Workspace` (conversation)
/// mode; `ExternalSession` shows an external agent CLI's TUI terminal in place
/// of the conversation. Future overlays can extend this enum rather than
/// carrying parallel `bool` flags.
#[derive(Default)]
enum ViewMode {
    #[default]
    Workspace,
    Settings,
    Terminal,
    ExternalSession,
}

/// Right-side composer width. Wide enough for rendered markdown
/// (headings, lists, code blocks) alongside the 1100px window.
const EDITOR_PANEL_WIDTH: f32 = 640.;
const EDITOR_MIN_WIDTH: f32 = 320.;
const EDITOR_MAX_WIDTH: f32 = 960.;
/// Width of the drag handle between the message column and the right side
/// view (the editor pane).
const EDITOR_DIVIDER_WIDTH: f32 = 6.;
// Mirrors `views/sidebar.rs` (`Sidebar` renders at `w(px(SIDEBAR_WIDTH))`).
// Kept here so the editor pane's resize clamp can reserve space for the
// sidebar + main column without depending on the sidebar's internals.
const SIDEBAR_WIDTH: f32 = 260.;
const SIDEBAR_MIN_WIDTH: f32 = 200.;
const SIDEBAR_MAX_WIDTH: f32 = 480.;
const SIDEBAR_DIVIDER_WIDTH: f32 = 6.;
/// Floor for the message column width when the right side view (editor
/// pane) is dragged wide.
const MAIN_MIN_WIDTH: f32 = 160.;

/// Trailing overdraw for the message list: rows within this many pixels
/// below the viewport are pre-measured so scrolling never pops an
/// unmeasured row into view.
const MSG_LIST_OVERDRAW: Pixels = px(2048.);

#[derive(Clone, Copy, Debug, PartialEq)]
struct TurnNavigatorLayout {
    left_inset: Pixels,
    right_inset: Pixels,
    panel_width: Pixels,
}

fn turn_navigator_layout(
    window_width: Pixels,
    sidebar_width: Pixels,
    right_pane_width: Option<Pixels>,
    show_context_rail: bool,
) -> TurnNavigatorLayout {
    let left_inset = sidebar_width + px(SIDEBAR_DIVIDER_WIDTH);
    let right_pane_inset = right_pane_width
        .map(|width| width + px(EDITOR_DIVIDER_WIDTH))
        .unwrap_or(px(0.));
    let context_inset = if show_context_rail {
        px(crate::views::context_rail::ENV_CONTENT_INSET)
    } else {
        px(0.)
    };
    let right_inset = right_pane_inset + context_inset;
    let available = window_width - left_inset - right_inset - px(24.);
    let panel_width = if available <= px(0.) {
        px(0.)
    } else if available < px(480.) {
        available
    } else {
        px(480.)
    };

    TurnNavigatorLayout {
        left_inset,
        right_inset,
        panel_width,
    }
}

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

enum RecallDirection {
    Up,
    Down,
}

#[derive(Debug)]
enum RecallStep {
    None,
    Recall(String),
    Clear,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        // An unbound conversation must not inherit the launch terminal's
        // cwd as its working directory — that is an arbitrary project dir
        // (or `/` under a GUI launch). Home is the neutral default; binding
        // a project via the chip / project "+" overrides it later.
        if let Some(home) = agent::paths::home_dir() {
            cwd = home;
        }
        let thread = { Thread::landing(cwd.clone(), cx) };

        let input_state = cx.new(|cx| {
            InputState::new(window, cx)
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
        let conversation = cx.new(|_| ConversationState::new());
        let context_rail =
            { cx.new(|_| crate::views::context_rail::ContextRail::new(thread.clone())) };
        let weak_ws = cx.weak_entity();
        context_rail.update(cx, |r, _| r.set_workspace(weak_ws));

        let mut ws = Self {
            cwd,
            thread,
            background_threads: Vec::new(),
            git_status_gen: 0,
            sidebar,
            conversation: conversation.clone(),
            input_state,
            drafts: HashMap::new(),
            editor_drafts: HashMap::new(),
            editor_state,
            editor_open: false,
            editor_preview: false,
            editor_preview_md: None,
            editor_preview_scroll: ScrollHandle::new(),
            right_tabs: Vec::new(),
            active_right_tab: 0,
            team_chip_open: false,
            member_panels: HashMap::new(),
            subagent_panels: HashMap::new(),
            subagent_transcripts: HashMap::new(),
            browser_views: BTreeMap::new(),
            editor_width: px(EDITOR_PANEL_WIDTH),
            sidebar_width: px(SIDEBAR_WIDTH),
            pending_ask: None,
            ask_snapshot_item: None,
            pending_plan_review: None,
            pending_plans: HashMap::new(),
            ask_step: 0,
            ask_transition_gen: 0,
            model_open: false,
            model_menu: None,
            model_menu_sub: None,
            plus_open: false,
            plus_menu: None,
            plus_menu_sub: None,
            access_open: false,
            project_chip_open: false,
            project_chip_menu: None,
            project_chip_menu_sub: None,
            completion: None,
            recall_index: -1,
            turn_navigator: None,
            turn_navigator_sub: None,
            turn_navigator_previous_focus: None,
            queued_follow_ups: std::collections::VecDeque::new(),
            queued_follow_ups_by_thread: HashMap::new(),
            composer_placeholder_mode: ComposerPlaceholderMode::Normal,
            pending_attachments: Vec::new(),
            project_picker_pending: false,
            blank_project_parent: None,
            blank_project_name_input: None,
            thread_sub: None,
            sidebar_sub: None,
            input_sub: None,
            editor_sub: None,
            list_state: ListState::new(0, ListAlignment::Bottom, MSG_LIST_OVERDRAW),
            message_list_width: crate::views::MessageListWidthInvalidator::default(),
            list_count: 0,
            history_rendered: 0,
            view_mode: ViewMode::default(),
            exiting_settings: false,
            settings_transition_gen: 0,
            goal_popover_open: false,
            goal_ticker_gen: 0,
            turn_active: false,
            thinking_ticker_gen: 0,
            settings_view: None,
            settings_sub: None,
            terminal_view: None,
            context_rail,
            external_sessions: Vec::new(),
            resumable_external: list_sidecars(),
            resuming_external: std::collections::HashSet::new(),
            active_external: None,
        };
        ws.thread_sub = Some(ws.subscribe_thread(cx));
        ws.sidebar_sub = Some(ws.subscribe_sidebar(window, cx));
        ws.input_sub = Some(ws.subscribe_input(window, cx));
        ws.editor_sub = Some(ws.subscribe_editor(window, cx));
        // The sidebar lists the restored resumable rows from the first frame;
        // nothing is resumed until the user clicks one.
        ws.sync_sidebar_external(cx);
        // Focus the composer so typing works immediately on the hero screen.
        ws.input_state.update(cx, |s, cx| s.focus(window, cx));
        ws
    }

    /// Rebuild the conversation view from the current thread's message
    /// history. The harness-pi restore hook: `PiSession` rebuilds the pi
    /// session asynchronously, then asks the workspace to re-render once the
    /// restored history lands.
    pub(crate) fn rebuild_conversation_from_thread(&mut self, cx: &mut Context<Self>) {
        let messages: Vec<agent::Message> = self.thread.read(cx).messages().to_vec();
        let usage = self.thread.read(cx).request_token_usage().clone();
        let notes = self.thread.read(cx).ui_notes().to_vec();
        let role = self.model_label(cx);
        let weak = cx.weak_entity();
        let running = self.thread.read(cx).is_running();
        let cwd = thread_cwd(&self.thread, cx);
        let new_conv = cx.new(|cx| {
            ConversationState::rebuild_from_messages(
                &messages,
                &usage,
                &role,
                running,
                &notes,
                crate::conversation::ApplyCtx {
                    weak: weak.clone(),
                    cwd,
                },
                cx,
            )
        });
        self.conversation = new_conv;
        let count = self.conversation.read(cx).items().len();
        self.list_state.reset(count);
        self.list_count = count;
        // The authoritative list is fully rendered now; future preview
        // batches append from here.
        self.history_rendered = messages.len();
        // `Tail` natively pins to the end and keeps following; an upward user
        // scroll disengages it and landing back at the bottom re-arms it.
        self.list_state.set_follow_mode(FollowMode::Tail);
        cx.notify();
    }

    fn subscribe_thread(&self, cx: &mut Context<Self>) -> Subscription {
        let thread = self.thread.clone();
        cx.subscribe(&thread, |this, _thread, ev: &ThreadEvent, cx| {
            match ev {
                ThreadEvent::ToolCallAuthorization { id, input, .. } => {
                    // Every authorization — interactive tools, bubbled
                    // sub-agent auth, AutoPilot escalations — surfaces as the
                    // AskUserQuestion card; the question payload carries the
                    // decision options.
                    this.pending_ask = parse_pending_ask(id.clone(), input.clone());
                    this.ask_step = 0;
                    this.ask_transition_gen = this.ask_transition_gen.wrapping_add(1);
                    this.context_rail.update(cx, |r, cx| {
                        r.cockpit_phase = CockpitPhase::AwaitingApproval;
                        cx.notify();
                    });
                    // The card is visible now, but the marker survives a
                    // switch-away before the verdict: a parked thread blocked
                    // on this authorization still shows the sidebar badge.
                    let thread_id = this.thread.read(cx).id.0.clone();
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| s.mark_pending_auth(&thread_id, true, cx));
                    cx.notify();
                }
                ThreadEvent::PlanReady { plan_file, title } => {
                    // The model submitted the plan through `ProposePlan`; read
                    // the file once for the review card body.
                    let content = std::fs::read_to_string(plan_file.as_str()).unwrap_or_default();
                    let weak = cx.weak_entity();
                    let role = this.model_label(cx);
                    this.conversation.update(cx, |c, cx| {
                        c.push_plan_review(title.clone(), content.clone(), role, weak, cx);
                    });
                    this.pending_plan_review = Some(PendingPlanReview {
                        plan_file: plan_file.clone(),
                        title: title.clone(),
                        content,
                    });
                    // Persist the pending verdict so a restart re-surfaces
                    // the card (the engine re-emits PlanReady on Ready).
                    this.thread
                        .update(cx, |t, cx| t.set_plan_review_pending(true, cx));
                    // The sidebar row pauses its spinner (blue static) while
                    // the verdict is due; `respond_plan_review` releases it.
                    let thread_id = this.thread.read(cx).id.0.clone();
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| s.mark_pending_plan(&thread_id, true, cx));
                    this.sync_list_count(cx);
                    // The finalized plan surfaces at the tail; reveal it like any
                    // user-initiated jump to the live end.
                    this.list_state.set_follow_mode(FollowMode::Tail);
                    cx.notify();
                }
                ThreadEvent::PlanModeChanged { .. } => {
                    // Refresh the plan chip.
                    cx.notify();
                }
                ThreadEvent::PlanUpdated { snapshot } => {
                    // Live plan progress: mirror onto the rail as the model
                    // publishes it (an empty snapshot clears the section).
                    let snapshot = snapshot.clone();
                    this.context_rail
                        .update(cx, |r, cx| r.set_plan(snapshot, cx));
                }
                ThreadEvent::ApprovalModeChanged { .. } => {
                    // Refresh the access chip + Danger badge; no conversation item.
                    cx.notify();
                }
                // The pi actor restores session history asynchronously; the
                // attach-time conversation rebuild saw an empty transcript,
                // so rebuild once the authoritative history lands (sidebar
                // open). Without this a reopened thread strands on the
                // loading screen, and a forked session's preview is corrected
                // to the authoritative active branch here.
                ThreadEvent::HistoryRestored => {
                    this.rebuild_conversation_from_thread(cx);
                    // Re-seed the rail's plan now that the authoritative
                    // transcript has landed: the attach-time seed ran against
                    // an empty transcript and an unfilled sidecar mirror
                    // (`Ready` is async), so a restarted session's plan would
                    // otherwise wait for a manual thread switch.
                    let messages = this.thread.read(cx).messages().to_vec();
                    if let Some(snapshot) = agent::plan::rebuild_from_messages(&messages)
                        .or_else(|| this.thread.read(cx).persisted_plan().cloned())
                    {
                        this.context_rail
                            .update(cx, |r, cx| r.set_plan(snapshot, cx));
                    }
                }
                // A display-only preview batch landed while the authoritative
                // restore is still running: append the newly available
                // messages incrementally (paint as many as are loaded),
                // keeping the list pinned at the tail while the user waits.
                // Input stays gated on the thread's `HistoryPhase::Loading`.
                ThreadEvent::HistoryProgress => {
                    if this.thread.read(cx).history_phase().is_loading() {
                        let messages: Vec<agent::Message> =
                            this.thread.read(cx).messages().to_vec();
                        if messages.len() > this.history_rendered {
                            let new_messages = messages[this.history_rendered..].to_vec();
                            let usage = this.thread.read(cx).request_token_usage().clone();
                            let role = this.model_label(cx);
                            let cwd = thread_cwd(&this.thread, cx);
                            let weak = cx.weak_entity();
                            let outcome = this.conversation.update(cx, |c, cx| {
                                c.append_history_messages(
                                    &new_messages,
                                    &usage,
                                    &role,
                                    crate::conversation::ApplyCtx { weak, cwd },
                                    cx,
                                )
                            });
                            this.history_rendered = messages.len();
                            this.apply_list_outcome(outcome, cx);
                        }
                    }
                }
                ThreadEvent::ApprovalDecision {
                    tool_title,
                    verdict,
                    ..
                } => {
                    // Show a brief autopilot decision record in the
                    // MessageList, mirroring Codex's approval cell.
                    let msg = match &verdict {
                        agent::approval::ReviewVerdict::Allow => i18n::t_str(
                            "workspace-approval-autopilot-allowed",
                            &[("tool", tool_title.as_str())],
                        )
                        .to_string(),
                        agent::approval::ReviewVerdict::Ask { reason } => i18n::t_str(
                            "workspace-approval-autopilot-escalated",
                            &[("tool", tool_title.as_str()), ("reason", reason.as_str())],
                        )
                        .to_string(),
                    };
                    this.add_info_message(msg, cx);
                }
                ThreadEvent::ModelChanged { from, to } => {
                    // Persist a model_change event to the thread's event stream.
                    // The conversation view itself stays unchanged (no item).
                    let _ = (from, to);
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
                    // network latency). Terminal `TurnFinished`/`Error` below
                    // clear it. A new turn also supersedes a stale error flag
                    // from the previous turn.
                    let thread_id = this.thread.read(cx).id.0.clone();
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| {
                        s.mark_running(&thread_id, cx);
                        s.set_errored(&thread_id, false, cx);
                    });
                    // Drive the Thinking status row's per-second "for Xs"
                    // counter while this turn is live. The ticker polls
                    // `turn_active` and self-terminates on the terminal stop.
                    this.turn_active = true;
                    this.spawn_thinking_ticker(cx);
                }
                ThreadEvent::TurnFinished {
                    cancelled,
                    failed,
                    stranded_steer_ids,
                } => {
                    // Seal the conversation's streaming state at the
                    // authoritative turn boundary: a turn that ended without
                    // a terminal `Stop` (provider error, stream closed without
                    // `MessageStop`) would otherwise leave its activity
                    // segment accepting entries — a perpetual spinner and the
                    // root condition for the next turn's thinking folding into
                    // a segment above the new user bubble.
                    let weak = cx.weak_entity();
                    let role = this.model_label(cx);
                    let cwd = thread_cwd(&this.thread, cx);
                    let outcome = this.conversation.update(cx, |c, cx| {
                        c.apply(
                            ev,
                            &role,
                            None,
                            crate::conversation::ApplyCtx { weak, cwd },
                            cx,
                        )
                    });
                    this.apply_list_outcome(outcome, cx);
                    // This is the authoritative end-of-turn boundary: unlike a
                    // provider Stop event, `Thread::is_running()` is already
                    // false, so a queued follow-up can safely start a new turn.
                    this.mark_stranded_steers_failed(stranded_steer_ids, cx);
                    let thread_id = this.thread.read(cx).id.0.clone();
                    save_thread(this.thread.clone(), true, cx);
                    // Sidebar running indicator: the turn released the running
                    // slot, so the row stops spinning. A successful or
                    // cancelled turn also supersedes a stale error flag.
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| {
                        s.mark_idle(&thread_id, cx);
                        s.mark_pending_plan(&thread_id, false, cx);
                        if !*failed {
                            s.set_errored(&thread_id, false, cx);
                        }
                    });
                    this.turn_active = false;
                    this.background_threads
                        .retain(|b| b.entity.read(cx).id.0 != thread_id);
                    // A turn that ended while a plan-review card is still up
                    // (cancel / abnormal stop) demotes it, mirroring `Error` —
                    // the verdict is moot once the loop released the turn.
                    if this.pending_plan_review.take().is_some() {
                        this.conversation
                            .update(cx, |c, cx| c.consume_plan_review(cx));
                        this.list_state.remeasure();
                    }
                    this.spawn_git_status_refresh(cx);
                    // Dispatch last: `run_turn` emits `TurnStarted`
                    // synchronously, so no terminal bookkeeping above may run
                    // afterward and overwrite the new turn's running state.
                    if !cancelled {
                        this.flush_queued_follow_ups(cx);
                    }
                    cx.notify();
                }
                ThreadEvent::Stop(reason) => {
                    let weak = cx.weak_entity();
                    let role = this.model_label(cx);
                    let usage = this.thread.read(cx).last_request_token_usage();
                    let cwd = thread_cwd(&this.thread, cx);
                    let outcome = this.conversation.update(cx, |c, cx| {
                        c.apply(
                            ev,
                            &role,
                            usage,
                            crate::conversation::ApplyCtx { weak, cwd },
                            cx,
                        )
                    });
                    this.apply_list_outcome(outcome, cx);
                    // `Stop` flips streaming flags off, so finalized bodies switch
                    // to full `Markdown` layout and may grow a frame or two later;
                    // the list's Absolute scroll anchor holds the viewport steady
                    // across that growth, and `FollowMode::Tail` — if still
                    // engaged — re-pins to the end on the next layout.
                    // Persist on terminal state (not the ToolUse mid-state).
                    if !matches!(reason, StopReason::ToolUse) {
                        save_thread(this.thread.clone(), true, cx);
                        // `Stop` is a provider-round boundary. Queue draining,
                        // idle state, and git refresh wait for `TurnFinished`.
                    }
                    cx.notify();
                }
                ThreadEvent::PrefixStability { .. } => {
                    // Per-turn cache stability signal. The composer chip that
                    // used to render this was removed in #62; the event stays
                    // emitted for any future telemetry/debug subscriber.
                    cx.notify();
                }
                ThreadEvent::GoalChanged { goal } => {
                    // Bump the ticker generation so any prior ticker
                    // self-terminates; start a fresh ticker only on activation.
                    let active = goal
                        .as_ref()
                        .map(|g| !g.status.is_terminal())
                        .unwrap_or(false);
                    this.goal_ticker_gen = this.goal_ticker_gen.wrapping_add(1);
                    if active {
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
                ThreadEvent::SteerInjected { message_id } => {
                    // The running turn drained the steer. Confirm the bubble
                    // that was inserted optimistically when the user clicked;
                    // do not push a duplicate.
                    this.consume_steered_follow_up(message_id, cx);
                }
                _ => {
                    // Live monitors / background bash keep the loop able to
                    // self-advance; mirror the per-thread running-task check
                    // into the store so the sidebar keeps the row spinning
                    // even with no turn in flight. The event still falls
                    // through to the conversation's task-card dispatch below.
                    if let ThreadEvent::BackgroundTaskUpdated { .. } = ev {
                        let thread_id = this.thread.read(cx).id.0.clone();
                        let store = agent::thread_store_global();
                        store.update(cx, |s, cx| {
                            s.mark_background_work(
                                &thread_id,
                                agent::background_task::thread_has_running_tasks(&thread_id),
                                cx,
                            );
                        });
                    }
                    // Tool traffic past a pending authorization proves the
                    // verdict resolved (the call resumed or settled); drop
                    // the sidebar badge. The `PendingApproval` card itself
                    // precedes the authorization event and must not clear it.
                    if matches!(
                        ev,
                        ThreadEvent::ToolCall { status, .. }
                            if !matches!(
                                status,
                                agent::thread::ToolCallStatus::PendingApproval
                            )
                    ) || matches!(
                        ev,
                        ThreadEvent::ToolResult { .. } | ThreadEvent::ToolOutput { .. }
                    ) {
                        let thread_id = this.thread.read(cx).id.0.clone();
                        let store = agent::thread_store_global();
                        store.update(cx, |s, cx| s.mark_pending_auth(&thread_id, false, cx));
                    }
                    // `Error` is a terminal signal symmetric to a terminal
                    // `Stop`: the turn aborted, so this thread is no longer
                    // running. Pulled out of the catch-all rather than given a
                    // dedicated arm because the conversation still needs the
                    // generic `apply` below to render the error item.
                    if let ThreadEvent::Error(e) = ev {
                        let thread_id = this.thread.read(cx).id.0.clone();
                        // Sidebar running indicator: the turn aborted, so the
                        // row stops spinning, flags the error, and surfaces
                        // the unread state for the failed turn. A dead loop
                        // can no longer self-advance, so any background-work
                        // flag is stale and the row goes fully static.
                        let store = agent::thread_store_global();
                        store.update(cx, |s, cx| {
                            s.mark_idle(&thread_id, cx);
                            s.mark_background_work(&thread_id, false, cx);
                            s.mark_pending_plan(&thread_id, false, cx);
                            s.set_errored(&thread_id, true, cx);
                            s.set_unread(&thread_id, true, cx);
                        });
                        this.turn_active = false;
                        this.background_threads
                            .retain(|b| b.entity.read(cx).id.0 != thread_id);
                        // Persist the error card so a reloaded thread reproduces
                        // what went wrong, anchored to the failed turn.
                        this.record_ui_note(agent::db::UiNoteKind::Error, e.to_string(), cx);
                        // An error is a terminal state symmetric to a terminal
                        // `Stop`: the turn aborted, so any pending plan review
                        // is now stale and must not linger over an idle thread.
                        if this.pending_plan_review.take().is_some() {
                            this.conversation
                                .update(cx, |c, cx| c.consume_plan_review(cx));
                            // The demoted plan card stays in place but flips
                            // inactive — remeasure so the list's cached height
                            // for it stays honest.
                            this.list_state.remeasure();
                        }
                        // The run task emits `TurnFinished` after it has cleared
                        // `running_turn`; queue recovery and follow-up dispatch
                        // happen there.
                    }
                    // Cockpit phase tracking for the streaming/tool variants
                    // that flow through this generic arm. `Error` is handled
                    // above; `CompactionStarted` flips Summarizing, `Compaction`
                    // flips back to Streaming; a `Running` tool call caches its
                    // title and flips RunningTool; other tool statuses return to
                    // Streaming; text/thinking deltas mark Thinking/Streaming.
                    this.context_rail
                        .update(cx, |r, cx| r.update_cockpit_phase(ev, cx));
                    // Sub-agent observation: the pi harness observes its
                    // ephemeral nested sessions through progress events on
                    // the rail (the retired manox harness tracked child
                    // threads in observation panels instead).
                    if let ThreadEvent::SubagentProgress {
                        id,
                        subagent_type,
                        latest_activity,
                        status,
                        ..
                    } = ev
                    {
                        let id = id.clone();
                        let subagent_type = subagent_type.clone();
                        let latest_activity = latest_activity.clone();
                        this.context_rail.update(cx, |r, cx| {
                            r.apply_subagent_progress(
                                &id,
                                &subagent_type,
                                latest_activity.as_deref(),
                                *status,
                                cx,
                            );
                        });
                        if let Some(panel) = this.subagent_panels.get(&id) {
                            panel.update(cx, |p, cx| p.set_status(*status, cx));
                        }
                        // A finished run needs no backfill for panels opened
                        // later (they fall back to the final answer); drop the
                        // accumulated transcript so long threads do not hold
                        // every child delta until the thread switch.
                        if matches!(
                            *status,
                            agent::ToolCallStatus::Success
                                | agent::ToolCallStatus::Error
                                | agent::ToolCallStatus::Denied
                                | agent::ToolCallStatus::Cancelled
                        ) && !this.subagent_panels.contains_key(&id)
                        {
                            this.subagent_transcripts.remove(&id);
                        }
                    }
                    if let ThreadEvent::SubagentChild { id, child } = ev {
                        this.subagent_transcripts
                            .entry(id.clone())
                            .or_default()
                            .push(child.clone());
                        if let Some(panel) = this.subagent_panels.get(id) {
                            panel.update(cx, |p, cx| p.push(child, cx));
                        }
                    }
                    let weak = cx.weak_entity();
                    let role = this.model_label(cx);
                    let usage = this.thread.read(cx).last_request_token_usage();
                    let cwd = thread_cwd(&this.thread, cx);
                    let outcome = this.conversation.update(cx, |c, cx| {
                        c.apply(
                            ev,
                            &role,
                            usage,
                            crate::conversation::ApplyCtx { weak, cwd },
                            cx,
                        )
                    });
                    this.apply_list_outcome(outcome, cx);
                    cx.notify();
                }
            }
        })
    }

    /// Minimal subscription for a thread parked in `background_threads`. Unlike
    /// `subscribe_thread`, this only coordinates running state and the parked
    /// follow-up stash. It never touches `conversation` or `self.thread`, so a
    /// background thread's streaming deltas and tool events cannot be
    /// misrouted into the foreground view.
    /// The parked entry is left in `background_threads` (reclaimed on a later
    /// `open_thread`); self-removal from within the callback would drop the very
    /// subscription running it.
    fn subscribe_background_thread(
        &self,
        thread: Entity<ThreadEntity>,
        cx: &mut Context<Self>,
    ) -> Subscription {
        let id = thread.read(cx).id.0.clone();
        let parked_thread = thread.clone();
        cx.subscribe(
            &thread,
            move |this, _thread, ev: &ThreadEvent, cx| match ev {
                ThreadEvent::TurnStarted => {
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| s.mark_running(&id, cx));
                }
                ThreadEvent::ToolCallAuthorization { .. } => {
                    // A parked thread's approval card is not visible; the
                    // sidebar badge is the only signal until the user
                    // switches back and the card re-surfaces.
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| s.mark_pending_auth(&id, true, cx));
                }
                ThreadEvent::ToolCall { status, .. }
                    if !matches!(status, agent::thread::ToolCallStatus::PendingApproval) =>
                {
                    // Tool traffic past a parked authorization proves the run
                    // resumed (the verdict resolved); drop the badge. The
                    // `PendingApproval` card itself precedes the authorization
                    // event and must not clear it.
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| s.mark_pending_auth(&id, false, cx));
                }
                ThreadEvent::ToolResult { .. } | ThreadEvent::ToolOutput { .. } => {
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| s.mark_pending_auth(&id, false, cx));
                }
                ThreadEvent::SteerInjected { message_id } => {
                    this.consume_background_steer(&id, message_id);
                }
                ThreadEvent::PlanReady { .. } => {
                    // A parked thread's engine can re-emit PlanReady on
                    // restore; the sidebar keeps the blue-static wait visible
                    // until the verdict lands.
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| s.mark_pending_plan(&id, true, cx));
                }
                ThreadEvent::TurnFinished {
                    cancelled,
                    failed,
                    stranded_steer_ids,
                    ..
                } => {
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| {
                        s.mark_idle(&id, cx);
                        s.mark_pending_plan(&id, false, cx);
                        s.mark_pending_auth(&id, false, cx);
                        if !*failed {
                            s.set_errored(&id, false, cx);
                        }
                        s.set_unread(&id, true, cx);
                    });
                    // Mirror the foreground terminal bookkeeping: steers the
                    // aborted turn never drained flip to `Failed` in the
                    // parked stash (a late `SteerInjected` can still heal
                    // them), and queued follow-ups only become the next turn
                    // on a natural settle — never after a cancel.
                    this.mark_parked_stranded_steers_failed(&id, stranded_steer_ids);
                    if !*cancelled {
                        this.flush_parked_follow_ups(&id, cx);
                    }
                }
                ThreadEvent::Error(_) => {
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| {
                        s.mark_idle(&id, cx);
                        s.mark_background_work(&id, false, cx);
                        s.mark_pending_plan(&id, false, cx);
                        s.mark_pending_auth(&id, false, cx);
                        s.set_errored(&id, true, cx);
                        s.set_unread(&id, true, cx);
                    });
                }
                ThreadEvent::BackgroundTaskUpdated { .. } => {
                    save_thread(parked_thread.clone(), false, cx);
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| {
                        s.mark_background_work(
                            &id,
                            agent::background_task::thread_has_running_tasks(&id),
                            cx,
                        );
                        s.set_unread(&id, true, cx);
                    });
                }
                _ => {}
            },
        )
    }

    /// Flip the parked thread's `SteerPending` cards whose steers an aborted
    /// turn never drained to `Failed`, mirroring the foreground
    /// `mark_stranded_steers_failed` against the per-thread stash. A parked
    /// thread has no live conversation bubble to roll back (the optimistic
    /// bubble died with the conversation entity on switch-away), so only the
    /// queue state changes; the retry affordance renders once the stash is
    /// restored on switch-back.
    fn mark_parked_stranded_steers_failed(
        &mut self,
        thread_id: &str,
        stranded_steer_ids: &[String],
    ) {
        let stranded: std::collections::HashSet<&str> =
            stranded_steer_ids.iter().map(String::as_str).collect();
        if stranded.is_empty() {
            return;
        }
        let Some(queue) = self.queued_follow_ups_by_thread.get_mut(thread_id) else {
            return;
        };
        for item in queue.iter_mut() {
            if let FollowUpState::SteerPending { message_id } = &item.state
                && stranded.contains(message_id.as_str())
            {
                // Keep the id so a later `SteerInjected` can still heal this
                // provisional rollback (via `consume_background_steer`).
                let message_id = message_id.clone();
                item.state = FollowUpState::Failed { message_id };
            }
        }
    }

    /// Drain the stashed `Queued` follow-ups of a parked thread whose turn
    /// just settled: the follow-up turn runs on the parked thread itself,
    /// mirroring the foreground flush without a conversation bubble (a
    /// switch-back rebuild shows it through the thread mirror). `Queued`
    /// items coalesce into one `run_turn`, matching the foreground flush;
    /// `SteerPending`/`Failed` cards stay parked for the user.
    fn flush_parked_follow_ups(&mut self, thread_id: &str, cx: &mut Context<Self>) {
        let Some(queue) = self.queued_follow_ups_by_thread.get_mut(thread_id) else {
            return;
        };
        let Some(thread) = self
            .background_threads
            .iter()
            .find(|b| b.entity.read(cx).id.0 == thread_id)
            .map(|b| b.entity.clone())
        else {
            return;
        };
        let mut retain: std::collections::VecDeque<QueuedFollowUp> =
            std::collections::VecDeque::new();
        let mut flushed = false;
        while let Some(item) = queue.pop_front() {
            let is_queued = matches!(&item.state, FollowUpState::Queued);
            if is_queued {
                let mut content: Vec<agent::language_model::MessageContent> =
                    vec![agent::language_model::MessageContent::Text(item.turn.text)];
                content.extend(item.turn.images);
                let ui = item.turn.ui;
                thread.update(cx, |t, cx| {
                    t.insert_user_message_with_content_and_ui_metadata(content, Some(ui), cx);
                });
                flushed = true;
            } else {
                retain.push_back(item);
            }
        }
        if flushed {
            thread.update(cx, |t, cx| t.run_turn(cx));
        }
        if retain.is_empty() {
            self.queued_follow_ups_by_thread.remove(thread_id);
        } else {
            *queue = retain;
        }
    }

    fn subscribe_sidebar(&self, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        let sidebar = self.sidebar.clone();
        cx.subscribe_in(
            &sidebar,
            window,
            |this, _sidebar, ev: &SidebarEvent, window, cx| match ev {
                SidebarEvent::NewThread => this.start_new_thread(None, window, cx),
                SidebarEvent::NewThreadWithProject(dir) => {
                    this.start_new_thread(Some(dir.clone()), window, cx);
                }
                SidebarEvent::OpenThread(id) => this.open_thread(id.clone(), window, cx),
                SidebarEvent::SpawnExternalSession(kind, provider, model, project) => {
                    this.spawn_external_session(
                        *kind,
                        provider.clone(),
                        model.clone(),
                        project.clone(),
                        window,
                        cx,
                    );
                }
                SidebarEvent::SpawnPlainSession(kind, project) => {
                    this.spawn_plain_session(*kind, project.clone(), window, cx);
                }
                SidebarEvent::LaunchVSCode(project) => {
                    // VS Code opens the project directory the menu was launched
                    // from; from the Conversations header (no project) it
                    // falls back to the workspace cwd — the same directory a
                    // fresh session runs in. Injection targets come from the
                    // persisted `vscode_app:` settings (no launch-time choice).
                    let folder = project.clone().unwrap_or_else(|| this.cwd.clone());
                    this.launch_vscode_app(Some(folder), window, cx);
                }
                SidebarEvent::OpenExternalSession(id) => {
                    this.open_external_session(id, window, cx);
                }
                SidebarEvent::ArchiveExternalSession(id) => {
                    this.close_external_session(id, cx);
                }
                SidebarEvent::ArchiveThread(id, archived) => {
                    let is_current = this.thread.read(cx).id.0 == *id;
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| s.archive_thread(id, *archived, cx));
                    // Sync the in-memory flag so the title-bar menu label stays
                    // fresh when the sidebar archives the currently active thread.
                    if is_current {
                        this.thread
                            .update(cx, |t, cx| t.set_archived(*archived, cx));
                    }
                    // Archiving the active thread navigates away to a fresh
                    // empty thread (Hero view) so the user doesn't stare at a
                    // ghost conversation that just vanished from the sidebar.
                    if *archived && is_current {
                        this.start_new_thread(None, window, cx);
                    }
                }
            },
        )
    }

    /// New thread for the pi harness: no provider reload (registration is
    /// one-shot; actors await readiness themselves). A project binds at
    /// construction time — a single session creation, no orphaned session
    /// file.
    fn start_new_thread(
        &mut self,
        project: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = ThreadId(uuid::Uuid::new_v4().to_string());
        let new = match &project {
            Some(dir) => Thread::new_in_project(id, dir.clone(), cx),
            None => Thread::new_fresh(id, self.cwd.clone(), cx),
        };
        if let Some(dir) = &project {
            Self::register_project_in_store(dir, cx);
        }
        self.attach_thread(new, window, cx);
    }

    /// Switch into the Settings overlay. The Settings view is created lazily on
    /// first entry; from then on the entity + subscription are reused so the
    /// user's last selection (and any scroll position) survives re-entry.
    pub fn enter_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.settings_view.is_none() {
            let settings = cx.new(|cx| SettingsView::new(self.sidebar_width, window, cx));
            let sub = self.subscribe_settings(&settings, cx);
            self.settings_view = Some(settings);
            self.settings_sub = Some(sub);
        } else if let Some(settings) = self.settings_view.as_ref() {
            // Re-entry after a divider resize in the app page: the settings
            // nav follows the shared sidebar width.
            settings.update(cx, |s, cx| s.set_width(self.sidebar_width, cx));
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
            let pty = match terminal::pty::default_source(&self.cwd, 80, 24) {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(error = ?e, "failed to open terminal pty");
                    return;
                }
            };
            let terminal = match Terminal::new(id, self.cwd.clone(), 80, 24, pty, cx) {
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
    /// the `TerminalView` drops the underlying `Terminal`, whose `PtySource`
    /// kills the child and detaches the reader/waiter threads.
    pub fn close_terminal_tab(&mut self, cx: &mut Context<Self>) {
        self.terminal_view = None;
        self.focus_conversation(cx);
    }

    /// Launch a new external agent CLI session (`claude` / `codex` / `copilot`)
    /// with a user-picked provider + model (from the sidebar `+` wizard
    /// cascade). The shared `SessionHandle` backs a `CxSessionSource` PTY
    /// source that drives a `Terminal`/`TerminalView`, and is also held by the
    /// `ExternalSession` so the close path can `kill` the agent explicitly. A
    /// spawn failure (binary missing / apikey parse / unsupported combo) pushes
    /// an error notification and leaves the sidebar untouched.
    ///
    /// `project_cwd` is `Some(path)` when launched from a project folder's `+`
    /// button — the CLI runs in that project's directory. `None` uses the
    /// workspace's default cwd.
    pub fn spawn_external_session(
        &mut self,
        kind: SessionKind,
        provider_name: String,
        model_id: String,
        project_cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let agent_id = kind.agent_id();
        let Some(agent) = kind.agent() else {
            tracing::warn!(
                agent_id,
                "spawn_external_session called with a plain PTY kind"
            );
            return;
        };
        // The agent process runs in the project directory when spawned from a
        // project folder's `+` button, else the workspace cwd. cx's
        // AgentBuilder.cwd forwards to both the PTY-relay and direct spawn
        // paths; without it the agent inherits manox's own cwd (the user's
        // home dir), so a project-scoped session would operate on the wrong
        // tree.
        let cwd = project_cwd.clone().unwrap_or_else(|| self.cwd.clone());
        let handle = match cx::AgentBuilder::new()
            .agent(agent)
            .pty(true)
            .provider(provider_name.clone())
            .model(model_id.clone())
            .cwd(cwd.clone())
            .spawn()
        {
            Ok(h) => Arc::new(h),
            Err(e) => {
                tracing::error!(error = %e, agent = agent_id, "external agent spawn failed");
                window.push_notification(
                    Notification::error(format!(
                        "{}: {e}",
                        i18n::t("external-session-start-failed")
                    )),
                    cx,
                );
                return;
            }
        };
        let id = format!("external:{}:{}", agent_id, uuid::Uuid::new_v4());
        let source = terminal::cx_session::CxSessionSource::new(Arc::clone(&handle));
        let terminal = match Terminal::new(id.clone(), cwd.clone(), 80, 24, Box::new(source), cx) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, "failed to create terminal for external session");
                window.push_notification(
                    Notification::error(format!(
                        "{}: {e}",
                        i18n::t("external-session-start-failed")
                    )),
                    cx,
                );
                return;
            }
        };
        // Tear the session down when the CLI exits on its own (e.g. `/exit`),
        // without waiting for the user to click ×, and mirror the agent's OSC
        // title into the sidebar row + titlebar as it changes.
        let exit_sub = self.subscribe_session_terminal(&terminal, &id, cx);
        // The cx session id (and its socket path) are the traceable identity for
        // `~/.config/cx/sessions/<id>.sock`, surfaced in the sidebar tag +
        // clipboard copy. cx does not yet expose `SessionHandle::session_id()`,
        // so the id is recovered from the `<id>.sock` filename.
        let socket_path = handle.socket_path().map(std::path::Path::to_path_buf);
        let cx_session_id = handle
            .socket_path()
            .and_then(crate::external_session::cx_session_id_from_socket)
            .unwrap_or_default();
        let view = TerminalView::new(terminal, cx);
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // Persist the session as unclosed from the first frame: the sidecar
        // survives this process (crash or quit), is kept in sync while live,
        // and is deleted only on an explicit close — so the next launch offers
        // exactly the sessions the user never closed.
        let sidecar = ResumeSidecar {
            id: id.clone(),
            agent_id: agent_id.into(),
            cwd: cwd.to_string_lossy().into_owned(),
            project: project_cwd,
            created_at,
            provider: provider_name,
            model: model_id,
            title: None,
        };
        if let Err(e) = write_sidecar(&sidecar) {
            tracing::warn!(error = %e, id, "external session sidecar write failed");
        }
        self.external_sessions.push(ExternalSession {
            id: id.clone(),
            kind,
            created_at,
            project: sidecar.project.clone(),
            title: None,
            cx_session_id,
            socket_path,
            terminal_view: view,
            handle: Some(handle),
            _exit_sub: exit_sub,
            sidecar: Some(sidecar),
        });
        self.sync_sidebar_external(cx);
        self.attach_external_session(&id, window, cx);
    }

    /// Launch a plain PTY session — the user's shell (`Terminal`) — with no
    /// cx provider/model injection. Mirrors the `spawn_external_session` flow
    /// (sidebar row, ChildExit teardown, OSC title mirroring, project
    /// grouping), but the PTY is a local `PtyHandle` and
    /// `ExternalSession.handle` stays `None` — closing drops the view, whose
    /// PTY teardown kills the child tree.
    ///
    /// `project_cwd` is `Some(path)` when launched from a project folder's
    /// `+` button — the session runs in that project's directory. `None`
    /// uses the workspace's default cwd.
    pub fn spawn_plain_session(
        &mut self,
        kind: SessionKind,
        project_cwd: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = project_cwd.clone().unwrap_or_else(|| self.cwd.clone());
        let source: Box<dyn terminal::pty_source::PtySource> = match kind {
            SessionKind::Terminal => match terminal::pty::default_source(&cwd, 80, 24) {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(error = ?e, "failed to open terminal pty");
                    window.push_notification(
                        Notification::error(format!(
                            "{}: {e}",
                            i18n::t("plain-session-start-failed")
                        )),
                        cx,
                    );
                    return;
                }
            },
            _ => {
                tracing::warn!(
                    agent_id = kind.agent_id(),
                    "spawn_plain_session called with an agent kind"
                );
                return;
            }
        };
        let id = format!("external:{}:{}", kind.agent_id(), uuid::Uuid::new_v4());
        let terminal = match Terminal::new(id.clone(), cwd, 80, 24, source, cx) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = %e, "failed to create terminal for plain session");
                window.push_notification(
                    Notification::error(format!("{}: {e}", i18n::t("plain-session-start-failed"))),
                    cx,
                );
                return;
            }
        };
        let exit_sub = self.subscribe_session_terminal(&terminal, &id, cx);
        let view = TerminalView::new(terminal, cx);
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        self.external_sessions.push(ExternalSession {
            id: id.clone(),
            kind,
            created_at,
            project: project_cwd,
            title: None,
            cx_session_id: String::new(),
            socket_path: None,
            terminal_view: view,
            handle: None,
            _exit_sub: exit_sub,
            sidecar: None,
        });
        self.sync_sidebar_external(cx);
        self.attach_external_session(&id, window, cx);
    }

    /// Observe a session terminal's lifecycle, shared by the agent and plain
    /// spawn paths: `ChildExit` tears the session down on a natural exit
    /// (e.g. `/exit` or `exit`), and `Title` mirrors the TUI's OSC title into
    /// the sidebar row + titlebar as it changes. The subscription lives on
    /// the session so a later close detaches it before any spurious event.
    fn subscribe_session_terminal(
        &self,
        terminal: &Entity<Terminal>,
        id: &str,
        cx: &mut Context<Self>,
    ) -> Subscription {
        let exit_id = id.to_string();
        cx.subscribe(
            terminal,
            move |this, _terminal, ev: &terminal::event::TerminalEvent, cx| match ev {
                terminal::event::TerminalEvent::ChildExit(_) => {
                    this.remove_external_session(&exit_id, cx);
                }
                terminal::event::TerminalEvent::Title(title) => {
                    this.set_external_title(&exit_id, title.clone(), cx);
                }
                _ => {}
            },
        )
    }

    /// Launch ChatGPT.app through cx's injection path with the provider + model
    /// picked in the macOS `工具 → ChatGPT.app` menu cascade. The launch blocks
    /// (config load, model catalog build, CDP injection — up to ~20s), so it runs
    /// on a background thread and reports the outcome as a notification; the app
    /// itself detaches and keeps running independently of manox.
    pub fn launch_chatgpt_app(
        &mut self,
        provider: String,
        model: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let launch_provider = provider.clone();
            let launch_model = model.clone();
            let result = cx
                .background_spawn(
                    async move { cx::launch_chatgpt_app(&launch_provider, &launch_model) },
                )
                .await;
            let _ = this.update_in(cx, |_, window, cx| match result {
                Ok(()) => {
                    window.push_notification(
                        Notification::success(i18n::t_str(
                            "chatgpt-app-launched",
                            &[("provider", &provider), ("model", &model)],
                        )),
                        cx,
                    );
                }
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        provider = %provider,
                        model = %model,
                        "ChatGPT.app launch failed"
                    );
                    window.push_notification(
                        Notification::error(format!(
                            "{}: {e}",
                            i18n::t("chatgpt-app-launch-failed")
                        )),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// Launch VS Code with injections resolved from the persisted
    /// `vscode_app:` settings (Settings → 外部工具 → Visual Studio Code.app):
    /// Claude Code Extension block → ANTHROPIC_* env; Codex Extension block →
    /// CODEX_HOME + config.toml; both off → plain open. (工具 → VS Code menu
    /// entry and the sidebar new-session menu's VS Code item.) `folder` is
    /// `Some` when the launch should open a directory (the sidebar passes the
    /// project path or the workspace cwd); the 工具 menu passes `None` for a
    /// folder-less launch. Same background-spawn + notification shape as
    /// `launch_chatgpt_app`; the cx injection path may block on login-shell
    /// env resolution and — when VS Code is already running — on the restart
    /// confirmation + graceful quit wait.
    pub fn launch_vscode_app(
        &mut self,
        folder: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(
                    async move { cx::launch_vscode_app_from_settings(folder.as_deref()) },
                )
                .await;
            let _ = this.update_in(cx, |_, window, cx| match result {
                Ok(()) => {
                    window.push_notification(
                        Notification::success(i18n::t("vscode-app-launched")),
                        cx,
                    );
                }
                Err(e) => {
                    tracing::error!(error = %e, "VS Code launch failed");
                    window.push_notification(
                        Notification::error(format!(
                            "{}: {e}",
                            i18n::t("vscode-app-launch-failed")
                        )),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    /// Route a sidebar click on an external row. A live session attaches its
    /// running TUI; a resumable row (restored from a sidecar) re-spawns the
    /// CLI with its resume flag. Nothing is resumed at launch — only when the
    /// user picks the row, mirroring the manox thread contract.
    pub fn open_external_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.external_sessions.iter().any(|s| s.id == id) {
            self.attach_external_session(id, window, cx);
            return;
        }
        if self.resumable_external.iter().any(|s| s.id == id) {
            self.resume_external_session(id, window, cx);
        }
    }

    /// Re-spawn an unclosed external session's CLI with its resume flag so the
    /// CLI's own on-disk storage picks the conversation back up. The sidecar
    /// replays the original provider / model / cwd — no picker, and the resume
    /// command is never surfaced; the only feedback is a loading row until the
    /// TUI takes over. On failure the sidecar stays, so the row remains
    /// resumable for a retry.
    fn resume_external_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sidecar) = self.resumable_external.iter().find(|s| s.id == id).cloned() else {
            return;
        };
        let id = id.to_string();
        let Some(kind) = SessionKind::from_agent_id(&sidecar.agent_id) else {
            tracing::warn!(id, "resume sidecar carries a non-agent kind");
            return;
        };
        let Some(agent) = kind.agent() else {
            return;
        };
        // Guard and insert are adjacent on the UI thread, so a double-click
        // between them cannot double-spawn. Only ids that passed the
        // resolution above ever enter the set, so no early-return path can
        // strand a spinner.
        if !self.resuming_external.insert(id.clone()) {
            return;
        }
        self.sync_sidebar_external(cx);

        // The cx spawn (config load, keychain resolve, process spawn, socket
        // bind) is pure I/O, so it runs off the UI thread and the spinner
        // stays live while the CLI boots.
        let args = resume_args(&sidecar.agent_id);
        let provider = sidecar.provider.clone();
        let model = sidecar.model.clone();
        let cwd = PathBuf::from(&sidecar.cwd);
        let project = sidecar.project.clone();
        let created_at = sidecar.created_at;
        let this = cx.weak_entity();
        cx.spawn_in(window, async move |_window, cx| {
            let handle = match cx
                .background_spawn(async move {
                    cx::AgentBuilder::new()
                        .agent(agent)
                        .pty(true)
                        .provider(provider)
                        .model(model)
                        .cwd(cwd)
                        .passthrough(args)
                        .spawn()
                })
                .await
            {
                Ok(h) => Arc::new(h),
                Err(e) => {
                    tracing::error!(error = %e, id, "external session resume failed");
                    let _ = this.update_in(cx, |this, window, cx| {
                        this.resuming_external.remove(&id);
                        this.sync_sidebar_external(cx);
                        window.push_notification(Notification::error(format!(
                            "{}: {e}",
                            i18n::t("external-session-resume-failed")
                        )), cx);
                    });
                    return;
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                let source = terminal::cx_session::CxSessionSource::new(Arc::clone(&handle));
                let terminal =
                    match terminal::Terminal::new(id.clone(), PathBuf::from(&sidecar.cwd), 80, 24, Box::new(source), cx)
                    {
                        Ok(t) => t,
                        Err(e) => {
                            tracing::error!(error = %e, id, "failed to create resumed session terminal");
                            this.resuming_external.remove(&id);
                            this.sync_sidebar_external(cx);
                            window.push_notification(Notification::error(format!(
                                "{}: {e}",
                                i18n::t("external-session-resume-failed")
                            )), cx);
                            return;
                        }
                    };
                let exit_sub = this.subscribe_session_terminal(&terminal, &id, cx);
                let socket_path = handle.socket_path().map(std::path::Path::to_path_buf);
                let cx_session_id = handle
                    .socket_path()
                    .and_then(crate::external_session::cx_session_id_from_socket)
                    .unwrap_or_default();
                let view = TerminalView::new(terminal, cx);
                this.external_sessions.push(ExternalSession {
                    id: id.clone(),
                    kind,
                    created_at,
                    project,
                    title: None,
                    cx_session_id,
                    socket_path,
                    terminal_view: view,
                    handle: Some(handle),
                    _exit_sub: exit_sub,
                    // The resumed session keeps its sidecar (same identity):
                    // the disk record already represents it, and title sync
                    // keeps the row fresh for a possible later exit.
                    sidecar: Some(sidecar),
                });
                this.resuming_external.remove(&id);
                this.sync_sidebar_external(cx);
                this.attach_external_session(&id, window, cx);
            });
        })
        .detach();
    }

    /// Display an already-running external session in the main area. Does not
    /// touch the foreground `Thread` (the thread entity stays mounted; only the
    /// view mode flips) — the session's terminal keeps running across switches
    /// because the `ExternalSession` owns the live `TerminalView` + handle.
    /// Focuses the terminal view on the next frame so the user can type
    /// immediately without clicking into the TUI.
    pub fn attach_external_session(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = self
            .external_sessions
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.terminal_view.clone());
        if view.is_none() {
            return;
        }
        self.active_external = Some(id.to_string());
        self.view_mode = ViewMode::ExternalSession;
        self.sidebar
            .update(cx, |s, cx| s.set_selected(Some(id.to_string()), cx));
        cx.notify();
        if let Some(view) = view {
            self.focus_external_view(view, window, cx);
        }
    }

    /// Focus an external session's terminal view on the next frame. Deferred so
    /// the `TerminalView` element is mounted (the view-mode flip schedules a
    /// re-render) before the focus is set — GPUI can't focus an element that
    /// hasn't rendered its `track_focus` yet.
    fn focus_external_view(
        &self,
        view: Entity<terminal_ui::TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.defer(cx, move |window, cx| {
            let handle = view.read(cx).focus_handle();
            window.focus(&handle, cx);
        });
    }

    /// Mirror an external agent's OSC title (`TerminalEvent::Title`) into its
    /// `ExternalSession`, push a fresh projection to the sidebar, and refresh
    /// the titlebar when the active session's title changed. No-op when the
    /// session was already removed (a spurious title after close).
    fn set_external_title(&mut self, id: &str, title: Option<String>, cx: &mut Context<Self>) {
        let active = self.active_external.as_deref() == Some(id);
        let new = title.as_deref().filter(|t| !t.trim().is_empty());
        let changed = self.external_sessions.iter_mut().any(|s| {
            if s.id != id {
                return false;
            }
            if s.title.as_deref() == new {
                return false;
            }
            s.title = new.map(str::to_string);
            true
        });
        if changed {
            // Keep the durable sidecar's title in sync so a later exit offers
            // the resumable row under the latest OSC title rather than the one
            // captured at spawn / last resume.
            if let Some(sidecar) = self
                .external_sessions
                .iter_mut()
                .find(|s| s.id == id)
                .and_then(|s| s.sidecar.as_mut())
            {
                sidecar.title = new.map(str::to_string);
                if let Err(e) = write_sidecar(sidecar) {
                    tracing::warn!(error = %e, id, "external session sidecar title update failed");
                }
            }
            self.sync_sidebar_external(cx);
            if active {
                cx.notify();
            }
        }
    }

    /// Kill an external session and remove it (the sidebar `×` path). The
    /// explicit `handle.kill()` unblocks the reader thread even mid-`read`, so
    /// the terminal drains instead of hanging on a dead PTY; `kill` on an
    /// already-dead child is best-effort (warn-logged). Removal itself —
    /// including sidebar sync + fallback-to-conversation — is shared with the
    /// natural-exit path in [`remove_external_session`].
    pub fn close_external_session(&mut self, id: &str, cx: &mut Context<Self>) {
        let kill_handle = self
            .external_sessions
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.handle.as_ref().map(Arc::clone));
        if let Some(handle) = kill_handle
            && let Err(e) = handle.kill()
        {
            tracing::warn!(error = %e, id, "external session kill failed");
        }
        // Plain PTY sessions (no cx handle) tear down on drop: the dropped
        // TerminalView's PtyHandle kills the child tree in its own Drop.
        self.remove_external_session(id, cx);
    }

    /// Remove an external session without killing the child — the natural-exit
    /// path (the `ChildExit` subscription fired because the CLI already exited).
    /// Dropping the `ExternalSession` drops its `TerminalView` (and thus the
    /// `Terminal` + `CxSessionSource`); the last `Arc<SessionHandle>` ref then
    /// drops, and cx's `SessionHandle::Drop` does best-effort reap + socket
    /// cleanup. If the removed session was the active one, fall back to the
    /// conversation pane.
    fn remove_external_session(&mut self, id: &str, cx: &mut Context<Self>) {
        let was_active = self.active_external.as_deref() == Some(id);
        self.external_sessions.retain(|s| s.id != id);
        // The session is now explicitly closed (sidebar `×` or a natural CLI
        // exit): drop its sidecar — both the in-memory row and the disk record
        // — so it is no longer offered for resume.
        self.resumable_external.retain(|s| s.id != id);
        remove_sidecar(id);
        if was_active {
            self.active_external = None;
            self.focus_conversation(cx);
        }
        self.sync_sidebar_external(cx);
    }

    /// Push a fresh projection of the external sessions to the sidebar so its
    /// list reflects spawns / closes / resumes without owning the
    /// PTY-bearing structs. Live sessions render first, then the resumable
    /// rows restored from sidecars.
    fn sync_sidebar_external(&mut self, cx: &mut Context<Self>) {
        let live: Vec<_> = self.external_sessions.iter().map(|s| s.summary()).collect();
        let mut summaries = merge_external_summaries(live, self.resumable_external.clone());
        let resuming = self.resuming_external.clone();
        for s in &mut summaries {
            s.resuming = resuming.contains(s.id.as_str());
        }
        self.sidebar
            .update(cx, |s, cx| s.set_external_sessions(summaries, cx));
    }

    /// Open a browser tab navigating to `url` (defaulting to
    /// [`crate::views::browser_view::DEFAULT_URL`] when empty) and focus it.
    /// Returns the allocated `BrowserTabId` so callers (the host, tool
    /// surface) can drive the tab afterwards. The webview is built untrusted:
    /// no Tauri command surface, only the notify/inbound bridges.
    pub fn open_browser_tab(
        &mut self,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> BrowserTabId {
        let url = if url.is_empty() {
            crate::views::browser_view::DEFAULT_URL
        } else {
            url
        };
        let tab_id = crate::views::browser_view::allocate_tab_id();
        let view =
            cx.new(|cx| crate::views::browser_view::BrowserView::new(tab_id, url, window, cx));
        self.browser_views.insert(tab_id, view);
        self.right_tabs.push(RightTab::Browser(tab_id));
        self.set_active_right_tab(self.right_tabs.len() - 1, cx);
        tab_id
    }

    /// Close and recycle a browser tab by id. Dropping the `BrowserView`
    /// drops the underlying native webview, whose `Drop` hides and detaches
    /// the platform view. No-op if the id is not live.
    pub fn close_browser_tab(&mut self, tab_id: BrowserTabId, cx: &mut Context<Self>) {
        if self.browser_views.remove(&tab_id).is_none() {
            return;
        }
        // Reclaim the host's routing entry so a late notify for this tab finds
        // no route (no orphaned oneshot). `close_tab` reclaims first then calls
        // us — in that direction `reclaim_routes` is a no-op; this call covers
        // the UI-close direction.
        if let Some(host) = crate::browser_host::WorkspaceBrowserHost::concrete() {
            host.reclaim_routes(tab_id);
        }
        if let Some(ix) = self
            .right_tabs
            .iter()
            .position(|t| matches!(t, RightTab::Browser(id) if *id == tab_id))
        {
            self.right_tabs.remove(ix);
            self.reseat_active_after_close(ix);
            self.editor_open = self
                .right_tabs
                .get(self.active_right_tab)
                .is_some_and(|t| matches!(t, RightTab::Editor));
        }
        cx.notify();
    }

    fn subscribe_input(&self, window: &mut Window, cx: &mut Context<Self>) -> Subscription {
        let input = self.input_state.clone();
        cx.subscribe_in(
            &input,
            window,
            |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { shift: false, .. } => this.submit_input(window, cx),
                // Shift+Enter inserts a newline inside the input and does not submit.
                InputEvent::PressEnter { shift: true, .. } => {}
                InputEvent::Change => this.sync_completion(window, cx),
                InputEvent::Focus | InputEvent::Blur => {}
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

    /// Re-evaluate the completion popover against the live input value + caret.
    ///
    /// When the caret sits inside a `/` or `@` trigger token, the matching
    /// source is filtered by the query and a fresh [`CompletionState`] replaces
    /// the current one. With no trigger or zero matches the popover closes. The
    /// popover is a pure render overlay and never grabs focus, so the
    /// `InputState` keeps typing and the filter updates every keystroke.
    fn sync_completion(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let (value, cursor) = {
            let s = self.input_state.read(cx);
            (s.value().to_string(), s.selected_range().end)
        };
        let new = match detect(&value, cursor) {
            None => None,
            Some(det) => {
                let items = if det.trigger == '/' {
                    slash_source(&det.query)
                } else {
                    mention_source(&det.query)
                };
                if items.is_empty() {
                    None
                } else {
                    // Carry the selection forward when the same trigger is
                    // active and the previously-picked item survived the
                    // narrower filter, so typing more to refine doesn't snap
                    // the highlight back to the top.
                    let selected = self
                        .completion
                        .as_ref()
                        .filter(|s| s.trigger == det.trigger)
                        .and_then(|s| s.items.get(s.selected).map(|it| it.name.clone()))
                        .and_then(|name| items.iter().position(|it| it.name == name))
                        .unwrap_or(0);
                    Some(CompletionState::new(
                        det.trigger,
                        det.token_start,
                        items,
                        selected,
                    ))
                }
            }
        };
        let changed = match (&self.completion, &new) {
            (None, None) => false,
            (Some(_), None) | (None, Some(_)) => true,
            (Some(a), Some(b)) => {
                !a.items.eq(&b.items) || a.trigger != b.trigger || a.selected != b.selected
            }
        };
        self.completion = new;
        if changed {
            cx.notify();
        }
    }

    /// Drop the popover without touching the input.
    fn close_completion(&mut self, cx: &mut Context<Self>) {
        if self.completion.take().is_some() {
            cx.notify();
        }
    }

    /// Confirm the selected (or clicked) completion item: replace the trigger
    /// token with `trigger + name + " "` and place the caret after the space.
    fn completion_confirm(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(state) = self.completion.take() else {
            return;
        };
        let Some(item) = state.items.get(ix) else {
            self.completion = Some(state);
            return;
        };
        let name = item.name.to_string();
        let trigger = state.trigger;
        let token_start = state.token_start;
        let (value, cursor) = {
            let s = self.input_state.read(cx);
            (s.value().to_string(), s.selected_range().end)
        };
        if cursor > value.len() || token_start > cursor {
            return;
        }
        let (new_value, caret) = build_replacement(trigger, &name, &value, token_start, cursor);
        self.input_state.update(cx, |s, cx| {
            s.set_value(new_value, window, cx);
            let pos = RopeExt::offset_to_position(s.text(), caret.min(s.text().len()));
            s.set_cursor_position(pos, window, cx);
        });
        cx.notify();
    }

    fn completion_up(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.completion.as_mut() {
            state.move_selection(-1);
            cx.notify();
        }
    }

    fn completion_down(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(state) = self.completion.as_mut() {
            state.move_selection(1);
            cx.notify();
        }
    }

    fn completion_confirm_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.completion.as_ref().map(|s| s.selected).unwrap_or(0);
        self.completion_confirm(ix, window, cx);
    }

    /// Close the access-chip dropdown, dropping the menu entity + subscription.
    fn close_access_menu(&mut self) {
        self.access_open = false;
    }

    /// Close the project-chip dropdown.
    fn close_project_chip_menu(&mut self) {
        self.project_chip_open = false;
        self.project_chip_menu = None;
        self.project_chip_menu_sub = None;
    }

    fn blocking_overlay_active(&self) -> bool {
        self.pending_plan_review.is_some()
            || self.pending_ask.is_some()
            || self.blank_project_parent.is_some()
    }

    fn toggle_turn_navigator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.turn_navigator.is_some() {
            self.close_turn_navigator(window, cx);
            return;
        }
        if !matches!(self.view_mode, ViewMode::Workspace) || self.blocking_overlay_active() {
            return;
        }

        let turns = collect_user_turns(
            self.conversation
                .read(cx)
                .items()
                .iter()
                .enumerate()
                .map(|(ix, item)| (ix, item.read(cx).kind())),
        );
        let previous_focus = window.focused(cx);
        let navigator = cx.new(|cx| TurnNavigator::new(turns, window, cx));
        let sub = cx.subscribe_in(
            &navigator,
            window,
            |this, _navigator, event: &TurnNavigatorEvent, window, cx| match event {
                TurnNavigatorEvent::Navigate { item_ix } => {
                    let target = *item_ix;
                    this.close_turn_navigator(window, cx);
                    this.reveal_message(target, window, cx);
                }
                TurnNavigatorEvent::Dismiss => this.close_turn_navigator(window, cx),
            },
        );
        self.turn_navigator = Some(navigator.clone());
        self.turn_navigator_sub = Some(sub);
        self.turn_navigator_previous_focus = previous_focus;
        navigator.update(cx, |navigator, cx| navigator.focus(window, cx));
        cx.notify();
    }

    /// Reconcile the `list_state` item count with the live conversation length
    /// via `splice`, which preserves scroll position. Append (the common case,
    /// every user/assistant/tool item) splices new tail items in as Unmeasured;
    /// a tail removal (a `Retry` badge popped without replacement) splices the
    /// dangling slot out. Call after any direct conversation mutation that the
    /// `ApplyOutcome` path does not already cover (e.g. `push_user`/`push_notice`,
    /// which bypass `apply`).
    fn sync_list_count(&mut self, cx: &App) -> bool {
        let new_count = self.conversation.read(cx).items().len();
        if new_count == self.list_count {
            return false;
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
        true
    }

    /// Reconcile the `list_state` with a conversation mutation: splice the
    /// count (append/remove) and remeasure the affected index/indices. Call
    /// after any `ConversationState::apply` (the outcome tells which path) so
    /// the virtualized list's per-item height cache never goes stale.
    fn apply_list_outcome(&mut self, outcome: ApplyOutcome, cx: &App) {
        let count_changed = self.sync_list_count(cx);
        match outcome {
            ApplyOutcome::Remeasure(ix) => self.list_state.remeasure_items(ix..ix + 1),
            ApplyOutcome::RemeasureAll => self.list_state.remeasure(),
            // Remeasure the just-mutated segment (e.g. an activity segment
            // closed for an incoming reply) in addition to the append splice
            // `sync_list_count` already performed. When the append was net-
            // neutralized by a trailing `Retry` pop (count unchanged → no
            // splice), the new assistant bubble occupies a reused `Measured`
            // tail slot whose cached height is the popped retry badge's, so
            // remeasure the tail too. (When `popped_retry` was false the push
            // grew the count by one, `count_changed` is true, and the splice
            // already inserted the new bubble as `Unmeasured` — so the tail
            // remeasure is skipped as redundant, not because the branch is
            // dead.)
            ApplyOutcome::RemeasureAndAppend { remeasure_ix } => {
                self.list_state
                    .remeasure_items(remeasure_ix..remeasure_ix + 1);
                if !count_changed {
                    let tail = self.list_count.saturating_sub(1);
                    self.list_state.remeasure_items(tail..tail + 1);
                }
            }
            // `Unchanged` touched no item; `Appended`/`RemovedTail` only changed
            // the count, which `sync_list_count` already spliced.
            ApplyOutcome::Unchanged | ApplyOutcome::Appended | ApplyOutcome::RemovedTail => {}
        }
    }

    /// Re-engage tail-follow. `FollowMode::Tail` pins to the end natively and
    /// keeps following; an upward user scroll disengages it and landing back
    /// at the bottom re-arms it. This is the user-initiated "jump to live
    /// tail" path (submit, slash command, thread open) — it always re-arms
    /// follow, even if the user had scrolled up to read back.
    fn follow_message_tail(&mut self) {
        self.list_state.set_follow_mode(FollowMode::Tail);
    }

    /// Jump the viewport so the given conversation item is at the top. Native
    /// `scroll_to` is a single atomic state change, so no frame protection is
    /// needed against a stale tail re-pin.
    fn reveal_message(&mut self, item_ix: usize, _window: &mut Window, cx: &mut Context<Self>) {
        self.list_state.set_follow_mode(FollowMode::Normal);
        self.list_state.scroll_to(ListOffset {
            item_ix,
            offset_in_item: px(0.),
        });
        cx.notify();
    }

    fn close_turn_navigator(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.turn_navigator.take().is_none() {
            return;
        }
        self.turn_navigator_sub = None;
        if let Some(previous) = self.turn_navigator_previous_focus.take() {
            window.focus(&previous, cx);
        }
        cx.notify();
    }

    fn drop_turn_navigator(&mut self, cx: &mut Context<Self>) {
        if self.turn_navigator.take().is_some() {
            self.turn_navigator_sub = None;
            self.turn_navigator_previous_focus = None;
            cx.notify();
        }
    }

    fn render_turn_navigator_overlay(
        &self,
        window: &mut Window,
        theme: &Theme,
        right_pane_open: bool,
        show_context_rail: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let navigator = self.turn_navigator.clone()?;
        let layout = turn_navigator_layout(
            window.bounds().size.width,
            self.sidebar_width,
            right_pane_open.then_some(self.editor_width),
            show_context_rail,
        );
        let panel_height = navigator.read(cx).panel_height(cx);

        Some(
            v_flex()
                .id("turn-navigator-overlay")
                .absolute()
                .top_0()
                .right_0()
                .bottom_0()
                .left_0()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        this.close_turn_navigator(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(
                    v_flex()
                        .absolute()
                        .top_0()
                        .right(layout.right_inset)
                        .bottom_0()
                        .left(layout.left_inset)
                        .items_center()
                        .pt(TITLE_BAR_HEIGHT + px(8.0))
                        .child(
                            popup_menu::popup_container(theme, navigator)
                                .id("turn-navigator-panel")
                                .w(layout.panel_width)
                                .h(panel_height)
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation()),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Switch to a new thread: persist the current one, build/load the new one, re-subscribe, and rebuild the conversation view.
    fn attach_thread(
        &mut self,
        new_thread: Entity<ThreadEntity>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_turn_navigator(window, cx);
        let old_thread = self.thread.clone();
        let old_id = old_thread.read(cx).id.0.clone();
        let new_id = new_thread.read(cx).id.0.clone();

        // Sub-agent observation is per-thread ephemeral state; drop the
        // outgoing thread's panels and transcripts before rebinding.
        self.clear_subagent_observation();

        // Save the outgoing thread's unsent composer text before switching, so
        // a draft survives a round-trip through another thread (Bug 1). A
        // thread that just submitted already cleared its input, storing "".
        self.drafts.insert(
            old_id.clone(),
            self.input_state.read(cx).value().to_string(),
        );
        // The editor pane is a right-side resource of the outgoing thread:
        // stash its text so a switch-back restores the draft (mirrors the
        // composer `drafts` stash above).
        self.editor_drafts.insert(
            old_id.clone(),
            self.editor_state.read(cx).value().to_string(),
        );

        // Stash the outgoing thread's pending plan verdict so it survives a
        // round-trip through another thread. The plan text lives only in
        // `pending_plan_review` (never in persisted messages), so switching
        // away would otherwise drop it — and the verdict card with it.
        if let Some(review) = self.pending_plan_review.take() {
            self.pending_plans.insert(old_id.clone(), review);
        }

        // Queue state is session-local but belongs to a thread, not to the
        // currently visible workspace. Move it aside before rebinding.
        let outgoing_follow_ups = std::mem::take(&mut self.queued_follow_ups);
        if outgoing_follow_ups.is_empty() {
            self.queued_follow_ups_by_thread.remove(&old_id);
        } else {
            self.queued_follow_ups_by_thread
                .insert(old_id.clone(), outgoing_follow_ups);
        }
        // Restore the incoming thread's stashed follow-up queue (moved aside
        // when it was last switched away); without this the queue silently
        // vanishes on switch-back.
        if let Some(queue) = self.queued_follow_ups_by_thread.remove(&new_id) {
            self.queued_follow_ups = queue;
        }

        // If the old thread is still running a turn, park it in the background
        // so its `run_turn_loop` task stays alive (the entity is otherwise only
        // held by `self.thread`; overwriting that field would drop it and
        // silently kill the turn via `WeakEntity::upgrade() -> None`).
        if (old_thread.read(cx).is_running()
            || agent::background_task::thread_has_running_tasks(&old_id))
            && old_id != new_id
        {
            // Seed the live-state sets for whatever is still in flight: the
            // background subscription below reacts only to future events, so a
            // turn that started (or a monitor / task that spawned) while this
            // thread was foreground would otherwise never reach the sidebar's
            // running indicator. A task-only thread (no turn) gets the
            // background-work seed, not the running one.
            if old_thread.read(cx).is_running() {
                let store = agent::thread_store_global();
                store.update(cx, |s, cx| s.mark_running(&old_id, cx));
            }
            if agent::background_task::thread_has_running_tasks(&old_id) {
                let store = agent::thread_store_global();
                store.update(cx, |s, cx| s.mark_background_work(&old_id, true, cx));
            }
            let sub = self.subscribe_background_thread(old_thread.clone(), cx);
            self.background_threads.push(BackgroundThread {
                entity: old_thread,
                _sub: sub,
            });
        }

        // If the new thread was previously parked in the background, reclaim it
        // so it becomes the foreground thread and is no longer double-held.
        self.background_threads
            .retain(|b| b.entity.read(cx).id.0 != new_id);

        // Persist the old thread's current state before switching away. The
        // spawned-task save backstop in `run_turn` will persist again when the
        // turn actually finishes, capturing the final assistant messages.
        save_thread(self.thread.clone(), false, cx);

        self.thread = new_thread;
        let id = self.thread.read(cx).id.0.clone();
        let messages: Vec<agent::Message> = self.thread.read(cx).messages().to_vec();
        let usage = self.thread.read(cx).request_token_usage().clone();
        let notes = self.thread.read(cx).ui_notes().to_vec();
        let background_tasks = self.thread.read(cx).background_task_snapshots();
        let role = self.model_label(cx);
        let weak = cx.weak_entity();
        let running = self.thread.read(cx).is_running();
        let cwd = thread_cwd(&self.thread, cx);
        let new_conv = cx.new(|cx| {
            let mut conversation = ConversationState::rebuild_from_messages(
                &messages,
                &usage,
                &role,
                running,
                &notes,
                crate::conversation::ApplyCtx {
                    weak: weak.clone(),
                    cwd,
                },
                cx,
            );
            conversation.restore_background_tasks(&background_tasks, &role, weak.clone(), cx);
            conversation
        });
        self.conversation = new_conv;
        // Restore the incoming thread's saved draft, or clear the input if it
        // has none — without this the previous thread's text would bleed into
        // the new one (Bug 1). `set_value` is silent (no Change event), so
        // re-sync the slash menu by hand in case the draft begins with `/`.
        let saved = self.drafts.remove(&new_id).unwrap_or_default();
        self.input_state
            .update(cx, |s, cx| s.set_value(saved, window, cx));
        self.sync_completion(window, cx);
        // Restore the incoming thread's stashed editor draft, or clear the
        // pane so the previous thread's text never bleeds into this one.
        // `set_value` is silent (no Change event), so the editor's submit
        // binding is unaffected.
        let editor_saved = self.editor_drafts.remove(&new_id).unwrap_or_default();
        self.editor_state
            .update(cx, |s, cx| s.set_value(editor_saved, window, cx));
        // Reveal the latest turn for the new thread: `reset` drops the old
        // thread's measured heights and scroll position, then reveal the latest
        // turn once. Both running and completed threads arm `FollowMode::Tail`:
        // the list pins to the end on each layout while following, and GPUI
        // auto-disengages follow the moment the user scrolls up. Using
        // `Normal` here would leave a one-shot scroll that never re-pins if a
        // late delta or notice lands after the user scrolls.
        let count = self.conversation.read(cx).items().len();
        self.list_state.reset(count);
        self.list_count = count;
        // All of the thread's current messages are rendered (a restoring
        // thread has none yet — the preview batches append from here).
        self.history_rendered = messages.len();
        self.list_state.set_follow_mode(FollowMode::Tail);
        self.pending_ask = None;
        // Restore the incoming thread's stashed pending plan, if any.
        self.pending_plan_review = self.pending_plans.remove(&new_id);
        if let Some(review) = self.pending_plan_review.as_ref() {
            let title = review.title.clone();
            let content = review.content.clone();
            let role = self.model_label(cx);
            let weak = cx.weak_entity();
            self.conversation.update(cx, |c, cx| {
                c.push_plan_review(title, content, role, weak, cx);
            });
            // The card is appended after the earlier `reset`, so splice the
            // list count and re-pin so the restored drawer is in view.
            self.sync_list_count(cx);
            self.list_state.set_follow_mode(FollowMode::Tail);
        }
        self.thread_sub = Some(self.subscribe_thread(cx));
        // The thinking ticker belongs to the outgoing thread: bump its
        // generation so the old ticker self-terminates, then mirror the incoming
        // thread's running state. A parked thread resumed mid-turn keeps the
        // "for Xs" counter live; a completed history thread is idle.
        self.turn_active = running;
        if running {
            self.spawn_thinking_ticker(cx);
        }
        // Cockpit state is per-thread: the outgoing thread's plan,
        // running-tool title, and per-model counter state do not apply to the
        // incoming one. The execution plan, unlike the proposed-plan review,
        // IS recoverable: re-derived from the transcript's `UpdatePlan` tool
        // calls, falling back to the independent sidecar snapshot (the facade
        // mirrors the persisted copy on every `PlanUpdated` / `Ready`).
        let new_thread_for_rail = self.thread.clone();
        let restored_plan = agent::plan::rebuild_from_messages(&messages)
            .or_else(|| self.thread.read(cx).persisted_plan().cloned());
        self.context_rail.update(cx, |r, cx| {
            // Rebind the rail to the incoming thread. Without this the rail
            // keeps reading the construction-time thread's `per_model` usage /
            // project / display_title, so a freshly loaded thread with real
            // usage data renders an empty "消费" section (no per-model tree).
            r.thread = new_thread_for_rail;
            r.reset_for_thread_switch(running, cx);
            if let Some(snapshot) = restored_plan {
                r.set_plan(snapshot, cx);
            }
            cx.notify();
        });
        // If the new thread has pending authorizations (e.g. it was parked
        // while waiting for tool approval), re-surface them so the overlay
        // appears immediately upon switching back.
        self.resurface_pending_auths(cx);
        self.sidebar
            .update(cx, |s, cx| s.set_selected(Some(id.clone()), cx));
        // The user is now viewing this thread: clear any unread red dot it
        // carried from a prior background completion, and any pending-auth
        // badge (the verdict card re-surfaced below if one is outstanding).
        let store = agent::thread_store_global();
        store.update(cx, |s, cx| {
            s.set_unread(&id, false, cx);
            s.mark_pending_auth(&id, false, cx);
        });
        // The incoming thread's cwd / worktree may differ from the outgoing
        // one; refresh the rail's git stats/branch display for it.
        self.spawn_git_status_refresh(cx);
        // Returning to a thread leaves the external-session view: without this
        // the render still takes the ExternalSession branch and the swapped-in
        // thread is invisible behind the terminal TUI.
        self.view_mode = ViewMode::Workspace;
        self.active_external = None;
        cx.notify();
    }

    /// Start (or restart) the per-second ticker that drives the Thinking status
    /// row's "for Xs" counter. Bumping `thinking_ticker_gen` first invalidates
    /// any prior ticker — it polls the generation and self-terminates when it
    /// no longer matches, so a new turn or thread switch replaces the old task
    /// instead of stacking a second one.
    fn spawn_thinking_ticker(&mut self, cx: &mut Context<Self>) {
        self.thinking_ticker_gen = self.thinking_ticker_gen.wrapping_add(1);
        let entity = cx.entity().clone();
        let ticker_gen = self.thinking_ticker_gen;
        cx.spawn(async move |_this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(1))
                    .await;
                let still = entity.read_with(cx, |this, _cx| {
                    this.thinking_ticker_gen == ticker_gen && this.turn_active
                });
                if !still {
                    break;
                }
                entity.update(cx, |_, cx| cx.notify());
            }
        })
        .detach();
    }

    /// Debounced git-status refresh. Bumps `git_status_gen` (invalidating any
    /// prior in-flight refresh), waits 400ms so a burst of tool results
    /// coalesces into one git call, then shells out to `git diff --numstat`
    /// / `branch --show-current` on the global tokio runtime. The result is
    /// delivered back to the gpui side via `async_channel` and pushed onto the
    /// `ContextRail`. Cancelled (superseded) refreshes self-terminate by
    /// comparing their captured gen to the live one.
    ///
    /// Uses `cx.background_executor().timer()` — never `tokio::time` on the
    /// gpui foreground (that panics: no current tokio runtime).
    fn spawn_git_status_refresh(&mut self, cx: &mut Context<Self>) {
        self.git_status_gen = self.git_status_gen.wrapping_add(1);
        let entity = cx.entity().clone();
        let refresh_gen = self.git_status_gen;
        let rail = self.context_rail.clone();
        let cwd = self.thread.read(cx).cwd().to_path_buf();
        let worktree_branch = self.thread.read(cx).worktree().map(|w| w.branch.clone());
        cx.spawn(async move |_this, cx| {
            // Debounce: coalesce a burst of tool results / a turn's worth of
            // file writes into a single git call.
            cx.background_executor()
                .timer(std::time::Duration::from_millis(400))
                .await;
            // Superseded by a newer trigger — let the newer refresh win.
            let stale = entity.read_with(cx, |this, _| this.git_status_gen != refresh_gen);
            if stale {
                return;
            }
            let result = crate::git_status::gather_bridged(cwd, worktree_branch).await;
            // The refresh may have been superseded while the git call was in
            // flight; drop the result if so.
            let still_current = entity.read_with(cx, |this, _| this.git_status_gen == refresh_gen);
            if !still_current {
                return;
            }
            let (stats, display) = match result {
                Some(v) => (Some(v.0), Some(v.1)),
                None => (None, None),
            };
            rail.update(cx, |r, cx| r.set_git_status(stats, display, cx));
        })
        .detach();
    }

    /// Re-surface any pending authorization on the current thread that was
    /// emitted while the thread was in the background (no subscription). Called
    /// after switching threads so the question card appears without requiring
    /// the user to wait for the next event.
    fn resurface_pending_auths(&mut self, cx: &mut Context<Self>) {
        // Query the thread for any pending authorization metadata that was
        // stored when the auth event was originally emitted. If the thread was
        // parked waiting for user approval while in the background, re-surface
        // the events so the question card appears immediately upon switching
        // back. Every authorization surfaces as the AskUserQuestion card.
        let entries: Vec<(String, String, serde_json::Value)> = self
            .thread
            .read(cx)
            .pending_auth_entries()
            .into_iter()
            .map(|(id, meta)| (id, meta.tool_name.clone(), meta.input.clone()))
            .collect();
        for (id, _tool_name, input) in entries {
            self.pending_ask = parse_pending_ask(id.clone(), input);
            self.ask_step = 0;
            self.ask_transition_gen = self.ask_transition_gen.wrapping_add(1);
        }
        if self.pending_ask.is_some() {
            cx.notify();
        }
    }

    /// Archive the active thread and navigate to a fresh empty one. Shared
    /// by the `/exit` slash command and the `cmd-;` keybinding. No-op while
    /// a turn is running (a parked thread would strand pending verdicts).
    pub(crate) fn archive_current_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.archive_active_thread_if_idle(cx) {
            return;
        }
        self.start_new_thread(None, window, cx);
    }

    /// Archive the active thread if it is idle; `false` when a turn is
    /// running (attaching would park the thread and clear `pending_ask`,
    /// stranding a user verdict). Marks the thread archived and persists via
    /// the store, which refreshes the sidebar list.
    fn archive_active_thread_if_idle(&mut self, cx: &mut Context<Self>) -> bool {
        if self.thread.read(cx).is_running() {
            return false;
        }
        let id = self.thread.read(cx).id.0.clone();
        self.thread.update(cx, |t, cx| t.set_archived(true, cx));
        let store = agent::thread_store_global();
        store.update(cx, |s, cx| s.archive_thread(&id, true, cx));
        true
    }

    /// Archive the active thread and open a fresh one that inherits the
    /// outgoing thread's project, model, approval mode, and reasoning effort —
    /// `/new` starts a clean conversation without dropping the working
    /// context. No-op while a turn is running (see
    /// `archive_active_thread_if_idle`).
    pub(crate) fn archive_current_thread_inheriting(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.archive_active_thread_if_idle(cx) {
            return;
        }
        let old = self.thread.clone();
        let cwd = old.read(cx).cwd().to_path_buf();
        let project = old.read(cx).project().cloned();
        let model = old.read(cx).model().cloned();
        let effort = old.read(cx).reasoning_effort();
        let approval = old.read(cx).approval_mode();
        let new = match &project {
            Some(dir) => {
                Thread::new_in_project(ThreadId(uuid::Uuid::new_v4().to_string()), dir.clone(), cx)
            }
            None => Thread::new_fresh(ThreadId(uuid::Uuid::new_v4().to_string()), cwd, cx),
        };
        new.update(cx, |t, cx| {
            if let Some(model) = model {
                t.set_model(model, cx);
            }
            t.set_reasoning_effort(effort, cx);
            t.set_approval_mode(approval, cx);
        });
        self.attach_thread(new, window, cx);
    }

    /// Park the active thread into the background (preserving its run + event
    /// subscriptions) and open a fresh empty thread in the same project — the
    /// explicit "background this task, switch to a new one" gesture, bound to
    /// `ctrl-b`. No-op when the active thread is idle so the shortcut can't
    /// spam empty threads. `attach_thread` does the actual parking: a running
    /// thread is moved into `background_threads` with a terminal-`Stop`/`Error`
    /// subscription that flips its sidebar row to idle + unread when it lands.
    fn background_current_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.thread.read(cx).is_running() {
            return;
        }
        let project = self.thread.read(cx).project().cloned();
        self.start_new_thread(project, window, cx);
    }

    fn open_thread(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        // If the thread is already running in the background, reclaim it
        // instead of loading a stale snapshot from the db.
        if let Some(pos) = self
            .background_threads
            .iter()
            .position(|b| b.entity.read(cx).id.0 == id)
        {
            let thread = self.background_threads.remove(pos).entity;
            self.attach_thread(thread, window, cx);
            return;
        }
        let store = self.sidebar.read(cx).store();
        let Some(loaded) = store.update(cx, |s, cx| s.load_thread(&id, cx)) else {
            return;
        };
        self.attach_thread(loaded, window, cx);
    }

    pub(crate) fn submit_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // History is still loading: nothing may be submitted (the composer is
        // hidden, this guard covers keyboard paths that bypass the render).
        if self.thread.read(cx).history_phase().is_loading() {
            return;
        }
        let text = self.input_state.read(cx).value().to_string();
        let attachments = std::mem::take(&mut self.pending_attachments);
        if self.pending_ask.is_some() {
            self.pending_attachments = attachments;
            if !text.trim().is_empty() || self.pending_ask_has_selection() {
                self.input_state
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.close_completion(cx);
                self.resolve_ask_with_response(Some(text), cx);
            }
            return;
        }
        // Block submit on empty input or while the project picker is open.
        // Setting the project after a message lands is a no-op (`set_project`
        // guards on `!messages.is_empty()`), so the project would be silently
        // dropped. A message submitted while a turn is running is *not* dropped
        // here — it is routed through `send_user_turn`, which enqueues it as a
        // follow-up instead of interrupting the running turn.
        if (text.trim().is_empty() && attachments.is_empty()) || self.project_picker_pending {
            self.pending_attachments = attachments;
            return;
        }
        self.input_state
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.close_completion(cx);

        // Slash commands (line-initial `/name [args]`) are intercepted before
        // sending a normal user turn. A recognized command fully handles the
        // input (Handled), asks to inject text as a user turn (InjectUserTurn),
        // or declines (NoOp → fall through to the normal path). Slash parsing
        // only applies to text-only input; attachments force the normal path.
        // Slash commands only dispatch while idle — a `/name [args]` typed
        // while a turn is running is parked in the follow-up queue as raw text
        // rather than interrupting the run (e.g. `/clear` mid-turn would race
        // the streaming conversation). The queued text flushes at turn end.
        if !self.thread.read(cx).is_running()
            && attachments.is_empty()
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
                    this.add_info_message(i18n::t("composer-image-process-failed").to_string(), cx);
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

    /// Run a markdown prompt-macro slash turn (`/gitwork:deliver args`).
    pub(crate) fn run_command_turn(&mut self, name: &str, args: &str, cx: &mut Context<Self>) {
        self.run_registry_turn(RegistryTurnKind::Command, name, args, cx);
    }

    /// Run a skill slash turn (`/plugin:skill args` or bare `/skill`).
    pub(crate) fn run_skill_turn(&mut self, key: &str, args: &str, cx: &mut Context<Self>) {
        self.run_registry_turn(RegistryTurnKind::Skill, key, args, cx);
    }

    /// Shared dispatch for registry-backed slash turns: push the compact
    /// `/key args` display bubble, then hand the expanded body to the thread
    /// (`submit_command` / `submit_skill` render the registry content and run
    /// the turn). A registry miss surfaces as a thread error event. The
    /// transcript stores the expanded body, so a reloaded thread shows the
    /// body rather than the compact bubble (parity with the retired manox
    /// harness).
    fn run_registry_turn(
        &mut self,
        kind: RegistryTurnKind,
        key: &str,
        args: &str,
        cx: &mut Context<Self>,
    ) {
        let display_text = if args.is_empty() {
            format!("/{key}")
        } else {
            format!("/{key} {args}")
        };
        let meta = self.user_turn_meta(cx);
        // Persist the send-time chrome with the turn: the expanded macro/skill
        // body is the model-facing text, and `display_text` keeps the bubble
        // showing the compact `/key args` invocation after a reload — the same
        // form the live view shows at send time.
        let ui = agent::MessageUiMetadata {
            display_text: Some(display_text.clone()),
            ..Self::message_ui_metadata(&meta)
        };
        let weak = cx.weak_entity();
        self.conversation.update(cx, |c, cx| {
            c.push_user(display_text, Vec::new(), meta, weak, cx)
        });
        self.sync_list_count(cx);
        // Re-engage tail-follow so the streaming reply stays in view.
        self.follow_message_tail();
        let hit = self.thread.update(cx, |thread, cx| match kind {
            RegistryTurnKind::Command => thread.submit_command(key, args, Some(ui.clone()), cx),
            RegistryTurnKind::Skill => thread.submit_skill(key, args, Some(ui), cx),
        });
        if !hit {
            let i18n_key = match kind {
                RegistryTurnKind::Command => "workspace-unknown-command",
                RegistryTurnKind::Skill => "workspace-unknown-skill",
            };
            self.thread.update(cx, |_, cx| {
                cx.emit(ThreadEvent::Error(anyhow::anyhow!(
                    "{}",
                    i18n::t_str(i18n_key, &[("name", key)])
                )));
            });
        }
        // Persist on submit so the sidebar shows the new entry immediately.
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
        let meta = self.user_turn_meta(cx);
        let ui = Self::message_ui_metadata(&meta);
        let weak = cx.weak_entity();
        let user_images = Self::decode_user_images(&images);
        let turn = DeferredUserTurn {
            text,
            images,
            meta,
            ui,
            user_images,
        };
        // A turn is running — every new message first parks as a visible
        // follow-up. Steering is an explicit per-item action.
        if self.thread.read(cx).is_running() {
            self.queued_follow_ups.push_back(QueuedFollowUp {
                turn,
                state: FollowUpState::Queued,
            });
            cx.notify();
            return;
        }
        // The thread is idle (not running). If a plan review was awaiting a
        // verdict, a free-form message means the user is discussing or revising
        // rather than accepting — drop the stale verdict so the card hides
        // until the agent re-proposes via a fresh `PlanReady`. Without this the
        // lingering Implement button would act on the now-outdated plan text.
        let dismissed_plan = self.pending_plan_review.take();
        if let Some(review) = dismissed_plan.as_ref() {
            self.thread
                .update(cx, |t, cx| t.set_plan_review_pending(false, cx));
            self.conversation
                .update(cx, |c, cx| c.consume_plan_review(cx));
            // The demoted plan card stays in place but flips inactive —
            // remeasure so the list's cached height for it stays honest.
            self.list_state.remeasure();
            // Persist the dismissed plan as a UI note so the collapsed record
            // survives a thread switch / reload — the live card is UI-only and
            // never enters `Thread::messages`. `record_ui_note` anchors to the
            // last user message, which at this point (before the dismissing
            // message is appended below) is the one that triggered the plan's
            // turn, so the rebuild splices the card back at that turn's end,
            // ahead of this dismissing message — matching the live order.
            self.record_ui_note(
                agent::db::UiNoteKind::PlanReview,
                review.content.clone(),
                cx,
            );
        }
        self.append_and_run_user_turn(turn, weak, cx);
        // The conversation exists the moment the message is sent: refresh
        // the sidebar list now (the transcript-side refresh on the user
        // MessageEnd notice then fills in the summary text).
        save_thread(self.thread.clone(), true, cx);
    }

    /// Push a user turn into the conversation UI and the thread's message
    /// history without starting a turn. Used to batch drained follow-ups into a
    /// single follow-up turn.
    fn append_user_turn(
        &mut self,
        turn: DeferredUserTurn,
        weak: WeakEntity<Workspace>,
        follow_tail: bool,
        cx: &mut Context<Self>,
    ) {
        use agent::language_model::MessageContent;
        self.conversation.update(cx, |c, cx| {
            c.push_user(turn.text.clone(), turn.user_images, turn.meta, weak, cx)
        });
        self.sync_list_count(cx);
        // Re-engage tail-follow so the streaming reply stays pinned as it grows.
        if follow_tail {
            self.follow_message_tail();
        }
        self.thread.update(cx, |thread, cx| {
            if turn.images.is_empty() {
                thread.insert_user_message_with_ui_metadata(turn.text, Some(turn.ui), cx);
            } else {
                let mut content = Vec::with_capacity(turn.images.len() + 1);
                if !turn.text.trim().is_empty() {
                    content.push(MessageContent::Text(turn.text));
                }
                content.extend(turn.images);
                thread.insert_user_message_with_content_and_ui_metadata(content, Some(turn.ui), cx);
            }
        });
    }

    /// Hand a parked follow-up to the thread's steer queue — the running turn
    /// absorbs it at the next safe join point. No conversation bubble is pushed
    /// here. The caller immediately pushes an optimistic pending bubble; the
    /// later `SteerInjected` event confirms it. Returns the message id used to
    /// correlate the pending bubble and the canonical history message.
    fn enqueue_steer_pending(
        &mut self,
        turn: &mut DeferredUserTurn,
        cx: &mut Context<Self>,
    ) -> String {
        use agent::language_model::MessageContent;
        let mut content = Vec::with_capacity(turn.images.len() + 1);
        if !turn.text.trim().is_empty() {
            content.push(MessageContent::Text(turn.text.clone()));
        }
        content.append(&mut turn.images);
        let ui = std::mem::take(&mut turn.ui);
        // The steered tag is applied only when the drain succeeds (in
        // `consume_steered_follow_up`), so a stranded steer that the user
        // later retries as an idle fresh turn carries no badge — it was never
        // actually injected.
        self.thread
            .update(cx, |thread, cx| thread.enqueue_steer(content, Some(ui), cx))
    }

    fn consume_background_steer(&mut self, thread_id: &str, message_id: &str) {
        let Some(queue) = self.queued_follow_ups_by_thread.get_mut(thread_id) else {
            return;
        };
        if let Some(index) = queue.iter().position(|item| {
            matches!(
                &item.state,
                FollowUpState::SteerPending { message_id: id }
                    | FollowUpState::Failed { message_id: id }
                    if id == message_id
            )
        }) {
            queue.remove(index);
        }
        if queue.is_empty() {
            self.queued_follow_ups_by_thread.remove(thread_id);
        }
    }

    /// Render the optimistic message-list bubble for a steer that is still
    /// waiting in `Thread::pending_steer`.
    fn push_pending_steer_bubble(
        &mut self,
        turn: &DeferredUserTurn,
        message_id: &str,
        cx: &mut Context<Self>,
    ) {
        let weak = cx.weak_entity();
        self.conversation.update(cx, |conversation, cx| {
            conversation.push_pending_steer(
                turn.text.clone(),
                turn.user_images.clone(),
                turn.meta.clone(),
                message_id.to_string(),
                weak,
                cx,
            )
        });
        self.sync_list_count(cx);
        self.list_state.set_follow_mode(FollowMode::Tail);
    }

    fn append_and_run_user_turn(
        &mut self,
        turn: DeferredUserTurn,
        weak: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) {
        self.append_user_turn(turn, weak, true, cx);
        self.thread.update(cx, |thread, cx| thread.run_turn(cx));
        // Persist on submit so the sidebar shows the new entry immediately.
        save_thread(self.thread.clone(), true, cx);
        cx.notify();
    }

    /// Drain every parked `Queued` follow-up into a single new turn. Multiple
    /// messages coalesce into one user block, keeping the request prefix stable
    /// (mirrors the team inbox flush). `Failed` cards stay parked for the user
    /// to retry; `SteerPending` cards are marked `Failed` by
    /// `mark_stranded_steers_failed` before this runs, so none reach here.
    fn flush_queued_follow_ups(&mut self, cx: &mut Context<Self>) {
        if self.queued_follow_ups.is_empty() {
            return;
        }
        let weak = cx.weak_entity();
        let mut retain: Vec<QueuedFollowUp> = Vec::new();
        let mut drained = false;
        while let Some(item) = self.queued_follow_ups.pop_front() {
            match item.state {
                FollowUpState::Queued => {
                    self.append_user_turn(
                        item.turn,
                        weak.clone(),
                        self.list_state.is_following_tail(),
                        cx,
                    );
                    drained = true;
                }
                _ => retain.push(item),
            }
        }
        self.queued_follow_ups.extend(retain);
        if drained {
            self.thread.update(cx, |thread, cx| thread.run_turn(cx));
            save_thread(self.thread.clone(), true, cx);
        }
        cx.notify();
    }

    /// Mark every `SteerPending` card whose message id is still in the thread's
    /// steer queue as `Failed` — the running turn exited (terminal Stop/Error)
    /// before draining them, so they are stranded. Called at terminal states so
    /// the cards stop spinning and surface a retry instead of hanging forever.
    fn mark_stranded_steers_failed(
        &mut self,
        stranded_steer_ids: &[String],
        cx: &mut Context<Self>,
    ) {
        let stranded: std::collections::HashSet<&str> =
            stranded_steer_ids.iter().map(String::as_str).collect();
        if stranded.is_empty() {
            return;
        }
        let mut any = false;
        for item in self.queued_follow_ups.iter_mut() {
            if let FollowUpState::SteerPending { message_id } = &item.state
                && stranded.contains(message_id.as_str())
            {
                self.conversation.update(cx, |conversation, cx| {
                    conversation.rollback_pending_steer(message_id, cx);
                });
                self.list_state.remeasure();
                // Keep the id so a later `SteerInjected` can still heal this
                // provisional rollback.
                let message_id = message_id.clone();
                item.state = FollowUpState::Failed { message_id };
                any = true;
            }
        }
        if any {
            cx.notify();
        }
    }

    /// Confirm the optimistic bubble whose canonical message was just drained.
    /// `Failed` also matches so a late drain can heal a provisional rollback.
    fn consume_steered_follow_up(&mut self, message_id: &str, cx: &mut Context<Self>) {
        // Match either SteerPending or the prematurely-marked Failed card
        // (see the doc comment above) by the steer message id.
        let id_matches = |f: &QueuedFollowUp| match &f.state {
            FollowUpState::SteerPending { message_id: mid }
            | FollowUpState::Failed { message_id: mid } => mid == message_id,
            FollowUpState::Queued => false,
        };
        let Some(idx) = self.queued_follow_ups.iter().position(id_matches) else {
            return;
        };
        let Some(_) = self.queued_follow_ups.remove(idx) else {
            return;
        };
        self.conversation.update(cx, |c, cx| {
            c.confirm_pending_steer(message_id, cx);
        });
        self.list_state.remeasure();
        // Only re-engage tail-follow if the user hasn't scrolled away —
        // a steer injection is event-driven, not user-initiated, so it
        // should not yank the viewport back to the bottom.
        if self.list_state.is_following_tail() {
            self.list_state.scroll_to_end();
        }
        cx.notify();
    }

    /// Promote a parked follow-up to a steer. While running, enqueue it and
    /// immediately move an optimistic bubble into the message list. While idle,
    /// send it as a fresh ordinary turn.
    fn steer_follow_up(&mut self, idx: usize, cx: &mut Context<Self>) {
        let running = self.thread.read(cx).is_running();
        let Some(mut item) = self.queued_follow_ups.remove(idx) else {
            return;
        };
        match item.state {
            FollowUpState::Queued => {
                if running {
                    let id = self.enqueue_steer_pending(&mut item.turn, cx);
                    self.push_pending_steer_bubble(&item.turn, &id, cx);
                    item.state = FollowUpState::SteerPending { message_id: id };
                    self.queued_follow_ups.insert(idx, item);
                    cx.notify();
                } else {
                    let weak = cx.weak_entity();
                    self.append_and_run_user_turn(item.turn, weak, cx);
                }
            }
            FollowUpState::Failed { message_id } => {
                // Drop the stranded message so its id can never drain into a
                // later turn (no-op if the loop already cleared the queue).
                self.thread
                    .update(cx, |thread, _cx| thread.cancel_pending_steer(&message_id));
                if running {
                    let id = self.enqueue_steer_pending(&mut item.turn, cx);
                    self.push_pending_steer_bubble(&item.turn, &id, cx);
                    item.state = FollowUpState::SteerPending { message_id: id };
                    self.queued_follow_ups.insert(idx, item);
                    cx.notify();
                } else {
                    let weak = cx.weak_entity();
                    self.append_and_run_user_turn(item.turn, weak, cx);
                }
            }
            FollowUpState::SteerPending { .. } => {
                // Already in flight; restore untouched.
                self.queued_follow_ups.insert(idx, item);
            }
        }
    }

    fn delete_follow_up(&mut self, idx: usize, cx: &mut Context<Self>) {
        if let Some(item) = self.queued_follow_ups.remove(idx) {
            // A SteerPending or Failed card has a message that may still sit in
            // the thread's steer queue (SteerPending: enqueued and pending
            // drain; Failed: stranded, pending the loop's end-of-turn clear).
            // Removing the UI card alone would let that message drain later with
            // no matching card, so the steer would fire invisibly. Cancel it in
            // the thread too (no-op if already drained/cleared).
            let steer_id = match &item.state {
                FollowUpState::SteerPending { message_id }
                | FollowUpState::Failed { message_id } => Some(message_id.clone()),
                FollowUpState::Queued => None,
            };
            if let Some(id) = steer_id {
                self.thread
                    .update(cx, |thread, _cx| thread.cancel_pending_steer(&id));
                self.conversation.update(cx, |conversation, cx| {
                    conversation.rollback_pending_steer(&id, cx);
                });
                self.list_state.remeasure();
            }
            cx.notify();
        }
    }

    fn undo_last_queued(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = self.queued_follow_ups.pop_back() {
            let steer_id = match &item.state {
                FollowUpState::SteerPending { message_id }
                | FollowUpState::Failed { message_id } => Some(message_id.clone()),
                FollowUpState::Queued => None,
            };
            if let Some(id) = steer_id {
                self.thread
                    .update(cx, |thread, _cx| thread.cancel_pending_steer(&id));
                self.conversation.update(cx, |conversation, cx| {
                    conversation.rollback_pending_steer(&id, cx);
                });
                self.list_state.remeasure();
            }
            cx.notify();
        }
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

    /// Keep `active_right_tab` pointing at the same tab after the tab at
    /// `removed_ix` was just removed from `right_tabs`. Tabs after
    /// `removed_ix` shift left by one, so a still-live active tab to the right
    /// must decrement; the active tab itself being removed (and being the
    /// last) falls back to the new last tab.
    fn reseat_active_after_close(&mut self, removed_ix: usize) {
        if self.active_right_tab > removed_ix {
            self.active_right_tab -= 1;
        } else if self.active_right_tab >= self.right_tabs.len() {
            self.active_right_tab = self.right_tabs.len().saturating_sub(1);
        }
    }

    /// Open the markdown editor: hide the inline composer and transfer its draft
    /// into the editor so writing continues there. If an Editor tab is already
    /// present, just focus it. Submit from the editor with Cmd-Enter; close with
    /// Ctrl-G / Cmd-W to move the draft back.
    fn open_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Close any open inline menus so they don't linger behind the hidden footer.
        self.close_completion(cx);
        self.close_plus_menu();
        if let Some(ix) = self.editor_tab_ix() {
            self.set_active_right_tab(ix, cx);
            return;
        }
        let draft = self.input_state.read(cx).value().to_string();
        self.right_tabs.push(RightTab::Editor);
        let ix = self.right_tabs.len() - 1;
        self.editor_preview = false;
        self.editor_preview_md = None;
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
        self.editor_preview_md = None;
        self.input_state.update(cx, |s, cx| {
            s.set_value(draft, window, cx);
            s.focus(window, cx);
        });
        self.editor_state
            .update(cx, |s, cx| s.set_value("", window, cx));
        self.reseat_active_after_close(ix);
        self.editor_open = self
            .right_tabs
            .get(self.active_right_tab)
            .is_some_and(|t| matches!(t, RightTab::Editor));
        cx.notify();
    }

    /// Close a right-pane tab by index. The Editor tab routes through
    /// `close_editor` to preserve draft-transfer semantics.
    fn close_right_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.right_tabs.get(ix), Some(RightTab::Editor)) {
            self.close_editor(window, cx);
            return;
        }
        if let Some(RightTab::Browser(id)) = self.right_tabs.get(ix).cloned() {
            self.close_browser_tab(id, cx);
        }
        if let Some(RightTab::Member(name)) = self.right_tabs.get(ix).cloned() {
            self.member_panels.remove(&name);
            self.right_tabs.remove(ix);
            self.reseat_active_after_close(ix);
            cx.notify();
        }
        if let Some(RightTab::Subagent(id)) = self.right_tabs.get(ix).cloned() {
            self.subagent_panels.remove(&id);
            self.right_tabs.remove(ix);
            self.reseat_active_after_close(ix);
            cx.notify();
        }
    }
    /// Close the active right-pane tab if it is a browser tab. No-op
    /// otherwise (the keybinding is global; it should not close an Editor
    /// tab that happens to be active).
    pub fn close_active_browser_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(RightTab::Browser(id)) = self.right_tabs.get(self.active_right_tab).cloned() {
            self.close_browser_tab(id, cx);
        }
    }

    /// Open (or focus) a team member's observation panel in the right pane.
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
        let panel = crate::views::member_panel::MemberPanel::new(
            member_thread,
            name.to_string(),
            role,
            team.downgrade(),
            weak,
            cx,
        );
        self.member_panels.insert(name.to_string(), panel);
        self.right_tabs.push(RightTab::Member(name.to_string()));
        self.set_active_right_tab(self.right_tabs.len() - 1, cx);
        cx.notify();
    }
    /// Open (or focus) a sub-agent observation panel in the right pane.
    /// `topic` is the shared `subagent_topic` derivation so the tab title
    /// matches the rail and conversation rows.
    pub(crate) fn open_subagent_tab(
        &mut self,
        id: &str,
        subagent_type: &str,
        topic: &str,
        status: agent::ToolCallStatus,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self
            .right_tabs
            .iter()
            .position(|t| matches!(t, RightTab::Subagent(i) if i == id))
        {
            self.set_active_right_tab(ix, cx);
            return;
        }
        let backfill = self
            .subagent_transcripts
            .get(id)
            .cloned()
            .unwrap_or_default();
        let final_text = if backfill.is_empty() {
            self.agent_final_text(id, cx)
        } else {
            None
        };
        let title = crate::views::subagents::task_display_title(subagent_type, topic)
            .unwrap_or_else(|| id.to_string());
        let role = if subagent_type.is_empty() {
            "sub-agent".to_string()
        } else {
            subagent_type.to_string()
        };
        let panel = crate::views::subagent_panel::SubagentPanel::new(
            title,
            role,
            status,
            &backfill,
            final_text,
            cx.weak_entity(),
            cx,
        );
        self.subagent_panels.insert(id.to_string(), panel);
        self.right_tabs.push(RightTab::Subagent(id.to_string()));
        self.set_active_right_tab(self.right_tabs.len() - 1, cx);
        cx.notify();
    }

    /// Final answer of a finished Agent call, for panels opened after a
    /// reload when no live transcript was accumulated.
    fn agent_final_text(&self, id: &str, cx: &App) -> Option<String> {
        use agent::language_model::MessageContent;
        self.thread
            .read(cx)
            .messages()
            .iter()
            .flat_map(|m| m.content.iter())
            .find_map(|c| match c {
                MessageContent::ToolResult(r) if r.tool_use_id == id => Some(r.content.clone()),
                _ => None,
            })
    }

    /// Drop per-thread sub-agent observation state and close its tabs.
    fn clear_subagent_observation(&mut self) {
        self.subagent_panels.clear();
        self.subagent_transcripts.clear();
        let before = self.right_tabs.len();
        // Bulk removal shifts surviving tabs left by the number of dropped
        // tabs that sat before the active index; a plain OOB clamp would land
        // on the wrong tab when several sub-agent tabs precede it.
        let removed_before_active = self
            .right_tabs
            .iter()
            .take(self.active_right_tab.min(before))
            .filter(|t| matches!(t, RightTab::Subagent(_)))
            .count();
        self.right_tabs
            .retain(|t| !matches!(t, RightTab::Subagent(_)));
        if self.right_tabs.len() != before {
            self.active_right_tab = self
                .active_right_tab
                .saturating_sub(removed_before_active)
                .min(self.right_tabs.len().saturating_sub(1));
            self.editor_open = self
                .right_tabs
                .get(self.active_right_tab)
                .is_some_and(|t| matches!(t, RightTab::Editor));
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
        if !editor_can_submit(
            self.thread.read(cx).history_phase().is_loading(),
            self.thread.read(cx).is_running(),
            self.pending_ask.is_some(),
            &text,
        ) {
            return;
        }
        let meta = self.user_turn_meta(cx);
        let ui = Self::message_ui_metadata(&meta);
        let weak = cx.weak_entity();
        self.conversation.update(cx, |c, cx| {
            c.push_user(text.clone(), Vec::new(), meta, weak, cx)
        });
        self.sync_list_count(cx);
        self.follow_message_tail();
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
        self.editor_preview_md = None;
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
            steered: meta.steered.then_some(true),
            external_event: None,
            display_text: None,
        }
    }

    pub(crate) fn model_label(&self, cx: &mut Context<Self>) -> String {
        {
            self.thread
                .read(cx)
                .model()
                .map(agent::pi_providers::display_name)
                .unwrap_or_else(|| i18n::t("workspace-no-model").to_string())
        }
    }

    /// Push a system-styled notice into the conversation (no thread message,
    /// no model turn). Used by slash commands to report outcomes — e.g. the
    /// mode-change acknowledging the mode change. Renders as a neutral-toned
    /// `ConvItem::Notice` card (distinct from the red `ConvItem::Error`).
    ///
    /// Also persists the notice so a reloaded thread reproduces it. The live
    /// `push_notice` only touches `ConversationState`; the persisted copy is
    /// spliced back by `rebuild_from_messages` on next load, anchored to the
    /// current turn.
    pub fn add_info_message(&mut self, text: String, cx: &mut Context<Self>) {
        let weak = cx.weak_entity();
        self.conversation.update(cx, |c, cx| {
            c.push_notice(text.clone(), weak, cx);
        });
        self.record_ui_note(agent::db::UiNoteKind::Notice, text, cx);
        self.sync_list_count(cx);
        // If tail-follow is engaged the list reveals the notice; if the user
        // scrolled up, `FollowMode::Tail` has already disengaged so the
        // viewport stays put.
        cx.notify();
    }

    /// Persist a UI annotation (`Error` / `Notice`) to `thread_ui_notes`.
    /// The anchor is the current turn's user message — `None` before the first
    /// user message — so the rebuild can place the note at the end of its turn.
    /// Best-effort: the live item already rendered this turn; only the reload
    /// copy is at stake.
    ///
    /// Also appends to the in-memory `Thread::ui_notes` cache so a background
    /// thread reclaimed via `attach_thread` (which rebuilds from the entity,
    /// not a db reload) still reproduces the note. The placeholder row is
    /// discarded on the next db reload, which replaces the cache wholesale.
    fn record_ui_note(&self, kind: agent::db::UiNoteKind, text: String, cx: &mut Context<Self>) {
        let thread_id = self.thread.read(cx).id.0.clone();
        let anchor = self
            .thread
            .read(cx)
            .last_user_message_id()
            .map(str::to_owned);
        let data = serde_json::json!({ "text": text });
        // Keep the in-memory cache consistent with the persisted record so the
        // background-reclaim rebuild path (no db reload) reproduces the note.
        let cached = agent::db::UiNoteRecord {
            id: 0,
            thread_id: thread_id.clone(),
            seq: 0,
            anchor_user_id: anchor.clone(),
            kind,
            data: data.clone(),
            ts: 0,
        };
        self.thread.update(cx, |t, _| t.push_ui_note(cached));
        let store = agent::thread_store_global();
        store.update(cx, |s, cx| {
            s.record_ui_note(&thread_id, kind, anchor.as_deref(), &data, cx)
        });
    }

    pub(crate) fn resolve_auth(&mut self, decision: PermissionDecision, cx: &mut Context<Self>) {
        // An AskUserQuestion card's "Cancel" button calls this; the card is
        // the only pending surface, so resolve its id directly.
        let Some(ask) = self.pending_ask.as_ref() else {
            return;
        };
        let id = ask.id.clone();
        self.pending_ask = None;
        self.ask_step = 0;
        self.ask_transition_gen = self.ask_transition_gen.wrapping_add(1);
        self.thread.update(cx, |thread, cx| {
            thread.respond_authorization(
                &id,
                agent::ToolAuthorizationResponse::Decision(decision),
                cx,
            );
        });
        cx.notify();
    }

    /// Toggle an option in the pending ask card. Single-select questions reset
    /// siblings; multi-select toggles in place.
    pub(crate) fn ask_card_snapshot(&self, id: &str, _cx: &App) -> Option<AskCardSnapshot> {
        let ask = self.pending_ask.as_ref()?;
        if ask.id != id || ask.questions.is_empty() {
            return None;
        }
        let step = self.ask_step.min(ask.questions.len() - 1);
        let q = ask.questions.get(step)?;
        Some(AskCardSnapshot {
            id: ask.id.clone(),
            step,
            total: ask.questions.len(),
            transition_gen: self.ask_transition_gen,
            question: AskCardQuestion {
                question: q.question.clone(),
                header: q.header.clone(),
                multi_select: q.multi_select,
                options: q
                    .options
                    .iter()
                    .map(|o| AskCardOption {
                        label: o.label.clone(),
                        description: o.description.clone(),
                        recommended: o.recommended,
                    })
                    .collect(),
            },
            selections: ask.selections.get(step).cloned().unwrap_or_default(),
        })
    }

    /// Synchronize Workspace-derived ask state before the native list starts
    /// measuring. Updating a `MessageItem` from the row callback dirties the
    /// same entity tree whose height GPUI is currently caching, which can make
    /// the cached row and the painted subtree describe different layouts.
    fn sync_ask_card_snapshots(&mut self, cx: &mut App) {
        let next = self.pending_ask.as_ref().and_then(|ask| {
            let snapshot = self.ask_card_snapshot(&ask.id, cx)?;
            let ix = self.conversation.read(cx).find_tool(&ask.id, cx)?;
            let item = self.conversation.read(cx).items().get(ix)?.clone();
            Some((item, snapshot))
        });

        if let Some(previous) = self.ask_snapshot_item.take()
            && next
                .as_ref()
                .is_none_or(|(current, _)| current != &previous)
            && previous.read(cx).ask_snapshot.is_some()
        {
            previous.update(cx, |item, cx| {
                item.ask_snapshot = None;
                cx.notify();
            });
        }

        if let Some((item, snapshot)) = next {
            if item.read(cx).ask_snapshot.as_ref() != Some(&snapshot) {
                item.update(cx, |item, cx| {
                    item.ask_snapshot = Some(snapshot);
                    cx.notify();
                });
            }
            self.ask_snapshot_item = Some(item);
        }
    }

    fn pending_ask_has_selection(&self) -> bool {
        self.pending_ask
            .as_ref()
            .is_some_and(|ask| ask.selections.iter().flatten().any(|selected| *selected))
    }

    fn composer_can_submit(&self, running: bool, cx: &App) -> bool {
        if self.thread.read(cx).history_phase().is_loading() {
            return false;
        }
        if running {
            return true;
        }
        let input_empty = self.input_state.read(cx).value().trim().is_empty();
        if self.pending_ask.is_some() {
            !input_empty || self.pending_ask_has_selection()
        } else {
            !input_empty || !self.pending_attachments.is_empty()
        }
    }

    pub(crate) fn toggle_ask_option(&mut self, qi: usize, oi: usize, cx: &mut Context<Self>) {
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

    pub(crate) fn ask_prev(&mut self, cx: &mut Context<Self>) {
        if self.ask_step > 0 {
            self.ask_step -= 1;
            cx.notify();
        }
    }

    pub(crate) fn ask_next(&mut self, cx: &mut Context<Self>) {
        if let Some(ask) = self.pending_ask.as_ref()
            && self.ask_step < ask.questions.len() - 1
        {
            self.ask_step += 1;
            cx.notify();
        }
    }

    /// Submit the ask drawer: gather selected options plus an optional global
    /// supplemental note from the composer.
    pub(crate) fn resolve_ask_with_response(
        &mut self,
        response_override: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let ask = match self.pending_ask.take() {
            Some(a) => a,
            None => return,
        };
        let response_text = response_override.unwrap_or_default();
        let response = if response_text.trim().is_empty() {
            None
        } else {
            Some(response_text.trim().to_string())
        };
        let mut answers: Vec<(String, String)> = Vec::with_capacity(ask.questions.len());
        for (i, q) in ask.questions.iter().enumerate() {
            let sel = ask.selections.get(i).map(|s| s.as_slice()).unwrap_or(&[]);
            let selected: Vec<&str> = q
                .options
                .iter()
                .zip(sel.iter())
                .filter_map(|(o, &s)| s.then_some(o.label.as_str()))
                .collect();
            let answer = selected.join(", ");
            answers.push((q.question.clone(), answer));
        }
        let id = ask.id.clone();
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

    /// Resolve the pending plan review with the user's three-way verdict.
    /// Implement (with or without a context clear) delegates to the thread,
    /// which exits Plan mode and re-injects the plan as the implement turn's
    /// seed; the rail's plan overview seeds later from the model's first
    /// `UpdatePlan` call. Staying in Plan mode is not a verdict — the user
    /// simply keeps typing.
    /// Plan mode active on the current thread (drives the composer chip).
    pub(crate) fn thread_plan_mode(&self, cx: &mut Context<Self>) -> bool {
        self.thread.read(cx).plan_mode()
    }

    /// Toggle plan mode on the current thread (persisted by the engine).
    pub(crate) fn set_thread_plan_mode(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.thread.update(cx, |t, cx| t.set_plan_mode(enabled, cx));
    }

    /// The user's verdict on a proposed plan (oh-my-pi's four options):
    /// execute in fresh context / compact then execute / keep context and
    /// execute / refine. Execute verdicts exit plan mode and run the rendered
    /// execution seed referencing the plan file; refine keeps plan mode on
    /// and waits for the user's feedback turn.
    pub(crate) fn respond_plan_review(
        &mut self,
        choice: PlanReviewChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = self.pending_plan_review.take() else {
            return;
        };
        // Every verdict consumes the card; clear the persisted pending flag
        // and release the sidebar's plan-wait state. Capture the id before
        // `ExecuteFresh` swaps in a new thread below.
        let thread_id = self.thread.read(cx).id.0.clone();
        self.thread
            .update(cx, |t, cx| t.set_plan_review_pending(false, cx));
        agent::thread_store_global().update(cx, |s, cx| s.mark_pending_plan(&thread_id, false, cx));
        if matches!(choice, PlanReviewChoice::Refine) {
            // Keep plan mode ON: demote the card and prompt for feedback.
            // The feedback turn runs under the plan-mode instructions; the
            // model updates the plan file and proposes again (fresh card).
            self.conversation
                .update(cx, |c, cx| c.consume_plan_review(cx));
            self.list_state.remeasure();
            self.add_info_message(i18n::t("plan-refine-notice").to_string(), cx);
            cx.notify();
            return;
        }
        let lang = self.thread.read(cx).agent_language();
        let seed_text =
            match agent::collaboration_mode::render_plan_mode_approved(lang, &review.plan_file) {
                Ok(text) => text,
                Err(err) => {
                    tracing::warn!(error = %err, "failed to render plan execution seed");
                    format!(
                        "Read and implement the approved plan at {}",
                        review.plan_file
                    )
                }
            };
        let meta = self.user_turn_meta(cx);
        let ui = Self::message_ui_metadata(&meta);
        if matches!(choice, PlanReviewChoice::ExecuteFresh) {
            // Fresh context: archive this thread and continue on a new one
            // seeded with the execution directive. The plan file persists on
            // disk, so the new thread reads it from the path — no inline
            // plan-text copy in the transcript.
            let old_id = self.thread.read(cx).id.0.clone();
            let cwd = self.thread.read(cx).cwd().to_path_buf();
            let project = self.thread.read(cx).project().cloned();
            let model = self.thread.read(cx).model().cloned();
            let effort = self.thread.read(cx).reasoning_effort();
            let approval = self.thread.read(cx).approval_mode();
            let new = match &project {
                Some(dir) => Thread::new_in_project(
                    ThreadId(uuid::Uuid::new_v4().to_string()),
                    dir.clone(),
                    cx,
                ),
                None => Thread::new_fresh(ThreadId(uuid::Uuid::new_v4().to_string()), cwd, cx),
            };
            new.update(cx, |t, cx| {
                if let Some(model) = model {
                    t.set_model(model, cx);
                }
                t.set_reasoning_effort(effort, cx);
                t.set_approval_mode(approval, cx);
                t.seed_plan_execution(review.plan_file.clone(), seed_text, Some(ui), cx);
            });
            self.attach_thread(new, window, cx);
            save_thread(self.thread.clone(), true, cx);
            agent::thread_store_global().update(cx, |s, cx| s.archive_thread(&old_id, true, cx));
        } else {
            // Compact/keep-context: the engine exits plan mode, optionally
            // compacts the planning context toward the plan file, then runs
            // the seed turn. Retire the card and push the verdict bubble so
            // live and rebuilt views match.
            let weak = cx.weak_entity();
            self.conversation.update(cx, |c, cx| {
                c.pop_plan_review_tail(cx);
            });
            self.conversation.update(cx, |c, cx| {
                c.push_user(seed_text.clone(), Vec::new(), meta, weak, cx);
            });
            self.sync_list_count(cx);
            let ix = self.conversation.read(cx).items().len().saturating_sub(1);
            self.list_state.remeasure_items(ix..ix + 1);
            self.list_state.set_follow_mode(FollowMode::Tail);
            let compact = matches!(choice, PlanReviewChoice::ExecuteCompact);
            let compact_instructions = compact.then(|| {
                agent::collaboration_mode::plan_compact_instructions(lang, &review.plan_file)
            });
            self.thread.update(cx, |thread, cx| {
                thread.approve_plan(compact, compact_instructions, seed_text, cx);
            });
        }
        cx.notify();
    }

    /// Wire api string → Tag variant + label for the pi model menu.
    fn pi_wire_tag_variant(api: &str) -> (TagVariant, &'static str) {
        match api {
            "anthropic" => (TagVariant::Primary, "Anthropic"),
            "openai_responses" => (TagVariant::Info, "Responses"),
            "openai_completions" => (TagVariant::Warning, "Completions"),
            _ => (TagVariant::Secondary, "N/A"),
        }
    }

    /// Wire api string → text color for the pi composer model label and the
    /// context rail's per-model usage rows.
    pub(crate) fn pi_wire_text_color(api: &str, theme: &Theme) -> gpui::Hsla {
        match api {
            "anthropic" => theme.primary,
            "openai_responses" => theme.info,
            "openai_completions" => theme.warning,
            _ => theme.muted_foreground,
        }
    }

    /// The pi-harness model selector. Reads the shared pi provider registry
    /// (the streaming source of truth): closed, a ghost button showing
    /// `provider · model` with the model name tinted by wire api; open, a
    /// PopupMenu of provider submenus.
    fn render_model_selector_pi(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let open = self.model_open;
        let model = self.thread.read(cx).model().cloned();

        let trigger = h_flex()
            .id("model-trigger")
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.accent.opacity(0.08)))
            .cursor_pointer()
            .children(if let Some(ref m) = model {
                let model_color = Self::pi_wire_text_color(&m.api, theme);
                let dot = || {
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("·")
                };
                vec![
                    gpui::div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(agent::pi_providers::display_provider_name(m))
                        .into_any_element(),
                    dot().into_any_element(),
                    gpui::div()
                        .text_xs()
                        .text_color(model_color)
                        .child(agent::pi_providers::display_name(m))
                        .into_any_element(),
                ]
            } else {
                vec![
                    gpui::div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(i18n::t("workspace-no-model").to_string())
                        .into_any_element(),
                ]
            })
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
                        Self::build_model_popup_menu_pi(menu, workspace, window, cx)
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
                deferred(
                    gpui::div()
                        .id("model-dropdown")
                        .absolute()
                        .bottom_full()
                        .right_0()
                        .occlude()
                        .child(menu),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    /// Model menu for the pi harness: grouped by provider display name;
    /// each row shows a wire-api Tag and selects through the registry.
    fn build_model_popup_menu_pi(
        menu: PopupMenu,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        // Group by DISPLAY name via lookup (not adjacency): models() is
        // sorted by registration name, so same-display-name providers with
        // different registrations must still merge into one submenu.
        let mut providers: Vec<(String, Vec<pi::types::Model>)> = Vec::new();
        for m in agent::pi_providers::global().models() {
            let prov = agent::pi_providers::display_provider_name(&m);
            match providers.iter_mut().find(|(name, _)| *name == prov) {
                Some((_, models)) => models.push(m),
                None => providers.push((prov, vec![m])),
            }
        }
        let mut menu = menu;
        if providers.is_empty() {
            return menu.item(PopupMenuItem::Label("No models configured".into()));
        }
        for (prov_name, models) in providers {
            let ws = workspace.clone();
            menu = menu.submenu(prov_name, window, cx, move |submenu, _window, _cx| {
                let mut submenu = submenu;
                for m in &models {
                    let model = m.clone();
                    let model_name = agent::pi_providers::display_name(&model);
                    let (variant, label) = Self::pi_wire_tag_variant(&model.api);
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
                            let model = model.clone();
                            let _ = ws.update(cx, |this, cx| {
                                this.thread.update(cx, |t, cx| t.set_model(model, cx));
                            });
                        }),
                    );
                }
                submenu
            });
        }
        // The reasoning-effort knob lives in the model dropdown, next to the
        // model switch it tunes. The current effort is checked; a click
        // applies to the next request (same mid-run semantics as a model
        // switch; the menu dismisses like a model row).
        let current_effort = workspace
            .update(cx, |this, cx| this.thread.read(cx).reasoning_effort())
            .unwrap_or_default();
        let themed = cx.theme().clone();
        menu = menu.separator();
        menu = menu.label(i18n::t("workspace-reasoning-effort"));
        for effort in agent::language_model::ReasoningEffort::ALL {
            let ws = workspace.clone();
            let themed = themed.clone();
            let label = match effort {
                agent::language_model::ReasoningEffort::High => i18n::t("workspace-reasoning-high"),
                agent::language_model::ReasoningEffort::Max => i18n::t("workspace-reasoning-max"),
            };
            let selected = effort == current_effort;
            menu = menu.item(
                PopupMenuItem::element(move |_window, _cx| {
                    h_flex()
                        .items_center()
                        .gap_1()
                        .child(
                            gpui::div()
                                .text_sm()
                                .text_color(themed.foreground)
                                .child(label.clone()),
                        )
                        .when(selected, |el| {
                            el.child(Icon::new(IconName::Check).small().text_color(themed.accent))
                        })
                })
                .on_click(move |_, _, cx: &mut gpui::App| {
                    let _ = ws.update(cx, |this, cx| {
                        this.thread.update(cx, |t, cx| {
                            t.set_reasoning_effort(effort, cx);
                        });
                    });
                }),
            );
        }
        menu
    }

    /// Pure history-recall step for the composer input. `turns` is
    /// newest-first; `index` -1 means not recalling. Being in recall
    /// requires the current value to still equal the recalled text, so any
    /// edit or submit exits recall implicitly.
    fn recall_step(
        direction: RecallDirection,
        value: &str,
        index: i64,
        turns: &[String],
    ) -> (i64, RecallStep) {
        let in_recall = index >= 0
            && usize::try_from(index)
                .ok()
                .and_then(|ix| turns.get(ix))
                .is_some_and(|text| value == text);
        match direction {
            RecallDirection::Up => {
                if in_recall {
                    let ix = index as usize;
                    if ix + 1 < turns.len() {
                        (index + 1, RecallStep::Recall(turns[ix + 1].clone()))
                    } else {
                        (index, RecallStep::None)
                    }
                } else if value.is_empty() {
                    match turns.first() {
                        Some(newest) => (0, RecallStep::Recall(newest.clone())),
                        None => (-1, RecallStep::None),
                    }
                } else {
                    (-1, RecallStep::None)
                }
            }
            RecallDirection::Down => {
                if in_recall {
                    if index > 0 {
                        (
                            index - 1,
                            RecallStep::Recall(turns[index as usize - 1].clone()),
                        )
                    } else {
                        (-1, RecallStep::Clear)
                    }
                } else {
                    (-1, RecallStep::None)
                }
            }
        }
    }

    fn composer_recall_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_recall_step(RecallDirection::Up, window, cx);
    }

    fn composer_recall_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.apply_recall_step(RecallDirection::Down, window, cx);
    }

    fn apply_recall_step(
        &mut self,
        direction: RecallDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let turns = self.recall_turns(cx);
        let value = self.input_state.read(cx).value().to_string();
        let (index, step) = Self::recall_step(direction, &value, self.recall_index, &turns);
        self.recall_index = index;
        match step {
            RecallStep::None => {}
            RecallStep::Recall(text) => {
                self.input_state.update(cx, |s, cx| {
                    s.set_value(text, window, cx);
                    let pos = RopeExt::offset_to_position(s.text(), s.text().len());
                    s.set_cursor_position(pos, window, cx);
                });
                cx.stop_propagation();
            }
            RecallStep::Clear => {
                self.input_state.update(cx, |s, cx| {
                    s.set_value(String::new(), window, cx);
                    let pos = RopeExt::offset_to_position(s.text(), 0);
                    s.set_cursor_position(pos, window, cx);
                });
                cx.stop_propagation();
            }
        }
    }

    /// Newest-first user-turn texts for recall, mirroring the navigator's
    /// `collect_user_turns` ordering minus image-only/empty turns.
    fn recall_turns(&self, cx: &App) -> Vec<String> {
        collect_user_turns(
            self.conversation
                .read(cx)
                .items()
                .iter()
                .enumerate()
                .map(|(ix, item)| (ix, item.read(cx).kind())),
        )
        .into_iter()
        .filter(|t| !t.text.trim().is_empty())
        .map(|t| t.text)
        .collect()
    }

    /// Rendered bare — no card border, fill, or rounding — so it shares the
    /// page background with the message list and reads as the same layer.
    /// The `Input` has no appearance of its own; the only visual separator
    /// from the messages above is the hairline injected by the footer caller.
    fn render_composer(
        &mut self,
        running: bool,
        window: &mut Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Flip the composer placeholder only on mode transitions, so render
        // doesn't churn the InputState every frame.
        let followup_mode =
            running && self.pending_plan_review.is_none() && self.pending_ask.is_none();
        let placeholder_mode = if self.pending_ask.is_some() {
            ComposerPlaceholderMode::Ask
        } else if followup_mode {
            ComposerPlaceholderMode::FollowUp
        } else {
            ComposerPlaceholderMode::Normal
        };
        if placeholder_mode != self.composer_placeholder_mode {
            self.composer_placeholder_mode = placeholder_mode;
            let key = match placeholder_mode {
                ComposerPlaceholderMode::Normal => "workspace-input-placeholder",
                ComposerPlaceholderMode::FollowUp => "composer-placeholder-followup",
                ComposerPlaceholderMode::Ask => "workspace-ask-supplement-placeholder",
            };
            self.input_state.update(cx, |state, cx| {
                state.set_placeholder(i18n::t(key), window, cx);
            });
        }
        let queue = self.render_queued_follow_ups(theme, cx);
        let plus = self.render_plus_button(cx);
        let project_chip = self.render_project_chip_pi(theme, cx);
        let goal_chip = self.render_goal_chip(theme, cx);
        let team_chip = self.render_team_chip(theme, cx);
        let plan_chip = self.render_plan_chip(theme, cx);
        let access = self.render_access_placeholder(theme, cx);
        let model = self.render_model_selector_pi(theme, cx);
        let send = self.render_send_button(
            running && self.pending_plan_review.is_none() && self.pending_ask.is_none(),
            cx,
        );
        // The completion popover overlays the composer; anchoring it on the
        // composer's own v_flex keeps it glued to the input bar in both hero
        // and footer, with a single mount point and ElementId.
        let completion_overlay = self.render_completion_overlay(cx);

        v_flex()
            .w_full()
            .gap_2()
            .relative()
            .children(queue)
            .children(completion_overlay)
            // Own paste at the capture phase so a clipboard image becomes a
            // pending attachment instead of letting `InputState::paste` insert
            // the image's alt-text. `stop_propagation` keeps the inner input's
            // text-paste handler from also running; text is inserted via the
            // public `replace` so the completion popover re-sync still fires.
            .capture_action(cx.listener(|this, _: &Paste, window, cx| {
                cx.stop_propagation();
                let Some(clipboard) = cx.read_from_clipboard() else {
                    return;
                };
                let entries = clipboard.entries();
                let has_image = entries
                    .iter()
                    .any(|e| matches!(e, gpui::ClipboardEntry::Image(_)));
                if has_image {
                    for entry in entries {
                        if let gpui::ClipboardEntry::Image(image) = entry {
                            this.handle_pasted_image(image.clone(), cx);
                        }
                    }
                    cx.notify();
                } else {
                    let text = clipboard.text().unwrap_or_default();
                    if !text.is_empty() {
                        this.input_state
                            .update(cx, |state, cx| state.replace(text, window, cx));
                        this.sync_completion(window, cx);
                    }
                }
            }))
            .when(self.pending_ask.is_some(), |this| {
                this.child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(i18n::t("workspace-ask-supplement-label")),
                )
            })
            .child(
                // Composer input is message content in the mono family (Lilex)
                // at Light weight — the message-list body typeface. The `Input`
                // component forces `text_sm()` internally (its default
                // `Size::Medium` maps through `input_text_size`), so the host
                // pins the body size back with an instance-level
                // `.text_size(MESSAGE_BODY_SIZE)` (13px, one step below chrome
                // `text_base`); family + weight are applied from the wrapper
                // context.
                {
                    let mut wrap = gpui::div()
                        .font_family(theme.mono_font_family.clone())
                        .font_weight(gpui::FontWeight::LIGHT);
                    // The wrapper carries exactly one key context: with the
                    // completion popover open, `completion = open` (so the
                    // `completion == open > Input` bindings shadow the Input's
                    // own up/down/enter/tab/escape and drive the popover);
                    // otherwise `composer = recall`, so the recall bindings
                    // shadow up/down while the composer input is focused and
                    // defer to native caret movement unless a recall applies.
                    // The contexts are mutually exclusive, so the two
                    // same-depth binding sets never both match.
                    if self.completion.is_some() {
                        wrap = wrap.key_context("completion = open");
                    } else {
                        wrap = wrap.key_context("composer = recall");
                    }
                    wrap.child(
                        Input::new(&self.input_state)
                            .appearance(false)
                            .text_size(crate::views::message::MESSAGE_BODY_SIZE),
                    )
                },
            )
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
                            .when_some(goal_chip, |el, chip| el.child(chip))
                            .when_some(team_chip, |el, chip| el.child(chip))
                            .when_some(plan_chip, |el, chip| el.child(chip))
                            .child(access),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_1()
                            .flex_shrink_0()
                            .child(model)
                            .child(send),
                    ),
            )
            .into_any_element()
    }

    /// Render the compact queue above the composer. Pending steers live in the
    /// message list, so this area contains only ordinary queued rows and failed
    /// steers that need an explicit retry or deletion.
    fn render_queued_follow_ups(&self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut rows = Vec::with_capacity(self.queued_follow_ups.len());
        for (idx, item) in self.queued_follow_ups.iter().enumerate() {
            if matches!(item.state, FollowUpState::SteerPending { .. }) {
                continue;
            }
            let summary = truncate_follow_up(&item.turn.text);
            let delete_btn = Button::new(format!("queue-delete-{idx}"))
                .ghost()
                .xsmall()
                .icon(Icon::default().path("icons/trash-2.svg"))
                .tooltip(i18n::t("queued-delete-action"))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.delete_follow_up(idx, cx);
                }));
            let more_btn = Button::new(format!("queue-more-{idx}"))
                .ghost()
                .xsmall()
                .icon(IconName::Ellipsis)
                .tooltip(i18n::t("queued-more-action"));

            let (action_btn, danger): (AnyElement, bool) = match &item.state {
                FollowUpState::Queued => {
                    let steer_btn = Button::new(format!("queue-steer-{idx}"))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Redo2)
                        .label(i18n::t("queued-steer-action"))
                        .tooltip(i18n::t("queued-steer-action"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.steer_follow_up(idx, cx);
                        }));
                    (steer_btn.into_any_element(), false)
                }
                FollowUpState::Failed { .. } => {
                    let retry_btn = Button::new(format!("queue-steer-{idx}"))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Redo2)
                        .label(i18n::t("queued-steer-retry-action"))
                        .tooltip(i18n::t("queued-steer-retry-action"))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.steer_follow_up(idx, cx);
                        }));
                    (retry_btn.into_any_element(), true)
                }
                FollowUpState::SteerPending { .. } => unreachable!(),
            };

            let summary_color = if danger {
                theme.danger
            } else {
                theme.foreground
            };

            let left = h_flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .flex_1()
                .child(
                    Icon::default()
                        .path("icons/corner-right-up.svg")
                        .xsmall()
                        .text_color(if danger {
                            theme.danger
                        } else {
                            theme.muted_foreground
                        }),
                )
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w_0()
                        .overflow_x_hidden()
                        .text_xs()
                        .text_color(summary_color)
                        .child(summary),
                );

            let right = h_flex()
                .items_center()
                .gap_0p5()
                .flex_shrink_0()
                .child(action_btn)
                .child(delete_btn)
                .child(more_btn);

            rows.push(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(theme.border.opacity(0.6))
                    .when(danger, |row| row.bg(theme.danger.opacity(0.08)))
                    .child(left)
                    .child(right)
                    .into_any_element(),
            );
        }
        rows
    }

    /// Open the goal status popover (from the bare `/goal` command).
    pub fn open_goal_popover(&mut self, cx: &mut Context<Self>) {
        self.goal_popover_open = true;
        cx.notify();
    }

    /// Prefill the composer with the durable objective so `/goal edit` is an
    /// explicit, inspectable update rather than an ephemeral popover field.
    pub fn begin_goal_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(objective) = self
            .thread
            .read(cx)
            .goal()
            .map(|goal| goal.objective.clone())
        else {
            self.goal_popover_open = true;
            cx.notify();
            return;
        };
        self.input_state.update(cx, |state, cx| {
            state.set_value(format!("/goal edit {objective}"), window, cx);
        });
        cx.notify();
    }

    pub fn begin_goal_budget_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self
            .thread
            .read(cx)
            .goal()
            .and_then(|goal| goal.token_budget)
            .map(|budget| budget.to_string())
            .unwrap_or_else(|| "none".into());
        self.input_state.update(cx, |state, cx| {
            state.set_value(format!("/goal budget {value}"), window, cx);
        });
        cx.notify();
    }

    /// Selecting Replace only prepares an explicit confirmation command; the
    /// persisted CAS replacement happens when the user submits it.
    pub fn begin_goal_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.begin_goal_replace_with_objective("", window, cx);
    }

    pub fn begin_goal_replace_with_objective(
        &mut self,
        objective: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.input_state.update(cx, |state, cx| {
            state.set_value(format!("/goal replace {objective}"), window, cx);
        });
        cx.notify();
    }

    pub fn begin_goal_new(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input_state.update(cx, |state, cx| {
            state.set_value("/goal ".to_string(), window, cx);
        });
        cx.notify();
    }

    fn render_goal_chip(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let g = self.thread.read(cx).goal()?;
        let accent = theme.accent;
        let muted = theme.muted_foreground;
        let fg = theme.foreground;
        let status_key = match g.status {
            agent::goal::GoalStatus::Active => "goal-status-active",
            agent::goal::GoalStatus::Paused => "goal-status-paused",
            agent::goal::GoalStatus::Blocked => "goal-status-blocked",
            agent::goal::GoalStatus::BudgetLimited => "goal-status-budget-limited",
            agent::goal::GoalStatus::Complete => "goal-status-complete",
        };
        let elapsed = format_elapsed(std::time::Duration::from_secs(
            self.thread
                .read(cx)
                .goal_elapsed_seconds()
                .unwrap_or_default(),
        ));
        let label: SharedString = format!("◎ {} · {}", i18n::t(status_key), elapsed).into();
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
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.accent_foreground)
                    .child(label),
            )
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

        let objective = g.objective.clone();
        let status = i18n::t(status_key);
        let reason = g.status_reason.clone().unwrap_or_else(|| "—".into());
        let tokens = g.tokens_used.to_string();
        let budget = g
            .token_budget
            .map(|value| value.to_string())
            .unwrap_or_else(|| "∞".into());
        let remaining = g
            .remaining_tokens()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "∞".into());
        let goal_status = g.status;
        let objective_label = i18n::t("goal-popover-objective");
        let status_label = i18n::t("goal-popover-status");
        let elapsed_label = i18n::t("goal-popover-elapsed");
        let reason_label = i18n::t("goal-popover-reason");
        let tokens_label = i18n::t("goal-popover-tokens");
        let budget_label = i18n::t("goal-popover-budget");
        let remaining_label = i18n::t("goal-popover-remaining");
        let clear_label = i18n::t("goal-popover-clear");
        let pause_label = i18n::t("goal-popover-pause");
        let resume_label = i18n::t("goal-popover-resume");
        let edit_label = i18n::t("goal-popover-edit");
        let edit_budget_label = i18n::t("goal-popover-edit-budget");
        let replace_label = i18n::t("goal-popover-replace");
        let new_label = i18n::t("goal-popover-new");
        let title_label = i18n::t("goal-popover-title");
        let popover = v_flex()
            .w_full()
            .gap_1()
            .p_3()
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.accent_foreground)
                    .child(format!("◎ {title_label}")),
            )
            .child(goal_popover_row(&objective_label, &objective, fg, muted))
            .child(goal_popover_row(&status_label, &status, fg, muted))
            .child(goal_popover_row(&elapsed_label, &elapsed, fg, muted))
            .child(goal_popover_row(&reason_label, &reason, fg, muted))
            .child(goal_popover_row(&tokens_label, &tokens, fg, muted))
            .child(goal_popover_row(&budget_label, &budget, fg, muted))
            .child(goal_popover_row(&remaining_label, &remaining, fg, muted))
            .child(
                h_flex()
                    .justify_end()
                    .gap_1()
                    .when(goal_status == agent::goal::GoalStatus::Active, |row| {
                        row.child(
                            Button::new("goal-pause")
                                .small()
                                .label(pause_label)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.thread.update(cx, |t, cx| {
                                        if let Err(error) = t.set_goal_status(
                                            agent::goal::GoalStatus::Paused,
                                            Some("paused by user".into()),
                                            agent::db::GoalActor::User,
                                            cx,
                                        ) {
                                            cx.emit(ThreadEvent::Error(error));
                                        }
                                    });
                                })),
                        )
                    })
                    .when(
                        matches!(
                            goal_status,
                            agent::goal::GoalStatus::Paused | agent::goal::GoalStatus::Blocked
                        ),
                        |row| {
                            row.child(
                                Button::new("goal-resume")
                                    .small()
                                    .label(resume_label)
                                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                        this.thread.update(cx, |t, cx| {
                                            if let Err(error) = t.set_goal_status(
                                                agent::goal::GoalStatus::Active,
                                                None,
                                                agent::db::GoalActor::User,
                                                cx,
                                            ) {
                                                cx.emit(ThreadEvent::Error(error));
                                            }
                                        });
                                    })),
                            )
                        },
                    )
                    .when(
                        matches!(
                            goal_status,
                            agent::goal::GoalStatus::Active
                                | agent::goal::GoalStatus::Paused
                                | agent::goal::GoalStatus::Blocked
                        ),
                        |row| {
                            row.child(Button::new("goal-edit").small().label(edit_label).on_click(
                                cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.goal_popover_open = false;
                                    this.begin_goal_edit(window, cx);
                                }),
                            ))
                        },
                    )
                    .when(
                        goal_status == agent::goal::GoalStatus::BudgetLimited,
                        |row| {
                            row.child(
                                Button::new("goal-edit-budget")
                                    .small()
                                    .label(edit_budget_label)
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.goal_popover_open = false;
                                            this.begin_goal_budget_edit(window, cx);
                                        },
                                    )),
                            )
                        },
                    )
                    .when(
                        matches!(
                            goal_status,
                            agent::goal::GoalStatus::Paused
                                | agent::goal::GoalStatus::Blocked
                                | agent::goal::GoalStatus::BudgetLimited
                        ),
                        |row| {
                            row.child(
                                Button::new("goal-replace")
                                    .small()
                                    .label(replace_label)
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.goal_popover_open = false;
                                            this.begin_goal_replace(window, cx);
                                        },
                                    )),
                            )
                        },
                    )
                    .when(goal_status == agent::goal::GoalStatus::Complete, |row| {
                        row.child(Button::new("goal-new").small().label(new_label).on_click(
                            cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.goal_popover_open = false;
                                this.begin_goal_new(window, cx);
                            }),
                        ))
                    })
                    .child(
                        Button::new("goal-clear")
                            .small()
                            .label(clear_label)
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.thread.update(cx, |t, cx| {
                                    if let Err(error) = t.clear_goal(agent::db::GoalActor::User, cx)
                                    {
                                        cx.emit(ThreadEvent::Error(error));
                                    }
                                });
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
                    deferred(
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
                    .with_priority(1),
                )
                .into_any_element(),
        )
    }

    fn render_team_chip(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let team = self.thread.read(cx).team().cloned()?;
        // Collect member roster metadata in one pass so we never hold a
        // borrow on the `Team` entity across the render closures below.
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
            .child(
                Icon::new(IconName::User)
                    .xsmall()
                    .text_color(theme.accent_foreground),
            )
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.accent_foreground)
                    .child(label),
            )
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
                            theme.accent_foreground
                        } else {
                            theme.muted_foreground
                        };
                        let tasks_label = i18n::t_str_count("team-drawer-tasks", &[], owned as i64);
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
                    deferred(
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
                    .with_priority(1),
                )
                .into_any_element(),
        )
    }

    /// Plan-mode indicator chip: visible while the session plans (read-only
    /// research + plan-file writes), so the state is never silent. Clicking
    /// it leaves plan mode — the escape hatch when a review card is missed
    /// or the model stalls in research.
    fn render_plan_chip(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.thread.read(cx).plan_mode() {
            return None;
        }
        Some(
            h_flex()
                .id("plan-mode-chip")
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .bg(theme.warning.opacity(0.12))
                .hover(|s| s.bg(theme.warning.opacity(0.22)))
                .cursor_pointer()
                .tooltip(move |window, cx| {
                    Tooltip::new(i18n::t("plan-chip-exit-tooltip")).build(window, cx)
                })
                .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                    this.set_thread_plan_mode(false, cx);
                    this.add_info_message(i18n::t("plan-mode-off-notice").to_string(), cx);
                }))
                .child(
                    Icon::new(IconName::LayoutDashboard)
                        .xsmall()
                        .text_color(theme.warning),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(i18n::t("plan-chip-label")),
                )
                .into_any_element(),
        )
    }

    /// Access chip + 3-tier approval popover.
    ///
    /// The chip is a mode-aware pill rendered next to the composer send button.
    /// Each `ApprovalMode` gets its own icon + accent color (green thumbs-up for
    /// blue bot for AutoPilot, red triangle for Danger) so the
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
                deferred(
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
                .with_priority(1),
            )
            .into_any_element()
    }

    /// Composer `+` button: attachment entry points (files / goal). Closed,
    /// the bare trigger; open, a PopupMenu anchored above it.
    fn render_plus_button(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let trigger = Button::new("composer-plus")
            .ghost()
            .xsmall()
            .icon(IconName::Plus)
            .tooltip(i18n::t("composer-add-label"))
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
                deferred(
                    gpui::div()
                        .id("plus-dropdown")
                        .absolute()
                        .bottom_full()
                        .left_0()
                        .occlude()
                        .child(menu),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    /// Build the `+` menu: "Files and folders" opens the native picker into
    /// pending attachments; "Goal" seeds the `/goal` slash command into the
    /// composer.
    fn open_plus_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let theme = cx.theme().clone();
        let ws = cx.entity().downgrade();
        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            let ws_files = ws.clone();
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
                move |window, cx| {
                    let _ = ws_goal.update(cx, |this, cx| {
                        this.close_plus_menu();
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

    /// Open the native file picker and add chosen paths as pending
    /// attachments (images render as chips + inline blocks, other files ride
    /// the attachment chip row).
    fn pick_files(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let result = paths.await;
            this.update(cx, |this, cx| {
                if let Ok(Ok(Some(paths))) = result {
                    for path in paths {
                        this.pending_attachments.push(PendingAttachment::new(path));
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn close_plus_menu(&mut self) {
        self.plus_open = false;
        self.plus_menu = None;
        self.plus_menu_sub = None;
    }

    /// Circular icon-only send/stop button.
    ///
    /// The composer's primary action control, reused across the hero and footer
    /// layouts. The box is pinned to `SEND_BTN_SIZE` so the icon, spinner, hover
    /// border, and disabled tint never perturb the composer row's geometry.
    ///
    /// States are kept visually disjoint: while a turn is running (and no
    /// plan/ask awaits input) the button is a stop control — Pause glyph, danger
    /// tint, always enabled so cancel stays reachable. When idle it is a send
    /// control — ArrowUp glyph, accent tint — and goes inert (`disabled`) the
    /// moment the composer has no text and no pending attachments, so an empty
    /// input never reads as a ready-to-fire primary. The follow-up queue is
    /// driven by Enter, not by this button, so running never disables stop.
    fn render_send_button(&self, running: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let disabled = !self.composer_can_submit(running, cx);

        // Matches the composer chip row height (px_2/py_1 + text_xs ≈ 20px),
        // so the send control shares the effort/model chips' rhythm instead of
        // towering over them. The disc corner radius is half the box => circle.
        const SEND_BTN_SIZE: Pixels = px(24.);
        const SEND_BTN_RADIUS: Pixels = px(12.);

        // Accent/danger-tinted transparent fills that strengthen on hover/active,
        // mirroring the chip family's accent.opacity(0.08) hover rather than a
        // heavy solid disc. Custom variant computes bg as color@~0.2, hover
        // color@~0.3, active color@~0.4; disabled falls back to color@0.15 +
        // muted_foreground@0.5 automatically.
        let variant = if running {
            ButtonCustomVariant::new(cx)
                .color(theme.danger)
                .foreground(theme.danger)
                .hover(theme.danger.opacity(0.18))
                .active(theme.danger.opacity(0.28))
        } else {
            ButtonCustomVariant::new(cx)
                .color(theme.accent)
                .foreground(theme.accent_foreground)
                .hover(theme.accent.opacity(0.18))
                .active(theme.accent.opacity(0.28))
        };

        Button::new("send-btn")
            .custom(variant)
            .with_size(Size::Size(SEND_BTN_SIZE))
            .rounded(SEND_BTN_RADIUS)
            .icon(if running {
                IconName::Pause
            } else {
                IconName::ArrowUp
            })
            .disabled(disabled)
            .on_click(cx.listener(|this, _, window, cx| {
                if this.thread.read(cx).is_running()
                    && this.pending_plan_review.is_none()
                    && this.pending_ask.is_none()
                {
                    this.cancel_turn(cx);
                } else {
                    this.submit_input(window, cx);
                }
            }))
            .into_any_element()
    }

    /// The completion popover overlaid above the composer while a trigger token
    /// (`/` or `@`) is active at the caret. Uses [`gpui::anchored`] (the same
    /// mechanism gpui-component's `Popover` and zed's completion menu use) so the
    /// popover escapes ancestor `overflow_hidden` clipping and avoids window-edge
    /// overflow — `div().absolute().bottom_full()` inside `deferred` does not
    /// position correctly and gets clipped by the body wrapper's `overflow_hidden`.
    /// A click on a row confirms it.
    fn render_completion_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let state = self.completion.as_ref()?;
        let theme = cx.theme().clone();
        let on_select = cx.listener(|this, ix: &usize, window, cx| {
            this.completion_confirm(*ix, window, cx);
        });
        let on_select: SelectHandler =
            std::rc::Rc::new(move |ix, window, cx| on_select(&ix, window, cx));
        Some(
            deferred(
                anchored()
                    .anchor(Anchor::BottomLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        gpui::div()
                            .id("completion-dropdown")
                            .occlude()
                            .child(render_completion(state, &theme, on_select)),
                    ),
            )
            .with_priority(1)
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

    /// The pi-harness project chip: bound-project indicator + dropdown with
    /// recent projects (store `known_projects` plus session-cwd backfill),
    /// blank-project creation and folder selection. Selection is only
    /// allowed on empty threads (same guard as the manox chip). Data source
    /// is the pi thread store only.
    fn render_project_chip_pi(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
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

                let ws = workspace.clone();
                let theme = cx.theme().clone();
                let ws_blank = ws.clone();
                let ws_folder = ws.clone();

                let menu = PopupMenu::build(window, cx, move |menu, _window, cx| {
                    let mut menu = menu.max_w(gpui::px(320.)).scrollable(true);
                    menu = menu.label(i18n::t("sidebar-section-projects"));

                    // Recent projects: registered folders first (newest
                    // first), then session cwds not yet registered.
                    let store = agent::thread_store_global();
                    let mut recent_projects: Vec<String> = Vec::new();
                    let mut seen = std::collections::HashSet::new();
                    {
                        let store_ref = store.read(cx);
                        for path in store_ref.known_projects().iter().rev() {
                            if seen.insert(path.clone()) {
                                recent_projects.push(path.clone());
                            }
                            if recent_projects.len() >= 20 {
                                break;
                            }
                        }
                        if recent_projects.len() < 20 {
                            for sum in store_ref.summaries() {
                                if sum.project.is_empty() || !seen.insert(sum.project.clone()) {
                                    continue;
                                }
                                recent_projects.push(sum.project.clone());
                                if recent_projects.len() >= 20 {
                                    break;
                                }
                            }
                        }
                    }

                    let ws_recent = ws.clone();
                    let theme_recent = theme.clone();
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
                                        this.thread
                                            .update(cx, |t, cx| t.set_project(p.clone(), cx));
                                        Self::register_project_in_store(&p, cx);
                                        cx.notify();
                                    });
                                },
                            ),
                        );
                    }

                    menu = menu.separator();
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

        let menu = self
            .project_chip_menu
            .clone()
            .expect("project_chip_menu exists when open");

        gpui::div()
            .relative()
            .child(trigger)
            .child(
                deferred(
                    gpui::div()
                        .id("project-chip-dropdown")
                        .absolute()
                        .bottom_full()
                        .right_0()
                        .occlude()
                        .child(menu),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    /// Register a bound project on the active variant's thread store so the
    /// sidebar keeps its folder (persisted; survives restarts and archives).
    fn register_project_in_store(path: &std::path::Path, cx: &mut Context<Self>) {
        let path = path.to_string_lossy().to_string();
        agent::thread_store_global().update(cx, |s, cx| s.register_project(path, cx));
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
        self.thread
            .update(cx, |t, cx| t.set_project(new_path.clone(), cx));
        Self::register_project_in_store(&new_path, cx);
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
                    this.thread
                        .update(cx, |t, cx| t.set_project(path.clone(), cx));
                    Self::register_project_in_store(&path, cx);
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
        if self.pending_ask.is_some() || self.pending_plan_review.is_some() {
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
                                        .text_color(theme.accent_foreground),
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
        self.render_manox(window, cx)
    }
}
impl Workspace {
    /// The full workspace chrome: sidebar, conversation column, context rail,
    /// right pane, approval overlays. Shared by both harness builds.
    fn render_manox(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !matches!(self.view_mode, ViewMode::Workspace) {
            self.drop_turn_navigator(cx);
        }
        // Settings reuses the shared shell (`sidebar | divider | main`) — the
        // same layout container as the app page, only with the settings nav in
        // the sidebar slot and the settings panel as the main column. The
        // underlying Workspace state (conversation sidebar, composer) is
        // preserved and returns unchanged when the user clicks "Back to app".
        if matches!(self.view_mode, ViewMode::Settings) {
            let settings = self
                .settings_view
                .as_ref()
                .expect("enter_settings must have created the SettingsView")
                .clone();
            let nav = settings.update(cx, |s, cx| s.render_nav(window, cx));
            let main = settings.update(cx, |s, cx| s.render_main(window, cx));
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
            let shell = self.shell_root(nav, main, cx);
            let anim_el = shell.with_animation(
                anim_id,
                Animation::new(Duration::from_millis(SLIDE_MS)).with_easing(ease_out_quint()),
                move |el, delta| {
                    let offset = panel_w * sign * (1.0 - delta);
                    el.relative().ml(offset)
                },
            );
            return h_flex().size_full().child(anim_el).into_any_element();
        }
        // Terminal pane: the shared shell (sidebar + draggable divider) with a
        // full-bleed terminal view as the main column. The terminal view owns
        // its PTY and grid; this branch only mounts it.
        // Resize/scrollback/selection are handled inside `TerminalView` /
        // `TerminalElement`.
        if matches!(self.view_mode, ViewMode::Terminal) {
            let title_text: SharedString = self
                .thread
                .read(cx)
                .project()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                .unwrap_or("manox")
                .to_string()
                .into();
            let terminal = self
                .terminal_view
                .clone()
                .expect("view_mode == Terminal implies terminal_view is set");
            let icon = Icon::new(IconName::SquareTerminal)
                .small()
                .into_any_element();
            return self
                .shell_root(
                    self.sidebar.clone(),
                    self.render_terminal_column(icon, title_text, terminal),
                    cx,
                )
                .on_action(
                    cx.listener(|this, _: &crate::ToggleCockpitTasks, _window, cx| {
                        this.context_rail.update(cx, |r, cx| {
                            r.cockpit_hide_tasks = !r.cockpit_hide_tasks;
                            cx.notify();
                        });
                        cx.notify();
                    }),
                )
                .into_any_element();
        }
        // External agent CLI session: render the active session's terminal TUI
        // in place of the conversation. Same shared shell as the conversation
        // and terminal views — only the main column (the agent's TUI) and the
        // title differ, so the sidebar divider stays draggable here too. The
        // bar title is the agent's OSC title (mirrored from
        // `TerminalEvent::Title`), falling back to the kind label ("Claude
        // Code" / "Codex" / "GitHub Copilot") until the TUI sets its own. The
        // provider/model picked at spawn is intentionally omitted: the user
        // can switch models mid-session inside the TUI (`/model`), and manox
        // cannot observe that change.
        if matches!(self.view_mode, ViewMode::ExternalSession) {
            let active = self
                .active_external
                .as_deref()
                .and_then(|id| self.external_sessions.iter().find(|s| s.id == id));
            if let Some(session) = active {
                let kind = session.kind;
                // Titlebar + sidebar share `display_title()` so a TUI rename
                // (OSC title) updates both at once.
                let title: SharedString = session.display_title();
                let terminal = session.terminal_view.clone();
                let icon = gpui::svg()
                    .path(kind.icon_asset())
                    .size(px(16.))
                    .text_color(cx.theme().muted_foreground)
                    .into_any_element();
                return self
                    .shell_root(
                        self.sidebar.clone(),
                        self.render_terminal_column(icon, title, terminal),
                        cx,
                    )
                    .into_any_element();
            }
            // No live session matches the recorded id (closed underneath us).
            // Fall back to the conversation pane: flip the mode and fall
            // through to the Workspace branch below, so this frame renders the
            // full shell (sidebar + divider + conversation) rather than a
            // sidebar-only stub that skips the divider and the mode-switching
            // actions.
            self.view_mode = ViewMode::Workspace;
            cx.notify();
        }
        let theme = cx.theme().clone();
        let running = self.thread.read(cx).is_running();

        self.ensure_blank_project_input(window, cx);

        if self.blocking_overlay_active() && self.turn_navigator.is_some() {
            self.close_turn_navigator(window, cx);
        }

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
        // Restoring history keeps the composer mounted so the user can draft
        // immediately, while submission remains gated until the transcript is
        // authoritative.
        let first_screen = self.conversation.read(cx).is_empty(cx) && !running;
        let loading = self.thread.read(cx).history_phase().is_loading();
        let composer_placement = composer_placement(editor_open, first_screen);
        let main_body_w = window.bounds().size.width
            - self.sidebar_width
            - px(SIDEBAR_DIVIDER_WIDTH)
            - if right_pane_open {
                editor_width + px(EDITOR_DIVIDER_WIDTH)
            } else {
                px(0.)
            };
        let show_rail = !first_screen
            && !editor_open
            && self.thread.read(cx).has_interacted()
            && crate::views::context_rail::ContextRail::rail_width_for(main_body_w).is_some();
        let overlay = self.render_blank_project_overlay(window, &theme, cx);
        let turn_navigator_overlay =
            self.render_turn_navigator_overlay(window, &theme, right_pane_open, show_rail, cx);
        // The inline composer stays visible while inline AskUserQuestion cards
        // are open; submitting text resolves the ask as a free-form response.
        // The editor pane still hides the inline composer while editing there.
        let footer = (composer_placement == ComposerPlacement::Footer).then(|| {
            v_flex()
                .w_full()
                .flex_shrink_0()
                .bg(theme.background)
                .py_2()
                .gap_2()
                .child(centered(gpui::div().w_full().h(px(1.)).bg(theme.border)))
                .children(self.render_attachments(&theme, cx))
                .child(centered(self.render_composer(running, window, &theme, cx)))
        });

        // Hero occupies the message-list region on the first screen.
        // Notice items on the first screen (e.g. Danger toggle acknowledgement).
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
        let hero = if composer_placement != ComposerPlacement::Hero {
            None
        } else if loading {
            // History restore and drafting are independent: the progress label
            // describes the transcript while the disabled send action makes the
            // input gate explicit without delaying the editor itself.
            Some(
                v_flex()
                    .flex_1()
                    .w_full()
                    .justify_center()
                    .items_center()
                    .child(centered(
                        v_flex()
                            .w_full()
                            .gap_3()
                            .items_center()
                            .child(
                                crate::views::braille_spinner::BrailleSpinner::new()
                                    .color(theme.muted_foreground),
                            )
                            .child(
                                gpui::div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(i18n::t("workspace-loading-history")),
                            )
                            .children(self.render_attachments(&theme, cx))
                            .child(self.render_composer(running, window, &theme, cx)),
                    )),
            )
        } else {
            Some(
                v_flex()
                    .flex_1()
                    .w_full()
                    .justify_center()
                    .items_center()
                    .child(centered(
                        v_flex()
                            .w_full()
                            .gap_5()
                            .items_center()
                            .child(
                                gpui::div()
                                    .text_base()
                                    .font_weight(gpui::FontWeight::BLACK)
                                    .text_color(theme.foreground)
                                    .child(i18n::t("workspace-empty-prompt")),
                            )
                            .children(self.render_attachments(&theme, cx))
                            .child(self.render_composer(running, window, &theme, cx))
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
                    )),
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
        // The sidebar divider lives in `shell_root` — shared by every view
        // mode so the sidebar resizes identically everywhere.
        // Right pane is a peer tab container for the editor, member/sub-agent
        // observers, browser views, and plan preview. The top-level TabBar is
        // built from `right_tabs`; the content below dispatches on the active
        // tab.
        let active_tab = self.right_tabs.get(self.active_right_tab).cloned();
        let right_tab_children: Vec<Tab> = self
            .right_tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                let base = match tab {
                    RightTab::Editor => Tab::new().label(i18n::t("member-editor-tab")),
                    RightTab::Member(name) => {
                        Tab::new().label(i18n::t_str("member-tab", &[("name", name.as_str())]))
                    }
                    RightTab::Browser(id) => {
                        let url = self
                            .browser_views
                            .get(id)
                            .map(|v| v.read(cx).url().to_string())
                            .unwrap_or_default();
                        Tab::new().label(i18n::t_str("browser-tab", &[("url", &url)]))
                    }
                    RightTab::Subagent(id) => {
                        let title = self
                            .subagent_panels
                            .get(id)
                            .map(|p| p.read(cx).title().to_string())
                            .unwrap_or_default();
                        Tab::new().label(title)
                    }
                };
                // Every observational/preview tab carries a close affordance;
                // the Editor tab keeps its keyboard toggle (`ToggleEditor` /
                // `CloseEditor`).
                match tab {
                    RightTab::Browser(_) | RightTab::Member(_) | RightTab::Subagent(_) => base
                        .suffix(
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
                            .child(if editor_preview {
                                // The preview entity is lazily created and kept stable
                                // across renders so the source is only re-parsed when
                                // the draft changes. The scroll lives on an explicit
                                // `ScrollHandle` + an outer `flex_1`-sized container
                                // — the message-list pattern — rather than the
                                // markdown entity's own `overflow_y_scroll`: an explicit
                                // handle keeps the offset pinned and defaulting to the
                                // top, and a flex-resolved (not `h_full`-percentage)
                                // scroll box reliably clips long content instead of
                                // letting it overflow and lose the first lines off the
                                // top.
                                let value = self.editor_state.read(cx).value().to_string();
                                let theme = cx.theme().clone();
                                if self.editor_preview_md.is_none() {
                                    self.editor_preview_md = Some(cx.new(|_cx| {
                                        Markdown::new("editor-preview", value.clone())
                                            .theme(&theme)
                                            .heading_mode(HeadingMode::Uniform)
                                            .body_size(crate::views::message::MESSAGE_BODY_SIZE)
                                    }));
                                }
                                let md = self
                                    .editor_preview_md
                                    .clone()
                                    .expect("preview md initialized above");
                                if md.read(cx).source() != value.as_str() {
                                    md.update(cx, |m, cx| m.replace(value, cx));
                                }
                                let scroll = self.editor_preview_scroll.clone();
                                gpui::div()
                                    .id("editor-preview-scroll")
                                    .w_full()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .track_scroll(&scroll)
                                    .child(gpui::div().w_full().p_4().child(md.into_any_element()))
                                    .into_any_element()
                            } else {
                                gpui::div()
                                    .w_full()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_hidden()
                                    .child(
                                        // The panel editor is a plain-text
                                        // composer for the same message
                                        // content, so it shares the inline
                                        // input's body typeface: Lilex Light
                                        // at MESSAGE_BODY_SIZE (13px).
                                        Input::new(&self.editor_state)
                                            .size_full()
                                            .appearance(false)
                                            .font_family(theme.mono_font_family.clone())
                                            .font_weight(gpui::FontWeight::LIGHT)
                                            .text_size(crate::views::message::MESSAGE_BODY_SIZE)
                                            .into_any_element(),
                                    )
                                    .into_any_element()
                            })
                            .into_any_element(),
                        Some(RightTab::Browser(id)) => self
                            .browser_views
                            .get(&id)
                            .map(|v| v.clone().into_any_element())
                            .unwrap_or_else(|| gpui::div().into_any_element()),
                        Some(RightTab::Member(name)) => self
                            .member_panels
                            .get(&name)
                            .map(|p| p.clone().into_any_element())
                            .unwrap_or_else(|| gpui::div().into_any_element()),
                        Some(RightTab::Subagent(id)) => self
                            .subagent_panels
                            .get(&id)
                            .map(|p| p.clone().into_any_element())
                            .unwrap_or_else(|| gpui::div().into_any_element()),
                        None => gpui::div().into_any_element(),
                    }),
            );

        // The shared shell provides the sidebar + draggable divider and the
        // mode-switching actions; this mode chains the conversation-only
        // actions and the turn-navigator overlay onto it. The shell's main
        // slot is the main view: a two-column container holding the message
        // column (conversation + rail) and, when any right-pane tab is open,
        // the right side view (editor / browser tabs).
        // Bind the column to a local before the shell call: the column's
        // builder borrows `self` (title-menu trigger, context rail), which
        // would collide with `shell_root`'s `&mut self` receiver inside a
        // single call expression.
        self.sync_ask_card_snapshots(cx);
        let conversation_column = {
            v_flex()
                .flex_1()
                .h_full()
                .min_w_0()
                .relative()
                .overflow_hidden()
                .child({
                    // Body wrapper: hero / list / footer / overlay. `pt`
                    // reserves space for the title-bar overlay; `pr` (when
                    // the card is shown) reserves the floating card's width
                    // so the message list never hides behind it.
                    v_flex()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .w_full()
                        .overflow_hidden()
                        .pt(TITLE_BAR_HEIGHT)
                        .pb_2()
                        .when(show_rail, |this| {
                            this.pr(px(crate::views::context_rail::ENV_CONTENT_INSET))
                        })
                        // Empty first screen shows the centered hero in place
                        // of the (empty) message list; otherwise a bottom-
                        // anchored, tail-following native list.
                        .children(hero)
                        .children({
                            // Keep the row factory a pure read-only projection.
                            // GPUI invokes it while measuring and prepainting;
                            // mutating a MessageItem here invalidates the same
                            // entity tree whose height is being cached.
                            let conversation = self.conversation.clone();
                            let processor = move |ix: usize, _window: &mut Window, cx: &mut App| {
                                let item = conversation.read(cx).items().get(ix).cloned();
                                match item {
                                    // `flex_shrink_0` guards against any
                                    // available height leaking down the
                                    // flex chain and compressing a row.
                                    Some(item) => v_flex()
                                        .w_full()
                                        .pt_1()
                                        .pb_4()
                                        .flex_shrink_0()
                                        .min_w_0()
                                        .child(item)
                                        .into_any_element(),
                                    // Index out of range mid-splice (count
                                    // changed between a layout pass and the
                                    // render closure): render an empty row.
                                    None => gpui::div().into_any_element(),
                                }
                            };
                            let list_state = self.list_state.clone();
                            let width_state = self.list_state.clone();
                            let message_list_width = self.message_list_width.clone();
                            let mono_family = theme.mono_font_family.clone();
                            (!first_screen).then(move || {
                                // Native `gpui::list`: it owns virtualization,
                                // scroll, the per-item height cache, and tail-
                                // follow. Visible rows re-measure every frame;
                                // the wrapper below explicitly invalidates all
                                // cached heights after a width change. `Tail`
                                // mode pins to the live end and
                                // re-engages at the bottom after an upward
                                // scroll. Item heights are reconciled from the
                                // ThreadEvent handler via
                                // `splice`/`remeasure_items`.
                                let list_el = gpui::list(list_state, processor)
                                    .w_full()
                                    .h_full()
                                    .min_h_0()
                                    .min_w_0();
                                // Body typeface: Lilex Light. Every message row
                                // (assistant, user, reasoning, tool cards, notices)
                                // inherits from this wrapper div: gpui's List applies
                                // its own text refinements only while requesting its
                                // own layout, and with `Auto` sizing the item rows are
                                // laid out in prepaint outside that scope — so the
                                // family/weight must live on a wrapping div. Markdown
                                // bold/headings resolve to Medium via nearest-weight,
                                // italic syntax and tool-card overrides hit the
                                // italic cuts.
                                let list_wrap = v_flex()
                                    .flex_1()
                                    .h_full()
                                    .min_h_0()
                                    .min_w_0()
                                    .font_family(mono_family.clone())
                                    .font_weight(gpui::FontWeight::LIGHT)
                                    .child(list_el)
                                    .on_prepaint(move |bounds, window, _app| {
                                        if message_list_width
                                            .update(bounds.size.width, &width_state)
                                        {
                                            window.refresh();
                                        }
                                    });
                                h_flex()
                                    .flex_1()
                                    .w_full()
                                    .min_h_0()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .child(list_wrap)
                            })
                        })
                        .children(footer)
                        // Approval overlay (if any)
                        .children(overlay)
                })
                // Title-bar overlay: absolute top of the conversation column,
                // painted after the body so the "..." menu isn't covered by
                // the conversation list.
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
                                        .pr_4()
                                        .child(
                                            gpui::svg()
                                                .path("icons/manox.svg")
                                                .size(px(16.))
                                                .text_color(theme.muted_foreground),
                                        )
                                        .child(
                                            gpui::div()
                                                .text_sm()
                                                .text_left()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .child(title_text),
                                        ),
                                )
                                .child(h_flex()),
                        ),
                )
                // Floating context card: absolute top-right of the
                // conversation column, below the title bar. Its own `Render`
                // positions it (`top` clears the title bar, `right` + the
                // body wrapper's `pr` keep the message list clear). Hidden
                // while the editor pane is open, on the first screen, before
                // the thread interacts, or below the narrow width gate.
                .when(show_rail, |this| this.child(self.context_rail.clone()))
        };
        // The main view is the shell's main slot: the message column plus the
        // right side view (editor / browser tabs) as its sub-columns. Nesting
        // the right pane inside the main view keeps the shell uniformly
        // `sidebar | divider | main view` across every view mode (Terminal /
        // ExternalSession / Settings pass a single-column main).
        let main_view = h_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .relative()
            .child(conversation_column)
            .when(right_pane_open, |this| {
                this.child(editor_divider).child(editor_pane)
            });
        let mut root = self.shell_root(self.sidebar.clone(), main_view, cx);
        root = root
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
            .on_action(cx.listener(|this, _: &ToggleTurnNavigator, window, cx| {
                this.toggle_turn_navigator(window, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &OpenBrowserTab, window, cx| {
                this.open_browser_tab(crate::views::browser_view::DEFAULT_URL, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseBrowserTab, _window, cx| {
                this.close_active_browser_tab(cx);
            }))
            .on_action(
                cx.listener(|this, _: &crate::BackgroundCurrentThread, window, cx| {
                    this.background_current_thread(window, cx);
                }),
            )
            .on_action(cx.listener(|this, _: &crate::UndoLastQueued, _window, cx| {
                this.undo_last_queued(cx);
            }))
            // Completion actions only match via the `completion == open > Input`
            // keybindings, so any fire means the popover was open and these
            // keystrokes belong to it. Stop propagation so the Input's own
            // parallel up/down/enter/tab/escape binding (same depth, lower
            // register index) doesn't also fire — otherwise Enter would both
            // confirm and submit, Up/Down would move caret and selection, etc.
            .on_action(cx.listener(|this, _: &crate::CompletionUp, window, cx| {
                this.completion_up(window, cx);
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|this, _: &crate::CompletionDown, window, cx| {
                this.completion_down(window, cx);
                cx.stop_propagation();
            }))
            .on_action(
                cx.listener(|this, _: &crate::CompletionConfirm, window, cx| {
                    this.completion_confirm_selected(window, cx);
                    cx.stop_propagation();
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::CompletionDismiss, _window, cx| {
                    this.close_completion(cx);
                    cx.stop_propagation();
                }),
            )
            // Composer history recall: fires only via the `composer = recall
            // > Input` bindings (the wrapper sets that context exactly when
            // the completion context is absent). The handlers stop
            // propagation only when they act, so a deferred key falls through
            // to the Input's native MoveUp/MoveDown.
            .on_action(
                cx.listener(|this, _: &crate::ComposerRecallUp, window, cx| {
                    this.composer_recall_up(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::ComposerRecallDown, window, cx| {
                    this.composer_recall_down(window, cx);
                }),
            )
            .on_action(
                cx.listener(|this, _: &crate::ArchiveCurrentThread, window, cx| {
                    this.archive_current_thread(window, cx);
                }),
            )
            // The right editor pane moved inside the shell's main view (the
            // `main_view` container above); it is no longer a top-level shell
            // column.
            .children(turn_navigator_overlay)
            .on_drag_move(cx.listener(
                |this, e: &DragMoveEvent<DraggedEditorDivider>, _window, cx| {
                    // The root fills the window, so its right edge is the
                    // window's right edge and the editor pane's width is the
                    // distance from the cursor to that edge. Clamp both to a
                    // minimum and to leave the message column at least
                    // `MAIN_MIN_WIDTH` (sidebar + divider + main view sit
                    // left of the editor), so dragging wide never overflows
                    // the window or collapses the conversation column. The
                    // context card is hidden while the editor is open, so it
                    // does not claim a width here — the conversation alone
                    // holds the message column. `sidebar_width` is read live
                    // so a wide sidebar correctly shrinks the available
                    // editor envelope.
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
            ));
        root.into_any_element()
    }
    /// The shared window shell every full-window `ViewMode` renders through:
    /// `sidebar | sidebar-divider | main`, plus the mode-switching actions and
    /// the sidebar drag/reset handling. The divider (drag handle, double-click
    /// reset, width clamp + sync) lives here once, so the conversation,
    /// built-in terminal, external-session, and Settings pages all resize
    /// their sidebar identically — only the sidebar slot and main column
    /// differ per mode. The Settings page passes its own nav as the sidebar
    /// slot; dragging the divider there updates the same width state so the
    /// layout container behaves identically across pages.
    fn shell_root(
        &mut self,
        sidebar: impl IntoElement,
        main: impl IntoElement,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = cx.theme().clone();
        // The divider is the visual separator and the drag handle for resizing
        // the sidebar. Double-click resets to the default `SIDEBAR_WIDTH` for
        // symmetry with the editor pane.
        let sync_width = |this: &mut Self, cx: &mut App, width: Pixels| {
            this.sidebar_width = width;
            this.sidebar.update(cx, |s, cx| s.set_width(width, cx));
            if let Some(settings) = this.settings_view.as_ref() {
                settings.update(cx, |s, cx| s.set_width(width, cx));
            }
        };
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
                cx.listener(move |this, e: &MouseUpEvent, _, cx| {
                    if e.click_count >= 2 {
                        sync_width(this, cx, px(SIDEBAR_WIDTH));
                        cx.notify();
                    }
                }),
            );

        h_flex()
            .size_full()
            .relative()
            .bg(theme.background)
            .text_color(theme.foreground)
            // Mode-switching shortcuts apply in every view mode.
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
            .child(sidebar)
            .child(sidebar_divider)
            .child(main)
            .on_drag_move(cx.listener(
                move |this, e: &DragMoveEvent<DraggedSidebarDivider>, _window, cx| {
                    // The root fills the window, so the sidebar's right edge is
                    // the cursor's x position relative to the root's left.
                    // Clamp so the message column (and the editor pane when
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
                    sync_width(this, cx, clamped);
                    cx.notify();
                },
            ))
    }

    /// The terminal-style main column shared by the built-in Terminal tab and
    /// external agent CLI sessions: a TitleBar (leading icon + title) over a
    /// full-bleed terminal view. One shape for both, so the two terminal
    /// surfaces read as peers inside the shared shell.
    fn render_terminal_column(
        &self,
        icon: AnyElement,
        title: SharedString,
        content: impl IntoElement,
    ) -> gpui::Div {
        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .relative()
            .child(
                TitleBar::new().child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .flex_1()
                        .min_w_0()
                        .child(icon)
                        .child(
                            gpui::div()
                                .text_sm()
                                .text_left()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .child(title),
                        ),
                ),
            )
            .child(v_flex().flex_1().h_full().w_full().child(content))
    }
}

/// Parse an `AskUserQuestion` tool input into a `PendingAsk`. The per-question
/// `InputState` entities are allocated lazily on first render (they need a
/// `Window`, which the event handler lacks). Returns `None` when the input is
/// malformed (the generic approval overlay then takes over as a fallback).
/// Snapshot a thread's working directory as a `SharedString` for the
/// `TerminalPanel` prompt line. Reads the `Thread` entity (not the `Workspace`)
/// so it stays safe inside a `Workspace::update` closure, where reading the
/// `Workspace` itself would double-lease. `None` only when the path is empty.
/// One label/value row of the goal status popover.
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
                .flex_1()
                .text_xs()
                .text_color(fg)
                .child(value.to_string()),
        )
}

fn thread_cwd(thread: &Entity<ThreadEntity>, cx: &App) -> Option<SharedString> {
    let cwd = thread.read(cx).cwd();
    if cwd.as_os_str().is_empty() {
        None
    } else {
        Some(SharedString::from(cwd.to_string_lossy().to_string()))
    }
}

fn parse_pending_ask(id: String, input: serde_json::Value) -> Option<PendingAsk> {
    let questions = input.get("questions")?.as_array()?;
    // Out-of-range counts violate the tool contract. No card is shown for
    // such input; the pending question resolves only when the turn is
    // cancelled.
    if !(1..=3).contains(&questions.len()) {
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
                let raw_label = o
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let description = o
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let explicit_recommended = o
                    .get("recommended")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let (label, suffix_recommended) = strip_recommended_suffix(raw_label);
                opts.push(AskOption {
                    label,
                    description,
                    recommended: explicit_recommended || suffix_recommended,
                });
            }
        }
        if !(2..=3).contains(&opts.len()) {
            return None;
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
    })
}

fn strip_recommended_suffix(label: String) -> (String, bool) {
    let lower = label.to_lowercase();
    for suffix in [" (Recommended)", "（推荐）", " (推荐)", "（Recommended）"] {
        let suffix_lower = suffix.to_lowercase();
        if lower.ends_with(&suffix_lower) {
            let stripped = &label[..label.len() - suffix.len()];
            return (stripped.trim().to_string(), true);
        }
    }
    (label, false)
}

/// Map an `ApprovalMode` to the chip's (label, accent color, icon) triple.
///
/// Colors are theme tokens, not raw hsla values, so the chip follows the
/// active theme (light/dark) without bespoke palettes per mode. The
/// AutoPilot accent uses `info` (green) as a "this is the safe default"
/// signal — staying gray would be visually identical to a disabled state.
fn mode_chip_visual(mode: ApprovalMode, theme: &Theme) -> (SharedString, gpui::Hsla, IconName) {
    match mode {
        ApprovalMode::AutoPilot => (
            i18n::t("workspace-chip-mode-autopilot"),
            theme.info,
            IconName::Bot,
        ),
        ApprovalMode::Danger => (
            i18n::t("workspace-chip-mode-danger"),
            theme.danger,
            IconName::TriangleAlert,
        ),
    }
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
            ApprovalMode::AutoPilot,
            i18n::t("workspace-mode-autopilot-title"),
            i18n::t("workspace-mode-autopilot-desc"),
            IconName::Bot,
            info,
            current == ApprovalMode::AutoPilot,
        ))
        .child(make_row(
            ApprovalMode::Danger,
            i18n::t("workspace-mode-danger-title"),
            i18n::t("workspace-mode-danger-desc"),
            IconName::TriangleAlert,
            danger,
            current == ApprovalMode::Danger,
        ))
}

impl Workspace {
    /// Toggle Danger mode on the current thread (`/danger` no-args form):
    /// Danger → AutoPilot, anything else → Danger. The mode change notice
    /// rides `apply_approval_mode` so the conversation shows the switch.
    pub(crate) fn toggle_danger(&mut self, cx: &mut Context<Self>) {
        let next = if self.thread.read(cx).approval_mode() == ApprovalMode::Danger {
            ApprovalMode::AutoPilot
        } else {
            ApprovalMode::Danger
        };
        self.apply_approval_mode(next, cx);
    }

    /// Enable Danger mode (if not already on) and immediately send `prompt`
    /// as a user turn — the `/danger [prompt]` form. Slash dispatch only
    /// fires while idle (the submit gate); `/danger Y` typed mid-turn parks
    /// in the follow-up queue as raw text like any other message.
    pub(crate) fn start_danger_turn(&mut self, prompt: String, cx: &mut Context<Self>) {
        if self.thread.read(cx).approval_mode() != ApprovalMode::Danger {
            self.apply_approval_mode(ApprovalMode::Danger, cx);
        }
        self.send_user_turn(prompt, Vec::new(), cx);
    }

    /// Switch the thread's `ApprovalMode`, post a localized notice, and close
    /// the popover. Centralized so slash command, chip click, and the
    /// future settings-panel wiring all funnel through one path.
    pub(crate) fn apply_approval_mode(&mut self, mode: ApprovalMode, cx: &mut Context<Self>) {
        let mode_key = match mode {
            ApprovalMode::AutoPilot => "autopilot",
            ApprovalMode::Danger => "danger",
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

/// Cap a queued follow-up's text for the compact queue row so long pastes
/// don't blow out the composer chrome. Trailing whitespace is trimmed and an
/// ellipsis marks a truncation.
fn truncate_follow_up(s: &str) -> String {
    const MAX: usize = 80;
    let s = s.trim();
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let mut t: String = s.chars().take(MAX).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::{
        ComposerPlacement, RecallDirection, RecallStep, Workspace, composer_placement,
        editor_can_submit,
    };

    fn step(
        direction: RecallDirection,
        value: &str,
        index: i64,
        turns: &[&str],
    ) -> (i64, RecallStep) {
        let owned: Vec<String> = turns.iter().map(|t| t.to_string()).collect();
        Workspace::recall_step(direction, value, index, &owned)
    }

    fn assert_recall(step: (i64, RecallStep), index: i64, text: &str) {
        assert_eq!(step.0, index);
        match step.1 {
            RecallStep::Recall(t) => assert_eq!(t, text),
            other => panic!("expected Recall, got {other:?}"),
        }
    }

    #[test]
    fn recall_up_from_empty_starts_at_the_newest_turn() {
        let turns = ["newest", "oldest"];
        assert_recall(step(RecallDirection::Up, "", -1, &turns), 0, "newest");
    }

    #[test]
    fn recall_up_walks_further_back_and_clamps_at_the_oldest() {
        let turns = ["newest", "middle", "oldest"];
        assert_recall(step(RecallDirection::Up, "newest", 0, &turns), 1, "middle");
        assert_recall(step(RecallDirection::Up, "middle", 1, &turns), 2, "oldest");
        let at_oldest = step(RecallDirection::Up, "oldest", 2, &turns);
        assert_eq!(at_oldest.0, 2);
        assert!(matches!(at_oldest.1, RecallStep::None));
    }

    #[test]
    fn recall_up_with_typed_text_defers_to_native_caret() {
        let turns = ["newest"];
        let out = step(RecallDirection::Up, "typed draft", -1, &turns);
        assert_eq!(out.0, -1);
        assert!(matches!(out.1, RecallStep::None));
    }

    #[test]
    fn recall_up_with_empty_history_defers() {
        let out = step(RecallDirection::Up, "", -1, &[]);
        assert_eq!(out.0, -1);
        assert!(matches!(out.1, RecallStep::None));
    }

    #[test]
    fn recall_down_walks_toward_newer_and_clears_at_the_newest() {
        let turns = ["newest", "middle", "oldest"];
        assert_recall(
            step(RecallDirection::Down, "oldest", 2, &turns),
            1,
            "middle",
        );
        assert_recall(
            step(RecallDirection::Down, "middle", 1, &turns),
            0,
            "newest",
        );
        let at_newest = step(RecallDirection::Down, "newest", 0, &turns);
        assert_eq!(at_newest.0, -1);
        assert!(matches!(at_newest.1, RecallStep::Clear));
    }

    #[test]
    fn recall_down_outside_recall_defers_to_native_caret() {
        let turns = ["newest"];
        let out = step(RecallDirection::Down, "", -1, &turns);
        assert_eq!(out.0, -1);
        assert!(matches!(out.1, RecallStep::None));
    }

    #[test]
    fn recall_state_is_derived_from_the_value_matching_the_recalled_text() {
        let turns = ["newest", "oldest"];
        // An edit after recall makes the value diverge: treated as fresh.
        let out = step(RecallDirection::Up, "newest edited", 0, &turns);
        assert_eq!(out.0, -1);
        assert!(matches!(out.1, RecallStep::None));
        // A stale index from another thread behaves like a fresh recall.
        assert_recall(step(RecallDirection::Up, "", 5, &turns), 0, "newest");
    }

    #[test]
    fn history_restore_keeps_composer_mounted() {
        assert_eq!(composer_placement(false, true), ComposerPlacement::Hero);
        assert_eq!(composer_placement(false, false), ComposerPlacement::Footer);
    }

    #[test]
    fn editor_remains_the_only_composer_exclusion() {
        assert_eq!(composer_placement(true, true), ComposerPlacement::Hidden);
        assert_eq!(composer_placement(true, false), ComposerPlacement::Hidden);
    }

    #[test]
    fn editor_submission_waits_for_authoritative_history() {
        assert!(!editor_can_submit(true, false, false, "draft"));
        assert!(editor_can_submit(false, false, false, "draft"));
        assert!(!editor_can_submit(false, true, false, "draft"));
        assert!(!editor_can_submit(false, false, true, "draft"));
        assert!(!editor_can_submit(false, false, false, "   "));
    }
}
