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
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use agent::language_model::StopReason;
use agent::provider::WireApi;
use agent::provider::registry;
use agent::settings;
use agent::thread::ApprovalMode;
use agent::webview_host::BrowserTabId;
use agent::{
    ModeKind, PermissionDecision, PlanReviewChoice, ReasoningEffort, Thread, ThreadEvent, ThreadId,
    i18n, save_thread,
};
use gpui::{
    Anchor, Animation, AnimationExt as _, AnyElement, App, ClickEvent, Context, CursorStyle,
    DismissEvent, DragMoveEvent, Entity, FocusHandle, MouseButton, MouseUpEvent, Pixels, Render,
    ScrollHandle, SharedString, Subscription, WeakEntity, Window, anchored, deferred,
    ease_out_quint, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Size, StyledExt as _,
    TITLE_BAR_HEIGHT, Theme, TitleBar, WindowExt as _,
    button::{Button, ButtonCustomVariant, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState, Paste, RopeExt},
    menu::{PopupMenu, PopupMenuItem},
    notification::Notification,
    tab::{Tab, TabBar},
    tag::{Tag, TagVariant},
    v_flex,
};
use manox_components::markdown::{HeadingMode, Markdown};

use crate::cockpit::CockpitPhase;
use crate::conversation::{ConvItem, ConversationState, UserImage, UserTurnMeta};
use crate::external_session::{ExternalSession, SessionKind};
use crate::views::browser_view::BrowserView;
use crate::views::centered;
use crate::views::completion::{
    CompletionState, SelectHandler, build_replacement, detect, mention_source, render_completion,
    slash_source,
};
use crate::views::composer_menu::{
    PendingAttachment, build_plus_menu, load_attachment, render_attachment_chips,
};
use crate::views::member_panel::MemberPanel;
use crate::views::popup_menu;
use crate::views::settings::{SettingsEvent, SettingsView};
use crate::views::sidebar::{Sidebar, SidebarEvent};
use crate::views::subagent_panel::{
    SubagentInfo, SubagentPanel, SubagentSnapshot, snapshots_from_messages, subagent_display_title,
};
use crate::views::turn_navigator::{TurnNavigator, TurnNavigatorEvent, collect_user_turns};
use crate::{
    CloseBrowserTab, CloseTerminalTab, FocusConversation, FocusTerminal, NewTerminalTab,
    OpenBrowserTab, OpenSettings, ToggleTurnNavigator,
};
use terminal::Terminal;
use terminal_ui::TerminalView;

/// A tab in the right observation pane. `Editor` is the markdown composer
/// (Write/Preview); `Member(name)` is a read-only [`MemberPanel`] over a team
/// worker's conversation + tasks; `Subagent(id)` is a read-only
/// [`SubagentPanel`]; `Browser(id)` is an untrusted embedded webview (see
/// [`BrowserView`]).
#[derive(Clone, Debug)]
enum RightTab {
    Editor,
    Member(String),
    Subagent(String),
    Browser(BrowserTabId),
    PlanPreview,
}

fn ensure_subagent_tab(tabs: &mut Vec<RightTab>, id: &str) -> usize {
    if let Some(ix) = tabs
        .iter()
        .position(|tab| matches!(tab, RightTab::Subagent(open_id) if open_id == id))
    {
        return ix;
    }
    tabs.push(RightTab::Subagent(id.to_string()));
    tabs.len() - 1
}

fn remove_subagent_tabs(tabs: &mut Vec<RightTab>, active: &mut usize) {
    let mut ix = 0;
    while ix < tabs.len() {
        if matches!(tabs.get(ix), Some(RightTab::Subagent(_))) {
            tabs.remove(ix);
            if *active > ix {
                *active -= 1;
            } else if *active >= tabs.len() {
                *active = tabs.len().saturating_sub(1);
            }
        } else {
            ix += 1;
        }
    }
}

struct SubagentRecord {
    info: SubagentInfo,
    panel: Entity<SubagentPanel>,
}

#[derive(Default)]
struct SubagentSession {
    records: BTreeMap<String, SubagentRecord>,
    order: Vec<String>,
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

/// A completed `<proposed_plan>` block awaiting the user's three-way review
/// verdict. Carries the plan text for the overlay body and (on an implement
/// verdict) for re-injection as the implement turn's seed.
struct PendingPlanReview {
    plan_text: String,
}

/// A pending inbound-write request from a built-in browser tab, surfaced by
/// `ThreadEvent::InboundAuthorization`. Unlike outbound tool approval this
/// axis is `ApprovalMode`-blind — a web page must never gain a write path
/// because the agent runs in Yolo — so the overlay always shows and the
/// decision always routes through `Thread::respond_inbound`, not the outbound
/// approval pipeline.
struct PendingInbound {
    id: String,
    intent: String,
}

/// A parsed `AskUserQuestion` prompt awaiting the user's selections.
struct PendingAsk {
    id: String,
    questions: Vec<AskQuestion>,
    /// Per-question toggled option flags, aligned with `questions[i].options`.
    selections: Vec<Vec<bool>>,
}

#[derive(Clone)]
pub(crate) struct AskCardSnapshot {
    pub id: String,
    pub step: usize,
    pub total: usize,
    pub transition_gen: u64,
    pub question: AskCardQuestion,
    pub selections: Vec<bool>,
}

#[derive(Clone)]
pub(crate) struct AskCardQuestion {
    pub question: String,
    pub header: String,
    pub multi_select: bool,
    pub options: Vec<AskCardOption>,
}

#[derive(Clone)]
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

struct DeferredUserTurn {
    text: String,
    images: Vec<agent::language_model::MessageContent>,
    meta: UserTurnMeta,
    ui: agent::MessageUiMetadata,
    user_images: Vec<UserImage>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PendingMessageReveal {
    generation: u64,
    item_ix: usize,
}

/// A thread parked in the background while still running a turn. The held
/// `Subscription` is a minimal handler that only tracks terminal `Stop`/`Error`
/// to clear the running indicator, mark the thread unread, and drop it from
/// `background_threads` — it never touches `conversation`/`self.thread`, so a
/// background thread's events cannot be misattributed to the foreground thread.
struct BackgroundThread {
    entity: Entity<Thread>,
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

pub struct Workspace {
    pub(crate) cwd: PathBuf,
    pub(crate) thread: Entity<Thread>,
    /// Threads that were running when the user switched away. Holding strong
    /// references keeps their `run_turn_loop` tasks alive so they can finish
    /// in the background and persist via the spawned-task save backstop. Each
    /// carries a minimal subscription so a terminal `Stop`/`Error` arriving
    /// while parked marks the thread unread for the sidebar red dot.
    background_threads: Vec<BackgroundThread>,
    pub(crate) sidebar: Entity<Sidebar>,
    pub(crate) conversation: Entity<ConversationState>,
    pub(crate) input_state: Entity<InputState>,
    /// Per-thread unsent composer text, keyed by thread id. Saved when
    /// switching away and restored on return, so each thread keeps its own
    /// in-progress draft instead of a single shared input bleeding across.
    drafts: HashMap<String, String>,
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
    /// Plan text for the `RightTab::PlanPreview` tab. `None` when no plan
    /// preview tab is open.
    plan_preview_text: Option<String>,
    /// Peer right-pane tabs for the editor, member/sub-agent observers,
    /// browser, and plan preview. The pane renders while non-empty;
    /// `editor_open` tracks whether the Editor tab specifically is active.
    right_tabs: Vec<RightTab>,
    active_right_tab: usize,
    /// Lazily-built MemberPanel entities, keyed by member name. A member tab
    /// keeps its panel across tab switches; dropped when the tab closes.
    member_panels: BTreeMap<String, Entity<MemberPanel>>,
    /// Read-only sub-agent observations keyed by the active root thread id.
    /// Switching main tasks releases the outgoing registry; completed panels
    /// can later be rebuilt recursively from persisted result envelopes.
    subagent_sessions: HashMap<String, SubagentSession>,
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
    /// Pending tool-call authorizations, keyed by their (possibly composite)
    /// id. Multiple can be open at once when parallel sub-agents each bubble an
    /// approval request — the overlay shows the most recent and queues the rest,
    /// resolving them one at a time so no `oneshot` is stranded by overwrite.
    pending_auths: Vec<PendingAuth>,
    /// Pending inbound-write requests from built-in browser tabs. Stacked
    /// like `pending_auths`; the overlay shows the most recent and queues the
    /// rest. Each carries its own `Thread::respond_inbound` id.
    pending_inbounds: Vec<PendingInbound>,
    /// A pending `AskUserQuestion` card rendered inline in the message list.
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
    /// Reasoning-effort dropdown (High / Max).
    effort_open: bool,
    effort_menu: Option<Entity<PopupMenu>>,
    effort_menu_sub: Option<Subscription>,
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
    /// Title bar "..." dropdown (conversation menu). Mirrors the
    /// model selector pattern: a button toggles `title_menu_open`; the
    /// `PopupMenu` entity and its dismiss subscription are created on open.
    title_menu_open: bool,
    title_menu: Option<Entity<PopupMenu>>,
    title_menu_sub: Option<Subscription>,
    /// A completed plan awaiting the user's implement / clear-context / stay
    /// verdict, rendered as the inline plan-review drawer card.
    pending_plan_review: Option<PendingPlanReview>,
    /// Per-thread stash of `pending_plan_review`, keyed by thread id. A
    /// pending plan never enters persisted messages (the `<proposed_plan>`
    /// block is stripped before the assistant text is saved), and a reloaded
    /// thread always starts in `Default` mode, so without this stash the
    /// verdict card + buttons vanish on a switch-away/switch-back round-trip.
    /// Mirrors `drafts`: populated on switch-away, drained on switch-back.
    pending_plans: HashMap<String, PendingPlanReview>,
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
    /// Pixel-anchored scroll state for the message column. Unlike `gpui::list`
    /// (index-anchored), a plain `track_scroll` container anchors the viewport
    /// to an absolute pixel offset: an item growing below the viewport, a width
    /// change reflowing every block, or a streaming-to-finalized body swap never
    /// shifts the viewport — the "messages fly off the top" failure mode is
    /// structurally gone, not patched over. `child_bounds` (populated from the
    /// div's direct children) still drives `bounds_for_item` / `scroll_to_item`
    /// for click-to-reveal.
    message_scroll: ScrollHandle,
    /// Tail-follow pin. Driven each render from the live scroll offset: pinned
    /// to the bottom means following (re-pin to bottom while content streams
    /// in); scrolled away means hold the absolute pixel offset so the user's
    /// readback survives the stream.
    auto_follow: bool,
    /// Holds tail-follow off while a navigator jump crosses GPUI's prepaint
    /// boundary. The first protected frame drains any previously queued
    /// `scroll_to_bottom`; the second applies `scroll_to_top_of_item` without
    /// that stale request overriding it.
    pending_message_reveal: Option<PendingMessageReveal>,
    message_reveal_generation: u64,
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
    /// The currently-displayed external session id when
    /// `view_mode == ExternalSession`. Mirrors `terminal_view`'s "one at a
    /// time" model; switching away parks the session (its terminal keeps
    /// running) rather than killing it.
    active_external: Option<String>,
    /// Generation counter for the debounced git-status refresh. Bumped on every
    /// refresh trigger (thread attach, terminal stop, enter/exit worktree) so
    /// a prior in-flight refresh self-cancels instead of overwriting newer
    /// state. The refresh runs on the global tokio runtime and delivers its
    /// result back via `async_channel`, the same bridge the worktree tool uses.
    git_status_gen: u64,
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

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let auto_compact = settings::load().auto_compact;
        let thread = {
            let id = ThreadId(uuid::Uuid::new_v4().to_string());
            Thread::new(id, cwd.clone(), cx)
        };

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
        let weak_workspace = cx.weak_entity();
        let context_rail = cx.new(|_| {
            crate::views::context_rail::ContextRail::new(
                thread.clone(),
                weak_workspace,
                auto_compact.enabled,
                auto_compact.threshold,
            )
        });

        let mut ws = Self {
            cwd,
            thread,
            background_threads: Vec::new(),
            sidebar,
            conversation: conversation.clone(),
            input_state,
            drafts: HashMap::new(),
            editor_state,
            editor_open: false,
            editor_preview: false,
            editor_preview_md: None,
            editor_preview_scroll: ScrollHandle::new(),
            plan_preview_text: None,
            right_tabs: Vec::new(),
            active_right_tab: 0,
            member_panels: BTreeMap::new(),
            subagent_sessions: HashMap::new(),
            browser_views: BTreeMap::new(),
            editor_width: px(EDITOR_PANEL_WIDTH),
            sidebar_width: px(SIDEBAR_WIDTH),
            pending_auths: Vec::new(),
            pending_inbounds: Vec::new(),
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
            completion: None,
            turn_navigator: None,
            turn_navigator_sub: None,
            turn_navigator_previous_focus: None,
            title_menu_open: false,
            title_menu: None,
            title_menu_sub: None,
            pending_plan_review: None,
            pending_plans: HashMap::new(),
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
            message_scroll: ScrollHandle::new(),
            auto_follow: true,
            pending_message_reveal: None,
            message_reveal_generation: 0,
            view_mode: ViewMode::default(),
            exiting_settings: false,
            settings_transition_gen: 0,
            goal_popover_open: false,
            team_chip_open: false,
            goal_ticker_gen: 0,
            turn_active: false,
            thinking_ticker_gen: 0,
            settings_view: None,
            settings_sub: None,
            terminal_view: None,
            context_rail,
            external_sessions: Vec::new(),
            active_external: None,
            git_status_gen: 0,
        };
        ws.thread_sub = Some(ws.subscribe_thread(cx));
        ws.sidebar_sub = Some(ws.subscribe_sidebar(window, cx));
        ws.input_sub = Some(ws.subscribe_input(window, cx));
        ws.editor_sub = Some(ws.subscribe_editor(window, cx));
        // Focus the composer so typing works immediately on the hero screen.
        ws.input_state.update(cx, |s, cx| s.focus(window, cx));
        let id = ws.thread.read(cx).id.0.clone();
        ws.sidebar
            .update(cx, |s, cx| s.set_selected(Some(id.clone()), cx));
        // The initial thread is the one the user lands on at startup: clear any
        // unread red dot it carried from a prior background completion.
        let store = agent::thread_store_global();
        store.update(cx, |s, cx| s.set_unread(&id, false, cx));
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
                    if tool_name == agent::tools::ASK_USER_QUESTION {
                        // The question card is the only surface for an
                        // interactive tool — it never queues as a generic
                        // approval entry.
                        this.pending_ask = parse_pending_ask(id.clone(), input.clone());
                        this.ask_step = 0;
                        this.ask_transition_gen = this.ask_transition_gen.wrapping_add(1);
                    } else {
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
                    }
                    this.context_rail.update(cx, |r, cx| {
                        r.cockpit_phase = CockpitPhase::AwaitingApproval;
                        cx.notify();
                    });
                    cx.notify();
                }
                ThreadEvent::PlanDelta { .. } => {
                    // Live plan text streaming in; the finalized plan surfaces
                    // as a PlanReady review prompt at turn end. Deltas only
                    // refresh so a future real-time preview can hook here.
                    cx.notify();
                }
                ThreadEvent::PlanReady { plan_text } => {
                    let weak = cx.weak_entity();
                    let role = this.model_label(cx);
                    let pt = plan_text.clone();
                    this.conversation.update(cx, |c, cx| {
                        c.push_plan_review(pt.clone(), role, weak, cx);
                    });
                    this.pending_plan_review = Some(PendingPlanReview { plan_text: pt });
                    this.auto_follow = true;
                    this.message_scroll.scroll_to_bottom();
                    cx.notify();
                }
                ThreadEvent::PlanUpdated { snapshot } => {
                    let snapshot = snapshot.clone();
                    this.context_rail.update(cx, |r, cx| {
                        r.set_plan(snapshot, cx);
                    });
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
                ThreadEvent::ContextOptimizationUpdated(metrics) => {
                    this.context_rail.update(cx, |rail, cx| {
                        rail.optimization = Some(metrics.clone());
                        cx.notify();
                    });
                }
                ThreadEvent::SideCallMetricsUpdated(metrics) => {
                    this.context_rail.update(cx, |rail, cx| {
                        rail.side_calls = metrics.clone();
                        cx.notify();
                    });
                }
                ThreadEvent::MainCallMetricsUpdated(metric) => {
                    this.context_rail.update(cx, |rail, cx| {
                        rail.main_call = Some(metric.clone());
                        cx.notify();
                    });
                }
                ThreadEvent::TurnStarted => {
                    // Light up the sidebar running indicator immediately —
                    // before the first streaming delta arrives (model warm-up,
                    // network latency). Terminal `Stop`/`Error` below clear it.
                    let thread_id = this.thread.read(cx).id.0.clone();
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| s.mark_running(&thread_id, cx));
                    // Drive the Thinking status row's per-second "for Xs"
                    // counter while this turn is live. The ticker polls
                    // `turn_active` and self-terminates on the terminal stop.
                    this.turn_active = true;
                    this.spawn_thinking_ticker(cx);
                    this.context_rail.update(cx, |r, _cx| {
                        r.cockpit_phase = CockpitPhase::Thinking;
                    });
                }
                ThreadEvent::TurnFinished {
                    cancelled,
                    failed,
                    stranded_steer_ids,
                } => {
                    // This is the authoritative end-of-turn boundary: unlike a
                    // provider Stop event, `Thread::is_running()` is already
                    // false, so a queued follow-up can safely start a new turn.
                    this.mark_stranded_steers_failed(stranded_steer_ids, cx);
                    let thread_id = this.thread.read(cx).id.0.clone();
                    save_thread(this.thread.clone(), true, cx);
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| s.mark_idle(&thread_id, cx));
                    this.turn_active = false;
                    this.context_rail.update(cx, |r, _cx| {
                        r.cockpit_phase = if *failed {
                            CockpitPhase::Failed
                        } else {
                            CockpitPhase::Stopped
                        };
                    });
                    this.background_threads
                        .retain(|b| b.entity.read(cx).id.0 != thread_id);
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
                    this.conversation.update(cx, |c, cx| {
                        c.apply(
                            ev,
                            &role,
                            usage,
                            crate::conversation::ApplyCtx { weak, cwd },
                            cx,
                        )
                    });
                    // Stop flips streaming flags off, so finalized bodies switch
                    // to full `Markdown` layout and may grow a frame or two later;
                    // the pixel-anchored scroll holds the viewport steady across
                    // that growth, and tail-follow — if still engaged — re-pins
                    // to the end via the per-frame arbitration in `render`.
                    // Persist on terminal state (not the ToolUse mid-state).
                    if !matches!(reason, StopReason::ToolUse) {
                        // A terminal stop is exactly when a finalized plan
                        // surfaces — `PlanReady` is emitted just before `Stop`
                        // in the same thread update, so it has already armed
                        // the review overlay. Do NOT clear it here (the old
                        // oneshot-approval clear raced the PlanReady arm).
                        // Mid-turn `Stop(ToolUse)` carries no plan review.
                        // A terminal state abandons any parked browser yield
                        // (the parked Task is cancelled on the thread side)
                        // and dismisses stranded inbound-write overlays whose
                        // decision oneshot the thread just dropped.
                        if let Some(host) = crate::browser_host::WorkspaceBrowserHost::concrete() {
                            host.clear_yields_for_thread(this, cx);
                        }
                        this.pending_inbounds.clear();
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
                ThreadEvent::SteerInjected { message_id } => {
                    // The running turn drained the steer. Confirm the bubble
                    // that was inserted optimistically when the user clicked;
                    // do not push a duplicate.
                    this.consume_steered_follow_up(message_id, cx);
                }
                ThreadEvent::InboundAuthorization { id, intent, .. } => {
                    // A built-in browser tab requested an inbound write. This
                    // axis ignores `ApprovalMode` — always confirm, never let
                    // a page drive the agent unprompted — so it is stacked
                    // separately from `pending_auths` and resolved through
                    // `Thread::respond_inbound`, not the outbound pipeline.
                    this.pending_inbounds.push(PendingInbound {
                        id: id.clone(),
                        intent: intent.clone(),
                    });
                    cx.notify();
                }
                _ => {
                    // `Error` is a terminal signal symmetric to a terminal
                    // `Stop`: the turn aborted, so this thread is no longer
                    // running. Pulled out of the catch-all rather than given a
                    // dedicated arm because the conversation still needs the
                    // generic `apply` below to render the error item.
                    if let ThreadEvent::Error(e) = ev {
                        let thread_id = this.thread.read(cx).id.0.clone();
                        let store = agent::thread_store_global();
                        store.update(cx, |s, cx| {
                            s.mark_idle(&thread_id, cx);
                            s.set_errored(&thread_id, true, cx);
                            s.set_unread(&thread_id, true, cx);
                        });
                        this.turn_active = false;
                        this.background_threads
                            .retain(|b| b.entity.read(cx).id.0 != thread_id);
                        // An error is a terminal state symmetric to a terminal
                        // `Stop`: the turn aborted, so any pending plan review
                        // is now stale and must not linger over an idle thread.
                        if this.pending_plan_review.take().is_some() {
                            this.conversation
                                .update(cx, |c, cx| c.consume_plan_review(cx));
                        }
                        // Symmetric to the terminal `Stop` arm: retire parked
                        // browser yields + stranded inbound overlays so an
                        // aborted turn leaves no stale banner behind.
                        if let Some(host) = crate::browser_host::WorkspaceBrowserHost::concrete() {
                            host.clear_yields_for_thread(this, cx);
                        }
                        this.pending_inbounds.clear();
                        this.context_rail.update(cx, |r, _cx| {
                            r.cockpit_phase = CockpitPhase::Failed;
                        });
                        // Persist the error card so a reloaded thread reproduces
                        // what went wrong, anchored to the failed turn.
                        this.record_ui_note(agent::db::UiNoteKind::Error, e.to_string(), cx);
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
                    let root_thread_id = this.thread.read(cx).id.0.clone();
                    match ev {
                        ThreadEvent::SubagentStarted {
                            id,
                            subagent_type,
                            description,
                            child,
                        } => this.register_live_subagent(
                            root_thread_id,
                            SubagentInfo {
                                id: id.clone(),
                                parent_id: None,
                                subagent_type: subagent_type.clone(),
                                description: description.clone(),
                                status: agent::ToolCallStatus::Running,
                            },
                            child.clone(),
                            cx,
                        ),
                        ThreadEvent::SubagentProgress { id, status, .. } => {
                            this.update_subagent_status(&root_thread_id, id, *status, cx);
                        }
                        _ => {}
                    }
                    let weak = cx.weak_entity();
                    let role = this.model_label(cx);
                    let usage = this.thread.read(cx).last_request_token_usage();
                    let cwd = thread_cwd(&this.thread, cx);
                    this.conversation.update(cx, |c, cx| {
                        c.apply(
                            ev,
                            &role,
                            usage,
                            crate::conversation::ApplyCtx { weak, cwd },
                            cx,
                        )
                    });
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
        thread: Entity<Thread>,
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
                ThreadEvent::SteerInjected { message_id } => {
                    this.consume_background_steer(&id, message_id);
                }
                ThreadEvent::TurnFinished {
                    cancelled,
                    stranded_steer_ids,
                    ..
                } => {
                    this.finish_background_follow_ups(
                        &id,
                        &parked_thread,
                        *cancelled,
                        stranded_steer_ids,
                        cx,
                    );
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| {
                        s.mark_idle(&id, cx);
                        s.set_unread(&id, true, cx);
                    });
                }
                ThreadEvent::Error(_) => {
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| {
                        s.mark_idle(&id, cx);
                        s.set_errored(&id, true, cx);
                        s.set_unread(&id, true, cx);
                    });
                }
                ThreadEvent::BackgroundTaskUpdated { .. } => {
                    save_thread(parked_thread.clone(), false, cx);
                    let store = agent::thread_store_global();
                    store.update(cx, |s, cx| s.set_unread(&id, true, cx));
                }
                _ => {}
            },
        )
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
                SidebarEvent::OpenExternalSession(id) => {
                    this.attach_external_session(id, window, cx);
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
        // The agent process runs in the project directory when spawned from a
        // project folder's `+` button, else the workspace cwd. cx's
        // AgentBuilder.cwd forwards to both the PTY-relay and direct spawn
        // paths; without it the agent inherits manox's own cwd (the user's
        // home dir), so a project-scoped session would operate on the wrong
        // tree.
        let cwd = project_cwd.clone().unwrap_or_else(|| self.cwd.clone());
        let handle = match cx::AgentBuilder::new()
            .agent(kind.agent())
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
        let terminal = match Terminal::new(id.clone(), cwd, 80, 24, Box::new(source), cx) {
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
        // title into the sidebar row + titlebar as it changes. The subscription
        // lives on the session so a later close detaches it before any spurious
        // event.
        let exit_id = id.clone();
        let exit_sub = cx.subscribe(
            &terminal,
            move |this, _terminal, ev: &terminal::event::TerminalEvent, cx| match ev {
                terminal::event::TerminalEvent::ChildExit(_) => {
                    this.remove_external_session(&exit_id, cx);
                }
                terminal::event::TerminalEvent::Title(title) => {
                    this.set_external_title(&exit_id, title.clone(), cx);
                }
                _ => {}
            },
        );
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
        self.external_sessions.push(ExternalSession {
            id: id.clone(),
            kind,
            created_at,
            project: project_cwd,
            title: None,
            cx_session_id,
            socket_path,
            terminal_view: view,
            handle,
            _exit_sub: exit_sub,
        });
        self.sync_sidebar_external(cx);
        self.attach_external_session(&id, window, cx);
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
        let new = title.as_deref().filter(|t| !t.is_empty());
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
            .map(|s| Arc::clone(&s.handle));
        if let Some(handle) = kill_handle
            && let Err(e) = handle.kill()
        {
            tracing::warn!(error = %e, id, "external session kill failed");
        }
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
        if was_active {
            self.active_external = None;
            self.focus_conversation(cx);
        }
        self.sync_sidebar_external(cx);
    }

    /// Push a fresh projection of the live external sessions to the sidebar so
    /// its "External" section reflects spawns / closes without owning the
    /// PTY-bearing structs.
    fn sync_sidebar_external(&mut self, cx: &mut Context<Self>) {
        let summaries: Vec<_> = self.external_sessions.iter().map(|s| s.summary()).collect();
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

    fn blocking_overlay_active(&self) -> bool {
        self.pending_plan_review.is_some()
            || !self.pending_auths.is_empty()
            || !self.pending_inbounds.is_empty()
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

    fn cancel_pending_message_reveal(&mut self) {
        self.message_reveal_generation = self.message_reveal_generation.wrapping_add(1);
        self.pending_message_reveal = None;
    }

    fn follow_message_tail(&mut self) {
        self.cancel_pending_message_reveal();
        self.auto_follow = true;
        self.message_scroll.scroll_to_bottom();
    }

    fn reveal_message(&mut self, item_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.message_reveal_generation = self.message_reveal_generation.wrapping_add(1);
        let pending = PendingMessageReveal {
            generation: self.message_reveal_generation,
            item_ix,
        };
        self.pending_message_reveal = Some(pending);
        self.auto_follow = false;

        // A tail-follow render may already have queued `scroll_to_bottom` on
        // the handle. GPUI applies an active-item jump before consuming that
        // flag, so submitting both for one prepaint always leaves us at the
        // bottom. The first protected frame only consumes the stale flag; its
        // next-frame callback then queues the jump for a second protected
        // frame. Protection is released before the third frame renders.
        cx.on_next_frame(window, move |this, window, cx| {
            if this.pending_message_reveal != Some(pending) {
                return;
            }
            cx.notify();
            cx.on_next_frame(window, move |this, window, cx| {
                if this.pending_message_reveal != Some(pending) {
                    return;
                }
                this.message_scroll.scroll_to_top_of_item(pending.item_ix);
                cx.notify();
                cx.on_next_frame(window, move |this, _window, cx| {
                    if this.pending_message_reveal == Some(pending) {
                        this.pending_message_reveal = None;
                        this.auto_follow = false;
                        cx.notify();
                    }
                });
            });
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
        new_thread: Entity<Thread>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_turn_navigator(window, cx);
        let old_thread = self.thread.clone();
        let old_id = old_thread.read(cx).id.0.clone();
        let new_id = new_thread.read(cx).id.0.clone();
        self.close_subagent_tabs();
        if old_id != new_id {
            self.subagent_sessions.remove(&old_id);
        }

        // Save the outgoing thread's unsent composer text before switching, so
        // a draft survives a round-trip through another thread (Bug 1). A
        // thread that just submitted already cleared its input, storing "".
        self.drafts.insert(
            old_id.clone(),
            self.input_state.read(cx).value().to_string(),
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

        // If the old thread is still running a turn, park it in the background
        // so its `run_turn_loop` task stays alive (the entity is otherwise only
        // held by `self.thread`; overwriting that field would drop it and
        // silently kill the turn via `WeakEntity::upgrade() -> None`).
        if (old_thread.read(cx).is_running()
            || agent::background_task::thread_has_running_tasks(&old_id))
            && old_id != new_id
        {
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
        let subagent_snapshots = snapshots_from_messages(&messages);
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
        // Reveal the latest turn for the new thread: pin the pixel-anchored
        // scroll to the bottom. A running thread keeps following the tail
        // (re-pinned each frame while at the bottom); a completed history
        // thread stays at the bottom once revealed, and scrolling up holds
        // because the per-frame arbitration disengages follow on upward scroll.
        self.follow_message_tail();
        self.pending_auths.clear();
        self.pending_ask = None;
        // Restore the incoming thread's stashed pending plan, if any. A reloaded
        // thread always starts in `Default` mode (collaboration_mode is
        // session-scoped and never persisted), so a restored verdict also
        // re-enters `Plan` mode — the buttons are only meaningful while the
        // model is paused in plan review, and a free-form message must keep
        // routing through Plan mode (re-propose) rather than Default (execute).
        self.pending_plan_review = self.pending_plans.remove(&new_id);
        if let Some(review) = self.pending_plan_review.as_ref() {
            self.thread
                .update(cx, |t, cx| t.set_collaboration_mode(ModeKind::Plan, cx));
            let plan_text = review.plan_text.clone();
            let weak = cx.weak_entity();
            self.conversation.update(cx, |c, cx| {
                c.push_plan_review(plan_text, role.clone(), weak, cx);
            });
            // The card is appended after the earlier `scroll_to_bottom`, so
            // re-pin so the restored drawer is in view.
            self.auto_follow = true;
            self.message_scroll.scroll_to_bottom();
        }
        self.restore_queued_follow_ups(&new_id, &messages, running, cx);
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
        // incoming one. The execution plan, unlike the
        // proposed-plan review, IS recoverable — each `UpdatePlan` call is an
        // ordinary tool round-trip in history, so the rail's plan is re-derived
        // from `messages` below. Refresh cached auto-compact knobs in case the
        // user edited settings while viewing another thread — cheap, and keeps
        // the budget accurate.
        let auto_compact = settings::load().auto_compact;
        let new_thread_for_rail = self.thread.clone();
        let restored_plan = agent::plan::rebuild_from_messages(&messages);
        self.context_rail.update(cx, |r, cx| {
            // Rebind the rail to the incoming thread. Without this the rail
            // keeps reading the construction-time thread's `per_model` usage /
            // project / display_title, so a freshly loaded thread with real
            // usage data renders an empty "消费" section (no per-model tree).
            r.thread = new_thread_for_rail;
            r.reset_for_thread_switch(running, cx);
            r.cockpit_auto_compact_enabled = auto_compact.enabled;
            r.cockpit_auto_compact_threshold = auto_compact.threshold;
            if let Some(snapshot) = restored_plan {
                r.set_plan(snapshot, cx);
            }
            cx.notify();
        });
        if self.subagent_sessions.contains_key(&new_id) {
            self.sync_subagents_to_rail(cx);
        } else {
            self.rebuild_subagent_observations(subagent_snapshots, cx);
        }
        // If the new thread has pending authorizations (e.g. it was parked
        // while waiting for tool approval), re-surface them so the overlay
        // appears immediately upon switching back.
        self.resurface_pending_auths(cx);
        self.sidebar
            .update(cx, |s, cx| s.set_selected(Some(id.clone()), cx));
        // The user is now viewing this thread: clear any unread red dot it
        // carried from a prior background completion.
        let store = agent::thread_store_global();
        store.update(cx, |s, cx| s.set_unread(&id, false, cx));
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

    /// Re-surface any pending authorizations on the current thread that were
    /// emitted while the thread was in the background (no subscription). Called
    /// after switching threads so the overlay appears without requiring the
    /// user to wait for the next event.
    fn resurface_pending_auths(&mut self, cx: &mut Context<Self>) {
        // Query the thread for any pending authorization metadata that was
        // stored when the auth event was originally emitted. If the thread was
        // parked waiting for user approval while in the background, re-surface
        // the events so the overlay appears immediately upon switching back.
        let entries: Vec<(String, String, String, serde_json::Value)> = self
            .thread
            .read(cx)
            .pending_auth_entries()
            .into_iter()
            .map(|(id, meta)| {
                (
                    id,
                    meta.tool_name.clone(),
                    meta.summary.clone(),
                    meta.input.clone(),
                )
            })
            .collect();
        for (id, tool_name, summary, input) in entries {
            // AskUserQuestion needs its interactive card state rebuilt too —
            // without `pending_ask` the generic approval overlay would surface
            // for a tool that must only ever show the question card.
            if tool_name == agent::tools::ASK_USER_QUESTION {
                self.pending_ask = parse_pending_ask(id.clone(), input);
                self.ask_step = 0;
                self.ask_transition_gen = self.ask_transition_gen.wrapping_add(1);
                continue;
            }
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
        if !self.pending_auths.is_empty() || self.pending_ask.is_some() {
            cx.notify();
        }
    }

    /// Open a fresh thread. The provider registry is reloaded from the cx
    /// config file first (on a background thread — api key resolution may hit
    /// the OS keychain), so every new thread starts from the latest providers
    /// and the swapped-in snapshot is visible to all existing threads' model
    /// menus. A failed reload keeps the previous registry and surfaces a
    /// notice; the new thread is still created either way.
    fn start_new_thread(
        &mut self,
        project: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cwd = self.cwd.clone();
        cx.spawn_in(window, async move |this, cx| {
            let reload_result = cx.background_spawn(async move { registry::reload() }).await;
            let _ = this.update_in(cx, |this, window, cx| {
                let id = ThreadId(uuid::Uuid::new_v4().to_string());
                let new = Thread::new(id, cwd, cx);
                if let Some(dir) = project {
                    new.update(cx, |t, cx| t.set_project(dir, cx));
                }
                this.attach_thread(new, window, cx);
                // The notice must land after the attach: `add_info_message`
                // targets the active conversation, which is the outgoing
                // thread until `attach_thread` swaps it.
                if let Err(e) = reload_result {
                    let msg = i18n::t_str(
                        "workspace-provider-reload-failed",
                        &[("error", &e.to_string())],
                    );
                    this.add_info_message(msg.to_string(), cx);
                }
            });
        })
        .detach();
    }

    /// Archive the active thread and navigate to a fresh empty one. Shared
    /// by the `/exit` slash command and the `cmd-;` keybinding.
    pub(crate) fn archive_current_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // No-op while a turn is running: `attach_thread` would park the thread
        // in the background and clear `pending_auths`, stranding tool approvals.
        if self.thread.read(cx).is_running() {
            return;
        }
        let id = self.thread.read(cx).id.0.clone();
        self.thread.update(cx, |t, cx| t.set_archived(true, cx));
        let store = agent::thread_store_global();
        store.update(cx, |s, cx| s.archive_thread(&id, true, cx));
        self.start_new_thread(None, window, cx);
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
        // Markdown prompt-macro commands (`/gitwork:deliver …`) are registered
        // into the same registry as `MarkdownSlashCommand` adapters and dispatch
        // into `run_command_turn` → `Thread::submit_command`, which substitutes
        // `$ARGUMENTS` and applies the command's `allowed-tools` filter.
        // Plugin/user skills are mirrored as `SkillSlashCommand` adapters too,
        // dispatching into `run_skill_turn` → `Thread::submit_skill`, which injects
        // the skill body as the turn's user message — so `/gitwork:deliver` works
        // even though gitwork ships it as a skill, not a command.
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
        // Re-engage tail-follow so the streaming reply stays in view: the
        // per-frame arbitration re-pins to the bottom while the user rides it.
        self.follow_message_tail();
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

    /// Run a skill slash-command turn (`/gitwork:deliver args`). The display
    /// text (`/key args`) is shown as the user bubble; `Thread::submit_skill`
    /// injects the skill body (plus the appended args) as the turn's user
    /// message. An unknown skill (adapter registered but the data registry
    /// miss — shouldn't normally happen) surfaces an error and drops the turn.
    pub(crate) fn run_skill_turn(&mut self, key: &str, args: &str, cx: &mut Context<Self>) {
        let display_text = if args.is_empty() {
            format!("/{key}")
        } else {
            format!("/{key} {args}")
        };
        let meta = self.user_turn_meta(cx);
        let weak = cx.weak_entity();
        self.conversation.update(cx, |c, cx| {
            c.push_user(display_text, Vec::new(), meta, weak, cx)
        });
        self.follow_message_tail();
        let hit = self
            .thread
            .update(cx, |thread, cx| thread.submit_skill(key, args, cx));
        if !hit {
            self.thread.update(cx, |_, cx| {
                cx.emit(agent::ThreadEvent::Error(anyhow::anyhow!(
                    "{}",
                    i18n::t_str("workspace-unknown-skill", &[("name", key)])
                )));
            });
        }
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
        // A plan review is awaiting the user's verdict. Allow the user to send
        // messages to discuss or revise the plan. The drawer buttons (implement /
        // clear-context / stay) provide the explicit verdict, while free-form
        // input lets the user refine the plan through conversation.
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
        // rather than accepting — drop the stale verdict so the drawer hides
        // until the agent re-proposes via a fresh `PlanReady`. Without this the
        // lingering Implement button would act on the now-outdated plan text.
        let dismissed_plan = self.pending_plan_review.take();
        if let Some(review) = dismissed_plan.as_ref() {
            self.conversation
                .update(cx, |c, cx| c.consume_plan_review(cx));
            // Persist the dismissed plan as a UI note so the collapsed record
            // survives a thread switch / reload — the live card is UI-only and
            // never enters `Thread::messages`. `record_ui_note` anchors to the
            // last user message, which at this point (before the dismissing
            // message is appended below) is the one that triggered the plan's
            // turn, so the rebuild splices the card back at that turn's end,
            // ahead of this dismissing message — matching the live order.
            self.record_ui_note(
                agent::db::UiNoteKind::PlanReview,
                review.plan_text.clone(),
                cx,
            );
        }
        self.append_and_run_user_turn(turn, weak, cx);
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

    /// Restore a thread's session-local follow-up queue after a task switch.
    /// Canonical steers that drained while the thread was in the background are
    /// already present in `messages` and are discarded from the stash; a still
    /// pending steer recreates its optimistic bubble.
    fn restore_queued_follow_ups(
        &mut self,
        thread_id: &str,
        messages: &[agent::Message],
        running: bool,
        cx: &mut Context<Self>,
    ) {
        let canonical_ids: std::collections::HashSet<&str> =
            messages.iter().map(|message| message.id.as_str()).collect();
        let pending_ids: std::collections::HashSet<String> = self
            .thread
            .read(cx)
            .pending_steer_ids()
            .into_iter()
            .collect();
        let mut restored = self
            .queued_follow_ups_by_thread
            .remove(thread_id)
            .unwrap_or_default();
        let mut pending_bubbles = Vec::new();
        let mut kept = std::collections::VecDeque::new();

        while let Some(mut item) = restored.pop_front() {
            let steer_id = match &item.state {
                FollowUpState::SteerPending { message_id }
                | FollowUpState::Failed { message_id } => Some(message_id.clone()),
                FollowUpState::Queued => None,
            };
            // Canonical history wins over the session stash: this steer was
            // injected while the task was in the background, so showing the
            // parked item again would duplicate it.
            if steer_id
                .as_deref()
                .is_some_and(|id| canonical_ids.contains(id))
            {
                continue;
            }
            // Recreate an optimistic bubble only while the same steer remains
            // pending on a running turn. Otherwise surface it as retryable;
            // it was never confirmed by canonical history.
            if let FollowUpState::SteerPending { message_id } = &item.state {
                if running && pending_ids.contains(message_id) {
                    pending_bubbles.push((
                        item.turn.text.clone(),
                        item.turn.user_images.clone(),
                        item.turn.meta.clone(),
                        message_id.clone(),
                    ));
                } else {
                    item.state = FollowUpState::Failed {
                        message_id: message_id.clone(),
                    };
                }
            }
            kept.push_back(item);
        }
        self.queued_follow_ups = kept;

        let weak = cx.weak_entity();
        for (text, images, meta, message_id) in pending_bubbles {
            self.conversation.update(cx, |conversation, cx| {
                conversation.push_pending_steer(text, images, meta, message_id, weak.clone(), cx)
            });
        }
        if !self.queued_follow_ups.is_empty() {
            self.auto_follow = true;
            self.message_scroll.scroll_to_bottom();
        }
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

    fn finish_background_follow_ups(
        &mut self,
        thread_id: &str,
        thread: &Entity<Thread>,
        cancelled: bool,
        stranded_steer_ids: &[String],
        cx: &mut Context<Self>,
    ) {
        let stranded: std::collections::HashSet<&str> =
            stranded_steer_ids.iter().map(String::as_str).collect();
        let mut queue = self
            .queued_follow_ups_by_thread
            .remove(thread_id)
            .unwrap_or_default();
        for item in &mut queue {
            if let FollowUpState::SteerPending { message_id } = &item.state
                && stranded.contains(message_id.as_str())
            {
                item.state = FollowUpState::Failed {
                    message_id: message_id.clone(),
                };
            }
        }

        let mut retained = std::collections::VecDeque::new();
        let mut started_follow_up = false;
        while let Some(item) = queue.pop_front() {
            if !cancelled && matches!(item.state, FollowUpState::Queued) {
                append_deferred_turn_to_thread(thread, item.turn, cx);
                started_follow_up = true;
            } else {
                retained.push_back(item);
            }
        }
        if !retained.is_empty() {
            self.queued_follow_ups_by_thread
                .insert(thread_id.to_string(), retained);
        }
        if started_follow_up {
            save_thread(thread.clone(), true, cx);
            let thread = thread.clone();
            // TurnFinished is delivered from the parked Thread's entity
            // update. Defer the successor so `run_turn` cannot re-enter that
            // update and so the terminal store bookkeeping finishes first.
            cx.defer(move |cx| {
                thread.update(cx, |thread, cx| thread.run_turn(cx));
            });
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
        self.auto_follow = true;
        self.message_scroll.scroll_to_bottom();
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
                    self.append_user_turn(item.turn, weak.clone(), self.auto_follow, cx);
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
        // Only re-engage tail-follow if the user hasn't scrolled away —
        // a steer injection is event-driven, not user-initiated, so it
        // should not yank the viewport back to the bottom.
        if self.auto_follow {
            self.message_scroll.scroll_to_bottom();
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

    pub(crate) fn register_live_subagent(
        &mut self,
        root_thread_id: String,
        info: SubagentInfo,
        child: Entity<Thread>,
        cx: &mut Context<Self>,
    ) {
        let id = info.id.clone();
        let panel = SubagentPanel::live(
            child,
            root_thread_id.clone(),
            info.clone(),
            cx.weak_entity(),
            cx,
        );
        let session = self
            .subagent_sessions
            .entry(root_thread_id.clone())
            .or_default();
        if !session.records.contains_key(&id) {
            session.order.push(id.clone());
        }
        session.records.insert(id, SubagentRecord { info, panel });
        if self.thread.read(cx).id.0 == root_thread_id {
            self.sync_subagents_to_rail(cx);
        }
        cx.notify();
    }

    pub(crate) fn update_subagent_status(
        &mut self,
        root_thread_id: &str,
        id: &str,
        status: agent::ToolCallStatus,
        cx: &mut Context<Self>,
    ) {
        let Some(record) = self
            .subagent_sessions
            .get_mut(root_thread_id)
            .and_then(|session| session.records.get_mut(id))
        else {
            return;
        };
        record.info.status = status;
        record
            .panel
            .update(cx, |panel, cx| panel.set_status(status, cx));
        if self.thread.read(cx).id.0 == root_thread_id {
            self.sync_subagents_to_rail(cx);
        }
        cx.notify();
    }

    pub(crate) fn open_subagent_tab_by_id(&mut self, id: &str, cx: &mut Context<Self>) {
        let root_thread_id = self.thread.read(cx).id.0.clone();
        if !self
            .subagent_sessions
            .get(&root_thread_id)
            .is_some_and(|session| session.records.contains_key(id))
        {
            return;
        }
        let ix = ensure_subagent_tab(&mut self.right_tabs, id);
        self.set_active_right_tab(ix, cx);
    }

    fn sync_subagents_to_rail(&mut self, cx: &mut Context<Self>) {
        let root_thread_id = self.thread.read(cx).id.0.clone();
        let agents = self
            .subagent_sessions
            .get(&root_thread_id)
            .map(|session| {
                session
                    .order
                    .iter()
                    .filter_map(|id| session.records.get(id).map(|record| record.info.clone()))
                    .collect()
            })
            .unwrap_or_default();
        self.context_rail
            .update(cx, |rail, cx| rail.set_agents(agents, cx));
    }

    fn rebuild_subagent_observations(
        &mut self,
        snapshots: Vec<SubagentSnapshot>,
        cx: &mut Context<Self>,
    ) {
        let root_thread_id = self.thread.read(cx).id.0.clone();
        let mut session = SubagentSession::default();
        let weak = cx.weak_entity();
        for snapshot in snapshots {
            let id = snapshot.info.id.clone();
            let info = snapshot.info.clone();
            let panel = SubagentPanel::snapshot(snapshot, weak.clone(), cx);
            session.order.push(id.clone());
            session.records.insert(id, SubagentRecord { info, panel });
        }
        self.subagent_sessions.insert(root_thread_id, session);
        self.sync_subagents_to_rail(cx);
    }

    fn close_subagent_tabs(&mut self) {
        remove_subagent_tabs(&mut self.right_tabs, &mut self.active_right_tab);
        self.editor_open = self
            .right_tabs
            .get(self.active_right_tab)
            .is_some_and(|tab| matches!(tab, RightTab::Editor));
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
    /// its panel; a Browser tab drops its webview.
    fn close_right_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.right_tabs.get(ix), Some(RightTab::Editor)) {
            self.close_editor(window, cx);
            return;
        }
        if let Some(RightTab::Member(name)) = self.right_tabs.get(ix).cloned() {
            self.right_tabs.remove(ix);
            self.member_panels.remove(&name);
            self.reseat_active_after_close(ix);
            self.editor_open = self
                .right_tabs
                .get(self.active_right_tab)
                .is_some_and(|t| matches!(t, RightTab::Editor));
            cx.notify();
            return;
        }
        if matches!(self.right_tabs.get(ix), Some(RightTab::Subagent(_))) {
            self.right_tabs.remove(ix);
            self.reseat_active_after_close(ix);
            self.editor_open = self
                .right_tabs
                .get(self.active_right_tab)
                .is_some_and(|t| matches!(t, RightTab::Editor));
            cx.notify();
            return;
        }
        if let Some(RightTab::PlanPreview) = self.right_tabs.get(ix).cloned() {
            self.right_tabs.remove(ix);
            self.plan_preview_text = None;
            self.reseat_active_after_close(ix);
            self.editor_open = self
                .right_tabs
                .get(self.active_right_tab)
                .is_some_and(|t| matches!(t, RightTab::Editor));
            cx.notify();
            return;
        }
        if let Some(RightTab::Browser(id)) = self.right_tabs.get(ix).cloned() {
            self.close_browser_tab(id, cx);
        }
    }
    /// Close the active right-pane tab if it is a browser tab. No-op
    /// otherwise (the keybinding is global; it should not close Editor or
    /// Member tabs that happen to be active).
    pub fn close_active_browser_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(RightTab::Browser(id)) = self.right_tabs.get(self.active_right_tab).cloned() {
            self.close_browser_tab(id, cx);
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
    /// Submit the editor text to the thread, then close the panel and return
    /// focus to the inline input.
    fn submit_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor_state.read(cx).value().to_string();
        if text.trim().is_empty() || self.pending_ask.is_some() || self.thread.read(cx).is_running()
        {
            return;
        }
        let meta = self.user_turn_meta(cx);
        let ui = Self::message_ui_metadata(&meta);
        let weak = cx.weak_entity();
        self.conversation.update(cx, |c, cx| {
            c.push_user(text.clone(), Vec::new(), meta, weak, cx)
        });
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

    /// Open the plan text in a right-pane PlanPreview tab (peer of Editor).
    /// If a PlanPreview tab already exists, update its text and focus it.
    pub(crate) fn open_plan_in_editor(&mut self, plan_text: String, cx: &mut Context<Self>) {
        if let Some(ix) = self
            .right_tabs
            .iter()
            .position(|t| matches!(t, RightTab::PlanPreview))
        {
            self.plan_preview_text = Some(plan_text);
            self.set_active_right_tab(ix, cx);
            return;
        }
        self.right_tabs.push(RightTab::PlanPreview);
        let ix = self.right_tabs.len() - 1;
        self.plan_preview_text = Some(plan_text);
        self.set_active_right_tab(ix, cx);
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
        // Note: Navigation away from archived thread is handled by the sidebar
        // archive button (which has Window access). The title menu archive
        // action archives the thread but leaves the view as-is; the user can
        // manually navigate or the next interaction will trigger a switch.
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
        // If tail-follow is engaged the scroll reveals the notice; if the user
        // scrolled up the per-frame arbitration has already disengaged follow so
        // the viewport stays put.
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

    pub(crate) fn resolve_auth(&mut self, decision: PermissionDecision, cx: &mut Context<Self>) {
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

    /// Resolve the most-recent inbound-write request from a built-in browser
    /// tab. Routed through `Thread::respond_inbound` — separate from the
    /// outbound `resolve_auth` pipeline because this axis is `ApprovalMode`-
    /// blind by design.
    pub(crate) fn resolve_inbound(&mut self, allowed: bool, cx: &mut Context<Self>) {
        let Some(req) = self.pending_inbounds.pop() else {
            return;
        };
        self.thread.update(cx, |thread, cx| {
            thread.respond_inbound(&req.id, allowed, cx);
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

    fn pending_ask_has_selection(&self) -> bool {
        self.pending_ask
            .as_ref()
            .is_some_and(|ask| ask.selections.iter().flatten().any(|selected| *selected))
    }

    fn composer_can_submit(&self, running: bool, cx: &App) -> bool {
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

    /// Resolve the pending plan review with the user's three-way verdict.
    /// Implement (with or without a context clear) delegates to the thread,
    /// which exits Plan mode and re-injects the plan as the implement turn's
    /// seed; the rail's plan overview seeds later from the model's first
    /// `UpdatePlan` call. Staying in Plan mode is not a verdict — the user
    /// simply keeps typing.
    pub(crate) fn respond_plan_review(
        &mut self,
        choice: PlanReviewChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(review) = self.pending_plan_review.take() else {
            return;
        };
        // The verdict becomes a user bubble carrying the approved plan text —
        // the same text+ui the thread injects below — so the live view and a
        // reloaded thread both show this one bubble. The ephemeral plan card
        // (never persisted) is retired in its place.
        let meta = self.user_turn_meta(cx);
        let ui = Self::message_ui_metadata(&meta);
        let text = agent::implement_plan_user_message(&review.plan_text);
        let plan_text = review.plan_text.clone();
        if matches!(choice, PlanReviewChoice::ImplementClearContext) {
            // Clear context = archive this thread and continue on a fresh one
            // seeded with the approved plan. The user perceives only that the
            // underlying thread id changed and prior messages vanished — the
            // new thread starts empty save the seed bubble, then runs.
            let old_id = self.thread.read(cx).id.0.clone();
            let cwd = self.thread.read(cx).cwd().to_path_buf();
            let project = self.thread.read(cx).project().cloned();
            let model = self.thread.read(cx).model().cloned();
            let effort = self.thread.read(cx).reasoning_effort();
            let approval = self.thread.read(cx).approval_mode();
            let new = Thread::new(ThreadId(uuid::Uuid::new_v4().to_string()), cwd, cx);
            new.update(cx, |t, cx| {
                if let Some(dir) = project {
                    t.set_project(dir, cx);
                }
                if let Some(model) = model {
                    t.set_model(model, cx);
                }
                t.set_reasoning_effort(effort, cx);
                t.set_approval_mode(approval, cx);
                t.seed_approved_plan(plan_text.clone(), Some(ui), cx);
            });
            // Switch the foreground: attach saves the old thread, rebuilds the
            // conversation from the new thread (just the seed bubble), wires
            // the event subscription, and clears the input draft. The seed is
            // inserted but not run yet so turn events stream into the live view.
            self.attach_thread(new, window, cx);
            // Surface the new thread in the sidebar immediately, then run —
            // run_turn marks it running so the sidebar row shows the spinner.
            save_thread(self.thread.clone(), true, cx);
            self.thread.update(cx, |t, cx| t.run_turn(cx));
            // The rail's plan is no longer seeded from the approved plan text.
            // The model publishes its execution plan via `UpdatePlan` once work
            // begins; until then the section stays empty (an empty rail beats a
            // stale 27-bullet dump of the plan's implementation details).
            // Retire the old thread from the active list (consistent with any
            // archived thread — no "show archived" UI today).
            agent::thread_store_global().update(cx, |s, cx| s.archive_thread(&old_id, true, cx));
        } else {
            // Implement: retire the ephemeral plan card (the pending card is
            // the live tail at verdict time, so a tail pop is the safe removal)
            // and push the verdict bubble, then seed + run on the current thread.
            let weak = cx.weak_entity();
            self.conversation.update(cx, |c, cx| {
                c.pop_plan_review_tail(cx);
            });
            self.conversation.update(cx, |c, cx| {
                c.push_user(text, Vec::new(), meta, weak, cx);
            });
            self.auto_follow = true;
            self.message_scroll.scroll_to_bottom();
            // The rail's plan seeds from the model's first `UpdatePlan` call, not
            // the approved plan text — see the continue-in-plan branch above.
            self.thread.update(cx, |thread, cx| {
                thread.implement_approved_plan(plan_text, Some(ui), cx);
            });
        }
        cx.notify();
    }

    fn render_auth_overlay(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
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
                                // Permission summaries read as tool-call output, so
                                // they render in Lilex LightItalic.
                                .italic()
                                .font_weight(gpui::FontWeight::LIGHT)
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

    /// Confirmation overlay for an inbound-write request from a built-in
    /// browser tab. Mirrors `render_auth_overlay`'s scrim + card layout but
    /// resolves through `respond_inbound` (the `ApprovalMode`-blind axis).
    fn render_inbound_overlay(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let req = self.pending_inbounds.last()?;
        let intent = req.intent.clone();
        let queued = self.pending_inbounds.len().saturating_sub(1);
        Some(
            gpui::div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
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
                                .child(
                                    Icon::new(IconName::TriangleAlert)
                                        .small()
                                        .text_color(theme.danger),
                                )
                                .child(
                                    gpui::div()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .child(i18n::t("workspace-inbound-title")),
                                ),
                        )
                        .child(
                            gpui::div()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child(i18n::t_str(
                                    "workspace-inbound-intent",
                                    &[("intent", intent.as_str())],
                                )),
                        )
                        .child(
                            gpui::div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(i18n::t("workspace-inbound-note")),
                        )
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
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(
                                    Button::new("inbound-deny")
                                        .ghost()
                                        .small()
                                        .label(i18n::t("workspace-inbound-deny"))
                                        .on_click(cx.listener({
                                            move |this, _, _, cx| {
                                                this.resolve_inbound(false, cx);
                                            }
                                        })),
                                )
                                .child(
                                    Button::new("inbound-allow")
                                        .primary()
                                        .small()
                                        .label(i18n::t("workspace-inbound-allow"))
                                        .on_click(cx.listener({
                                            move |this, _, _, cx| {
                                                this.resolve_inbound(true, cx);
                                            }
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_reasoning_effort_selector(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.effort_open;
        let selected = self.thread.read(cx).reasoning_effort();
        let workspace = cx.entity().downgrade();
        // Effort enum values are provider wire literals (high/max), not UI
        // chrome — they are not localized.
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
                deferred(
                    gpui::div()
                        .id("reasoning-effort-dropdown")
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
                // `deferred()` + `with_priority(1)` paints the dropdown after
                // the whole conversation column tree, escaping overflow_hidden
                // clipping from ancestor containers. `right_0()` keeps the menu
                // within the viewport when the window sits near the screen edge.
                deferred(
                    gpui::div()
                        .id("titlebar-dropdown")
                        .absolute()
                        .top_full()
                        .right_0()
                        .occlude()
                        .child(menu),
                )
                .with_priority(1),
            )
            .into_any_element()
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
        let project_chip = self.render_project_chip(theme, cx);
        let mode_chip = self.render_mode_chip(theme, cx);
        let goal_chip = self.render_goal_chip(theme, cx);
        let team_chip = self.render_team_chip(theme, cx);
        let access = self.render_access_placeholder(theme, cx);
        let effort = self.render_reasoning_effort_selector(theme, cx);
        let model = self.render_model_selector(theme, cx);
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
                // Composer input is message content in the mono family (Lilex).
                // Weight is pinned to Light to match body type; the Input component
                // has no per-instance font knob, so family + weight are applied
                // from the host context here. While the completion popover is open
                // this wrapper sets a `completion = open` key context so the
                // `completion == open > Input` keybindings in `main.rs` can shadow
                // the Input's own up/down/enter/tab/escape bindings and drive the
                // popover instead.
                {
                    let mut wrap = gpui::div()
                        .font_family(theme.mono_font_family.clone())
                        .font_weight(gpui::FontWeight::LIGHT);
                    if self.completion.is_some() {
                        wrap = wrap.key_context("completion = open");
                    }
                    wrap.child(Input::new(&self.input_state).appearance(false))
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
                            .when_some(mode_chip, |el, chip| el.child(chip))
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
                .icon(IconName::Delete)
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
                .child(Icon::new(IconName::Redo2).xsmall().text_color(if danger {
                    theme.danger
                } else {
                    theme.muted_foreground
                }))
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

    /// Collaboration-mode chip — always visible, shows the active mode's
    /// display name so the read-only Plan posture (vs execution Default) is
    /// legible at a glance. Clicking cycles to the next mode, mirroring
    /// `/plan`, the `+` menu row, and `shift-tab`.
    fn render_mode_chip(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let mode = self.thread.read(cx).collaboration_mode();
        let in_plan = mode == ModeKind::Plan;
        let accent = theme.accent;
        let label: SharedString = i18n::t(if in_plan {
            "mode-chip-plan"
        } else {
            "mode-chip-default"
        });
        Some(
            h_flex()
                .id("mode-chip")
                .items_center()
                .gap_1()
                .px_2()
                .py_1()
                .rounded(theme.radius)
                .bg(theme.secondary)
                .border_1()
                .border_color(if in_plan { accent } else { theme.border })
                .cursor_pointer()
                .child(
                    Icon::new(IconName::LayoutDashboard)
                        .xsmall()
                        .text_color(if in_plan {
                            accent
                        } else {
                            theme.muted_foreground
                        }),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(if in_plan {
                            accent
                        } else {
                            theme.muted_foreground
                        })
                        .child(label),
                )
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.thread.update(cx, |t, cx| {
                        t.set_collaboration_mode(t.collaboration_mode().next(), cx);
                    });
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
                    .text_color(accent)
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
                                    let _ = this.thread.update(cx, |t, cx| {
                                        t.set_goal_status(
                                            agent::goal::GoalStatus::Paused,
                                            Some("paused by user".into()),
                                            agent::db::GoalActor::User,
                                            cx,
                                        )
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
                                        let _ = this.thread.update(cx, |t, cx| {
                                            t.set_goal_status(
                                                agent::goal::GoalStatus::Active,
                                                None,
                                                agent::db::GoalActor::User,
                                                cx,
                                            )
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
                                let _ = this.thread.update(cx, |t, cx| {
                                    t.clear_goal(agent::db::GoalActor::User, cx)
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
                        this.thread.update(cx, |t, cx| {
                            t.set_collaboration_mode(t.collaboration_mode().next(), cx);
                        });
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
                .foreground(theme.accent)
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
                deferred(
                    gpui::div()
                        .id("project-chip-dropdown")
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
            || self.pending_plan_review.is_some()
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
        if !matches!(self.view_mode, ViewMode::Workspace) {
            self.drop_turn_navigator(cx);
        }
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
                .on_action(
                    cx.listener(|this, _: &crate::ToggleCockpitTasks, _window, cx| {
                        this.context_rail.update(cx, |r, cx| {
                            r.cockpit_hide_tasks = !r.cockpit_hide_tasks;
                            cx.notify();
                        });
                        cx.notify();
                    }),
                )
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
        // External agent CLI session: render the active session's terminal TUI
        // in place of the conversation. Mirrors the Terminal branch — sidebar on
        // the left, a TitleBar + the terminal filling the main column. The bar
        // title is the agent's OSC title (mirrored from `TerminalEvent::Title`),
        // falling back to the kind label ("Claude Code" / "Codex" / "GitHub
        // Copilot") until the TUI sets its own. The provider/model picked at
        // spawn is intentionally omitted: the user can switch models mid-session
        // inside the TUI (`/model`), and manox cannot observe that change.
        if matches!(self.view_mode, ViewMode::ExternalSession) {
            let theme = cx.theme().clone();
            let active = self
                .active_external
                .as_deref()
                .and_then(|id| self.external_sessions.iter().find(|s| s.id == id));
            let Some(session) = active else {
                // No live session matches the recorded id (closed underneath
                // us). Fall back to the conversation pane.
                self.view_mode = ViewMode::Workspace;
                cx.notify();
                return h_flex().size_full().child(self.sidebar.clone());
            };
            let kind = session.kind;
            // Titlebar + sidebar share `display_title()` so a TUI rename
            // (OSC title) updates both at once.
            let title: SharedString = session.display_title().to_string().into();
            let terminal = session.terminal_view.clone();
            return h_flex()
                .size_full()
                .bg(theme.background)
                .text_color(theme.foreground)
                .on_action(cx.listener(|this, _: &FocusConversation, _window, cx| {
                    this.focus_conversation(cx);
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
                                    .child(
                                        gpui::svg()
                                            .path(kind.icon_asset())
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
                                            .child(title),
                                    ),
                            ),
                        )
                        .child(v_flex().flex_1().h_full().w_full().child(terminal)),
                );
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
        let first_screen = self.conversation.read(cx).is_empty(cx) && !running;
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
        let overlay = self
            .render_auth_overlay(&theme, cx)
            .or_else(|| self.render_inbound_overlay(&theme, cx))
            .or_else(|| self.render_blank_project_overlay(window, &theme, cx));
        let turn_navigator_overlay =
            self.render_turn_navigator_overlay(window, &theme, right_pane_open, show_rail, cx);
        // The inline composer stays visible while inline AskUserQuestion cards
        // are open; submitting text resolves the ask as a free-form response.
        // The editor pane still hides the inline composer while editing there.
        let footer = if editor_open || first_screen {
            None
        } else {
            Some(
                v_flex()
                    .w_full()
                    .flex_shrink_0()
                    .bg(theme.background)
                    .py_2()
                    .gap_2()
                    .child(centered(gpui::div().w_full().h(px(1.)).bg(theme.border)))
                    .children(self.render_attachments(&theme, cx))
                    .child(centered(self.render_composer(running, window, &theme, cx))),
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
                        Tab::new().label(i18n::t_str("member-tab", &[("name", name)]))
                    }
                    RightTab::Subagent(id) => {
                        let root_thread_id = self.thread.read(cx).id.0.clone();
                        let label = self
                            .subagent_sessions
                            .get(&root_thread_id)
                            .and_then(|session| session.records.get(id))
                            .map(|record| subagent_display_title(&record.info))
                            .unwrap_or_else(|| i18n::t("subagent-tab-fallback").to_string());
                        Tab::new().label(label)
                    }
                    RightTab::Browser(id) => {
                        let url = self
                            .browser_views
                            .get(id)
                            .map(|v| v.read(cx).url().to_string())
                            .unwrap_or_default();
                        Tab::new().label(i18n::t_str("browser-tab", &[("url", &url)]))
                    }
                    RightTab::PlanPreview => Tab::new().label(i18n::t("plan-card-title")),
                };
                // Every observational/preview tab carries a close affordance;
                // the Editor tab keeps its keyboard toggle (`ToggleEditor` /
                // `CloseEditor`).
                match tab {
                    RightTab::Member(_)
                    | RightTab::Subagent(_)
                    | RightTab::Browser(_)
                    | RightTab::PlanPreview => base.suffix(
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
                                    .child(
                                        gpui::div()
                                            .w_full()
                                            .p_4()
                                            .text_sm()
                                            .child(md.into_any_element()),
                                    )
                                    .into_any_element()
                            } else {
                                gpui::div()
                                    .w_full()
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_hidden()
                                    .child(
                                        Input::new(&self.editor_state)
                                            .size_full()
                                            .appearance(false)
                                            .into_any_element(),
                                    )
                                    .into_any_element()
                            })
                            .into_any_element(),
                        Some(RightTab::Member(name)) => self
                            .member_panels
                            .get(&name)
                            .map(|p| p.clone().into_any_element())
                            .unwrap_or_else(|| gpui::div().into_any_element()),
                        Some(RightTab::Subagent(id)) => {
                            let root_thread_id = self.thread.read(cx).id.0.clone();
                            self.subagent_sessions
                                .get(&root_thread_id)
                                .and_then(|session| session.records.get(&id))
                                .map(|record| record.panel.clone().into_any_element())
                                .unwrap_or_else(|| gpui::div().into_any_element())
                        }
                        Some(RightTab::Browser(id)) => self
                            .browser_views
                            .get(&id)
                            .map(|v| v.clone().into_any_element())
                            .unwrap_or_else(|| gpui::div().into_any_element()),
                        Some(RightTab::PlanPreview) => {
                            let text = self.plan_preview_text.clone().unwrap_or_default();
                            let theme = cx.theme().clone();
                            let scroll = self.editor_preview_scroll.clone();
                            v_flex()
                                .h_full()
                                .child(
                                    h_flex().w_full().px_2().child(
                                        TabBar::new("plan-preview-tabs")
                                            .underline()
                                            .small()
                                            .selected_index(1)
                                            .child("Write")
                                            .child("Preview"),
                                    ),
                                )
                                .child(
                                    gpui::div()
                                        .id("plan-preview-scroll")
                                        .w_full()
                                        .flex_1()
                                        .min_h_0()
                                        .overflow_y_scroll()
                                        .track_scroll(&scroll)
                                        .child(
                                            gpui::div().w_full().p_4().text_sm().child(
                                                cx.new(|_cx| {
                                                    Markdown::new("plan-preview", text)
                                                        .theme(&theme)
                                                        .heading_mode(HeadingMode::Uniform)
                                                })
                                                .into_any_element(),
                                            ),
                                        ),
                                )
                                .into_any_element()
                        }
                        None => gpui::div().into_any_element(),
                    }),
            );

        h_flex()
            .size_full()
            .relative()
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
            .on_action(cx.listener(|this, _: &ToggleTurnNavigator, window, cx| {
                this.toggle_turn_navigator(window, cx);
                cx.stop_propagation();
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
            .on_action(
                cx.listener(|this, _: &crate::CycleCollaborationMode, _window, cx| {
                    this.thread.update(cx, |t, cx| {
                        t.set_collaboration_mode(t.collaboration_mode().next(), cx);
                    });
                    cx.notify();
                }),
            )
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
            .on_action(
                cx.listener(|this, _: &crate::ArchiveCurrentThread, window, cx| {
                    this.archive_current_thread(window, cx);
                }),
            )
            // Left sidebar with a draggable divider on its right edge.
            .child(self.sidebar.clone())
            .child(sidebar_divider)
            // Middle column: the conversation column. The context card floats
            // over the conversation's top-right as an absolute overlay — a peer
            // in the z-stack, not a flex column — so the column itself is the
            // conversation alone. The editor pane is a third top-level column to
            // the right.
            .child({
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .relative()
                    .overflow_hidden()
                    .child({
                        // Tail-follow arbitration, driven from the live scroll
                        // offset (reflecting the prior frame's layout). Pinned to
                        // the bottom => follow: re-pin each frame so streaming
                        // growth at the tail stays in view. Scrolled away => hold
                        // the absolute pixel offset so the user's readback of an
                        // earlier turn survives the stream below.
                        if self.pending_message_reveal.is_some() {
                            self.auto_follow = false;
                        } else {
                            let max_y = self.message_scroll.max_offset().y;
                            let off_y = self.message_scroll.offset().y;
                            // gpui scroll offset is non-positive: 0 at the top,
                            // -max at the bottom. "At the bottom" therefore means
                            // the offset has bottomed out near -max — not crossed
                            // the positive max (which a non-positive offset can
                            // never satisfy, so the prior comparison never fired
                            // and tail-follow silently died whenever content
                            // overflowed). The `max <= 0` clause also holds the
                            // follow alive while a streaming/history body is still
                            // shorter than the viewport (max == 0, no overflow),
                            // so the first frame that overflows still re-pins.
                            let at_bottom = max_y <= px(0.5) || off_y <= -max_y + px(1.0);
                            self.auto_follow = at_bottom;
                            if at_bottom {
                                self.message_scroll.scroll_to_bottom();
                            }
                        }
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
                            // of the (empty) message list; otherwise a pixel-
                            // anchored, tail-following conversation column. Each
                            // item is its own `Entity<MessageItem>`, so a streaming
                            // delta only marks that item's entity dirty. The plain
                            // `track_scroll` container anchors the viewport to an
                            // absolute pixel offset, so an item growing below, a
                            // width reflow, or a streaming-to-finalized body swap
                            // never shifts the viewport.
                            .children(hero)
                            .children((!first_screen).then(|| {
                                // Snapshot the item handles out of the read guard so
                                // no borrow spans the element tree. The scroll div's
                                // direct children are one row per item, so
                                // `child_bounds` (and thus `bounds_for_item` /
                                // `scroll_to_item`) key off the item index for
                                // click-to-reveal.
                                let items: Vec<_> = self.conversation.read(cx).items().to_vec();
                                let scroll = self.message_scroll.clone();
                                // Plain pixel-anchored scroll: no `min_h_full` /
                                // `justify_end`. A `min_h_full` against an unbounded
                                // ancestor inflates the scroll content-box to a
                                // near-infinite height, and `justify_end` then pins
                                // the messages to the bottom of that void —
                                // `scroll_to_bottom` lands the viewport in empty
                                // space below the content, so opening a thread shows
                                // nothing until the user wheels up through the gap.
                                // The plain container measures content at its real
                                // height, so `max_offset` tracks the actual overflow
                                // and the tail-follow arbitration below pins to the
                                // true bottom. Short threads sit at the top; that is
                                // the spec'd plain-container trade-off.
                                let list_el = v_flex()
                                    .id("message-list")
                                    .w_full()
                                    .min_w_0()
                                    .overflow_y_scroll()
                                    .track_scroll(&scroll)
                                    // Body typeface: Lilex Light. Every message row
                                    // (assistant, user, reasoning, tool cards, notices)
                                    // inherits from here; markdown bold/headings
                                    // resolve to Medium via nearest-weight, italic syntax
                                    // and tool-card overrides hit the italic cuts.
                                    .font_family(theme.mono_font_family.clone())
                                    .font_weight(gpui::FontWeight::LIGHT)
                                    .children(items.into_iter().enumerate().map(|(ix, item)| {
                                        v_flex()
                                            .id(("msg", ix))
                                            .w_full()
                                            .pt_1()
                                            .pb_4()
                                            .flex_shrink_0()
                                            .min_w_0()
                                            .child(item)
                                    }));
                                let list_wrap = v_flex()
                                    .flex_1()
                                    .h_full()
                                    .min_h_0()
                                    .min_w_0()
                                    .child(list_el);
                                // Outline rail (left) + flat message column (right)
                                // share the scroll region's height.
                                h_flex()
                                    .flex_1()
                                    .w_full()
                                    .min_h_0()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .child(list_wrap)
                            }))
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
                    // Floating context card: absolute top-right of the
                    // conversation column, below the title bar. Its own `Render`
                    // positions it (`top` clears the title bar, `right` + the
                    // body wrapper's `pr` keep the message list clear). Hidden
                    // while the editor pane is open, on the first screen, before
                    // the thread interacts, or below the narrow width gate.
                    .when(show_rail, |this| this.child(self.context_rail.clone()))
            })
            // Right editor pane: a third top-level column when an editor is
            // open (browser/terminal tabs will join it as future right-pane
            // surfaces). Sits outside the middle column so it is not a sibling
            // of the conversation+rail pair.
            .when(right_pane_open, |this| {
                this.child(editor_divider).child(editor_pane)
            })
            .children(turn_navigator_overlay)
            .on_drag_move(cx.listener(
                |this, e: &DragMoveEvent<DraggedEditorDivider>, _window, cx| {
                    // The root fills the window, so its right edge is the
                    // window's right edge and the editor pane's width is the
                    // distance from the cursor to that edge. Clamp both to a
                    // minimum and to leave the middle column at least
                    // `MAIN_MIN_WIDTH` (sidebar + main + divider sit left of
                    // the editor), so dragging wide never overflows the window
                    // or collapses the conversation column. The context card is
                    // hidden while the editor is open, so it does not claim a
                    // width here — the conversation alone holds the middle
                    // column. `sidebar_width` is read live so a wide sidebar
                    // correctly shrinks the available editor envelope.
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
/// Snapshot a thread's working directory as a `SharedString` for the
/// `TerminalPanel` prompt line. Reads the `Thread` entity (not the `Workspace`)
/// so it stays safe inside a `Workspace::update` closure, where reading the
/// `Workspace` itself would double-lease. `None` only when the path is empty.
fn thread_cwd(thread: &Entity<Thread>, cx: &App) -> Option<SharedString> {
    let cwd = thread.read(cx).cwd();
    if cwd.as_os_str().is_empty() {
        None
    } else {
        Some(SharedString::from(cwd.to_string_lossy().to_string()))
    }
}

fn parse_pending_ask(id: String, input: serde_json::Value) -> Option<PendingAsk> {
    let questions = input.get("questions")?.as_array()?;
    // Out-of-range counts violate the tool contract; fall back to the generic
    // approval path so the defensive tool runner can report a model-visible error.
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

/// Append a deferred follow-up to a parked thread without touching the
/// foreground conversation view. The canonical history is rebuilt when the
/// user switches back to this task.
fn append_deferred_turn_to_thread(
    thread: &Entity<Thread>,
    turn: DeferredUserTurn,
    cx: &mut Context<Workspace>,
) {
    use agent::language_model::MessageContent;

    thread.update(cx, |thread, cx| {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn visual_axis(window_width: Pixels, layout: TurnNavigatorLayout) -> Pixels {
        layout.left_inset + (window_width - layout.left_inset - layout.right_inset) / 2.
    }

    #[test]
    fn turn_navigator_centers_on_conversation_without_context_rail() {
        let window_width = px(960.);
        let layout = turn_navigator_layout(window_width, px(260.), None, false);

        assert_eq!(layout.left_inset, px(266.));
        assert_eq!(layout.right_inset, px(0.));
        assert_eq!(visual_axis(window_width, layout), px(613.));
    }

    #[test]
    fn turn_navigator_centers_on_conversation_with_context_rail() {
        let window_width = px(960.);
        let layout = turn_navigator_layout(window_width, px(260.), None, true);

        assert_eq!(layout.right_inset, px(296.));
        assert_eq!(visual_axis(window_width, layout), px(465.));
    }

    #[test]
    fn turn_navigator_accounts_for_resized_sidebar_and_right_pane() {
        let window_width = px(1440.);
        let layout = turn_navigator_layout(window_width, px(320.), Some(px(480.)), false);

        assert_eq!(layout.left_inset, px(326.));
        assert_eq!(layout.right_inset, px(486.));
        assert_eq!(visual_axis(window_width, layout), px(640.));
        assert_eq!(layout.panel_width, px(480.));
    }

    #[test]
    fn turn_navigator_keeps_twelve_pixel_margins_in_narrow_conversation() {
        let layout = turn_navigator_layout(px(600.), px(260.), None, false);

        assert_eq!(layout.panel_width, px(310.));
    }

    #[test]
    fn subagent_tabs_deduplicate_and_focus_by_tool_use_id() {
        let mut tabs = vec![RightTab::Editor];
        let first = ensure_subagent_tab(&mut tabs, "agent-a");
        let repeated = ensure_subagent_tab(&mut tabs, "agent-a");
        let second = ensure_subagent_tab(&mut tabs, "agent-b");

        assert_eq!(first, repeated);
        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(tabs.len(), 3);
        assert!(matches!(&tabs[1], RightTab::Subagent(id) if id == "agent-a"));
        assert!(matches!(&tabs[2], RightTab::Subagent(id) if id == "agent-b"));
    }

    #[test]
    fn clearing_subagent_tabs_preserves_peer_tabs_and_reseats_active() {
        let mut tabs = vec![
            RightTab::Editor,
            RightTab::Subagent("agent-a".into()),
            RightTab::PlanPreview,
            RightTab::Subagent("agent-b".into()),
        ];
        let mut active = 3;

        remove_subagent_tabs(&mut tabs, &mut active);

        assert_eq!(tabs.len(), 2);
        assert!(matches!(tabs[0], RightTab::Editor));
        assert!(matches!(tabs[1], RightTab::PlanPreview));
        assert_eq!(active, 1);
    }
}
