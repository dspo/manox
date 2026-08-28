//! Top-level workspace view.
//!
//! Holds `Entity<ThreadProxy>` (the transitional adapter around the
//! gpui-free `agent::ThreadHandle`, see `thread_proxy`) + `Entity<Sidebar>`;
//! `cx.subscribe` handles:
//! - `ThreadEvent`: text/thinking/tool deltas go to `ConversationState`; `ToolCallAuthorization` opens the question card;
//!   the terminal `Stop` (non-ToolUse) triggers `refresh_thread_list`.
//! - `SidebarEvent`: new conversation / open history / delete.
//!
//! Enter in the input box → append a user message + run_turn + persist (the sidebar shows the new entry immediately).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::thread_proxy::ThreadProxy;
use crate::views::launcher::LauncherPick;
use agent::PermissionDecision;
use agent::collaboration_mode::PlanReviewChoice;
use agent::i18n;
use agent::language_model::StopReason;
use agent::thread::PermissionMode;
use agent::webview_host::BrowserTabId;
use agent::{Thread, ThreadEvent, ThreadId, refresh_thread_list};
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
    ActiveTheme as _, ColorName, Disableable as _, ElementExt as _, Icon, IconName, Sizable as _,
    Size, TITLE_BAR_HEIGHT, Theme, TitleBar,
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
use serde::{Deserialize, Serialize};
use std::rc::Rc;

use crate::client_store_handle::ClientStoreHandle;
use crate::cockpit::{CockpitPhase, format_elapsed};
use crate::conversation::ConvItem;
use crate::conversation::{ApplyOutcome, ConversationState, NoticeAnchor, UserImage, UserTurnMeta};
use crate::external_session::{
    ExternalSession, ResumeSidecar, SessionKind, SessionPlacement, claude_cwd_from_file_head,
    claude_project_dir_for_cwd, claude_session_id_from_file_name, codex_session_id_from_rollout,
    codex_sessions_dir, list_nested_jsonl, list_sidecars, list_top_level_jsonl,
    merge_external_summaries, new_file_names, remove_sidecar, resume_args, write_sidecar,
};
use crate::views::browser_view::BrowserView;
use crate::views::centered;
use crate::views::completion::{
    CompletionState, SelectHandler, build_replacement, detect, mention_source, render_completion,
    slash_source,
};
use crate::views::composer_menu::{
    PendingAttachment, build_plus_menu, load_attachment, render_attachment_chips,
    render_browser_chips,
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
use manox_protocol::RpcConnection;
use terminal::Terminal;
use terminal_ui::TerminalView;
use terminal_ui::terminal_proxy::TerminalProxy;

pub(crate) type ThreadEntity = ThreadProxy;

/// A tab in the right observation pane. `Editor` is the markdown composer
/// (Write/Preview); `Launcher` is the empty-tab launcher offering the
/// built-in browser / terminal / CLI-agent views; `Browser(id)` is an
/// untrusted embedded webview (see [`BrowserView`]); `Session(id)` embeds an
/// [`ExternalSession`]'s terminal (plain PTY or CLI agent TUI).
#[derive(Clone, Debug)]
enum RightTab {
    Editor,
    Launcher,
    Browser(BrowserTabId),

    /// A pi sub-agent's observation panel, keyed by subagent address.
    Subagent(String),
    /// An embedded terminal/CLI-agent session, keyed by `ExternalSession.id`.
    Session(String),
}

/// Persisted shape of a thread's right-pane state — one row per thread in
/// `threads.db` (`thread_right_pane`). The UI layer owns this shape; the db
/// stores opaque TEXT. Subagent tabs are ephemeral by design and never
/// serialized.
#[derive(Serialize, Deserialize)]
struct PersistedRightPane {
    visible: bool,
    active: usize,
    tabs: Vec<PersistedRightTab>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PersistedRightTab {
    Editor,
    Launcher,
    Browser { url: String },

    Session { id: String },
}

/// In-session per-thread right-pane stash: the live tabs (browser views
/// keep their entities across switches), the active index, and visibility.
/// The persistent copy lives in `threads.db`.
struct RightPaneSnapshot {
    tabs: Vec<RightTab>,
    active: usize,
    visible: bool,
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

/// Key context for the composer wrapper. `completion = open` shadows the
/// input's keys for the popover; `composer = recall` is present exactly
/// while a history recall can apply — mid-walk (`recall_index >= 0`) or on
/// an empty input, where Up starts one. Anywhere else the recall bindings
/// must not match: a matched action listener consumes the keystroke even
/// when it does nothing, so the caret's native movement depends on the
/// context gate, not on the handler deferring.
fn composer_key_context(completion_open: bool, recalling: bool, value_empty: bool) -> &'static str {
    if completion_open {
        "completion = open"
    } else if recalling || value_empty {
        "composer = recall"
    } else {
        "composer"
    }
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
    /// γ-2a transitional read path: the `AgentServer`-backed
    /// `ClientStoreHandle`, mirroring kernel state via `ServerNote`s. `None`
    /// until the workspace creates the AgentServer connection (landing
    /// thread); views dual-read store-if-present else `ThreadProxy`. Held on
    /// the workspace for the next wiring step (re-handling the store on
    /// thread switch) — written at landing, read there.
    pub(crate) store: Option<gpui::Entity<ClientStoreHandle>>,
    /// The shared AgentServer this desktop connects to. `None` until the
    /// landing-thread wiring creates it; the bin may inject its own instance.
    /// Held for that injection path, so `dead_code` is allowed.
    #[allow(dead_code)]
    pub(crate) agent_server: Option<std::sync::Arc<manox_session_core::agent_server::AgentServer>>,
    /// γ-3: the client-side connection for sending `FromClient` commands to the
    /// AgentServer (replacing ThreadProxy mutations). None until the landing
    /// thread wires it.
    pub(crate) client_conn: Option<manox_protocol::InProcessConnection>,
    /// γ-3: the AgentServer session_id for the landing thread. Used as the
    /// `session_id` field in `FromClient` commands.
    pub(crate) session_id: Option<String>,
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
    /// gates.
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
    /// Peer right-pane tabs for the editor, launcher, browser, sub-agent
    /// observers, and embedded terminal/CLI sessions. `editor_open` tracks
    /// whether the Editor tab specifically is active.
    right_tabs: Vec<RightTab>,
    active_right_tab: usize,
    /// Right-pane visibility gate, orthogonal to the tab list: hiding the pane
    /// keeps every tab (and its state) alive for the next toggle. Closing the
    /// last tab hides the pane; the TitleBar toggle restores the tabs.
    right_pane_visible: bool,
    /// Per-thread right-pane stash for in-session round trips; the persistent
    /// copy lives in `threads.db` (`thread_right_pane`).
    right_pane_by_thread: HashMap<String, RightPaneSnapshot>,
    /// The tab currently under the mouse — the close `×` reveals on hover.
    hovered_right_tab: Option<usize>,
    /// Generation counter for the browser page-title ticker; bumped when the
    /// last browser tab closes so the prior ticker self-terminates.
    browser_title_ticker_gen: u64,
    /// Provider→model cascade opened from the Launcher's CLI-agent rows.
    /// Created on open, destroyed on close (the model-selector pattern).
    launcher_menu: Option<Entity<PopupMenu>>,
    launcher_menu_sub: Option<Subscription>,
    /// The CLI agent kind the open launcher cascade belongs to — anchors the
    /// popup under its launcher row.
    launcher_menu_kind: Option<SessionKind>,
    /// Live sub-agent observation panels keyed by Agent tool-call id.
    subagent_panels: HashMap<String, Entity<crate::views::subagent_panel::SubagentPanel>>,
    /// Accumulated child-session events per Agent tool-call id, so a panel
    /// opened mid-run backfills from the start.
    subagent_transcripts: HashMap<String, Vec<agent::SubagentChildEvent>>,
    /// Latest completion text per subagent address (from SubagentProgress
    /// status=Success/Error), used as the panel's final answer when the
    /// Agent tool-result is absent (new Steer bus has no ToolResult).
    subagent_final_text: HashMap<String, String>,
    /// The Captain's dispatch prompt per subagent address, captured from the
    /// Steer tool call so a panel always shows the opening user message even
    /// before the child streams anything.
    subagent_prompts: HashMap<String, String>,
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
    /// Access-chip dropdown (permission modes). Mirrors the model selector pattern.
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
    /// Opt-in browser tool suites activated via the `+` menu. Unlike file
    /// attachments these persist across submits (they track session-level tool
    /// activation); removing a chip deactivates the suite.
    active_browser_suites: Vec<agent::pi_engine::BrowserSuite>,
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
    /// Height-invalidation subscription: any `ConversationState` mutation may
    /// change a row's height (including off-screen rows whose height is cached
    /// in the list sum tree). Remeasure all rows on every conversation notify —
    /// the same cure a window resize applies — so a stale cached height can
    /// never survive to paint an overlapping row.
    conversation_sub: Option<Subscription>,
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
    /// Conversation file names already claimed by a live session's CLI-session
    /// watcher, keyed by watched directory — concurrent watchers on the same
    /// directory (two sessions in one cwd) can never claim the same file.
    cli_session_claims: std::collections::HashMap<PathBuf, std::collections::HashSet<String>>,
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
/// Fixed width of every right-pane tab: long labels cap + ellipsis instead
/// of stretching the bar.
const RIGHT_TAB_WIDTH: f32 = 160.;
/// Character cap for right-pane tab labels; longer labels end in `…` and the
/// full text rides the tab's tooltip.
const RIGHT_TAB_LABEL_CAP: usize = 16;

/// Cap a right-pane tab label at [`RIGHT_TAB_LABEL_CAP`] chars + `…`.
fn cap_tab_label(label: &str) -> String {
    let mut chars = label.chars();
    let head: String = chars.by_ref().take(RIGHT_TAB_LABEL_CAP).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}
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

/// The cascade selection backing [`Workspace::spawn_external_session`]; one
/// struct keeps the spawn entry point under clippy's argument cap.
pub(crate) struct ExternalSpawn {
    kind: SessionKind,
    provider_name: String,
    model_id: String,
    /// Cx wire key pinning the endpoint variant (`anthropic` /
    /// `responses` / `completions`); `None` = default derivation.
    wire_api: Option<String>,
    project_cwd: Option<PathBuf>,
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
        let thread = cx.new(|cx| ThreadProxy::new(Thread::landing(cwd.clone()), cx));

        // γ-2a: the landing thread also gets an AgentServer-backed
        // ClientStoreHandle — the desktop's transitional read path.
        let agent_server = std::sync::Arc::new(manox_session_core::agent_server::AgentServer::new(
            cwd.clone(),
        ));
        let (store, client_conn, session_id) = {
            let session_id = uuid::Uuid::new_v4().to_string();
            let (client_conn, server_conn) = manox_protocol::in_process_pair();
            agent_server.accept(std::sync::Arc::new(server_conn));
            client_conn.send_to_server(manox_protocol::FromClient::Request {
                id: manox_protocol::MsgId::new("init"),
                call: manox_protocol::ClientCall::Initialize(manox_protocol::Initialize {
                    client_id: format!("desktop-{session_id}"),
                    capabilities: vec![
                        manox_protocol::handshake::HookKind::Approve,
                        manox_protocol::handshake::HookKind::PlanVerdict,
                        manox_protocol::handshake::HookKind::AskUserQuestion,
                    ],
                    sessions: vec![],
                }),
            });
            client_conn.send_to_server(manox_protocol::FromClient::Notification {
                note: manox_protocol::ClientNote::CreateSession {
                    session_id: session_id.clone(),
                    cwd: Some(cwd.to_str().unwrap_or_default().into()),
                },
            });
            let store = cx.new(|cx| ClientStoreHandle::new(client_conn.clone(), cx));
            (store, client_conn, session_id)
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
        let context_rail = {
            cx.new(|_| {
                crate::views::context_rail::ContextRail::new(thread.clone(), Some(store.clone()))
            })
        };
        let weak_ws = cx.weak_entity();
        context_rail.update(cx, |r, _| r.set_workspace(weak_ws));

        let mut ws = Self {
            cwd,
            thread,
            store: Some(store),
            agent_server: Some(agent_server),
            client_conn: Some(client_conn),
            session_id: Some(session_id),
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
            right_pane_visible: false,
            right_pane_by_thread: HashMap::new(),
            hovered_right_tab: None,
            browser_title_ticker_gen: 0,
            launcher_menu: None,
            launcher_menu_sub: None,
            launcher_menu_kind: None,
            subagent_panels: HashMap::new(),
            subagent_transcripts: HashMap::new(),
            subagent_final_text: HashMap::new(),
            subagent_prompts: HashMap::new(),
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
            active_browser_suites: Vec::new(),
            project_picker_pending: false,
            blank_project_parent: None,
            blank_project_name_input: None,
            thread_sub: None,
            sidebar_sub: None,
            input_sub: None,
            editor_sub: None,
            conversation_sub: None,
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
            cli_session_claims: std::collections::HashMap::new(),
            active_external: None,
        };
        ws.thread_sub = Some(ws.subscribe_thread(cx));
        ws.sidebar_sub = Some(ws.subscribe_sidebar(window, cx));
        ws.input_sub = Some(ws.subscribe_input(window, cx));
        ws.editor_sub = Some(ws.subscribe_editor(window, cx));
        ws.observe_conversation(cx);
        // The sidebar lists the restored resumable rows from the first frame;
        // nothing is resumed until the user clicks one.
        ws.sync_sidebar_external(cx);
        // Focus the composer so typing works immediately on the hero screen.
        ws.input_state.update(cx, |s, cx| s.focus(window, cx));
        ws
    }

    /// Swap the conversation + list for a prebuilt state and re-arm tail
    /// follow. Diagnostic-only entry point used by the full-workspace overlap
    /// walk test to open a real session without the async attach pipeline.
    #[cfg(feature = "test-support")]
    pub fn diagnostic_replace_conversation(
        &mut self,
        conversation: Entity<ConversationState>,
        cx: &mut Context<Self>,
    ) {
        self.conversation = conversation;
        self.observe_conversation(cx);
        let count = self.conversation.read(cx).items().len();
        self.list_state.reset(count);
        self.list_count = count;
        self.list_state.set_follow_mode(FollowMode::Tail);
        cx.notify();
    }

    #[cfg(feature = "test-support")]
    pub fn diagnostic_list_state(&self) -> ListState {
        self.list_state.clone()
    }

    /// Attach a thread through the production switch path. Diagnostic-only
    /// entry point so integration tests can exercise parking + re-surface
    /// without simulating the sidebar click.
    #[cfg(feature = "test-support")]
    pub fn diagnostic_attach_thread(
        &mut self,
        thread: Entity<ThreadEntity>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.attach_thread(thread, window, cx);
    }

    /// Seed a parsed `AskUserQuestion` as the pending ask. Diagnostic-only:
    /// bypasses the engine gate so the synthesis path can be tested with a
    /// fake engine (whose `pending_auth_entries` is empty).
    #[cfg(feature = "test-support")]
    pub fn diagnostic_seed_ask(
        &mut self,
        id: &str,
        input: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        self.pending_ask = parse_pending_ask(id.to_string(), input);
        self.ask_step = 0;
        self.ask_transition_gen = self.ask_transition_gen.wrapping_add(1);
        cx.notify();
    }

    /// Run the missing-card synthesis. Diagnostic-only wrapper around the
    /// private `ensure_ask_tool_item`.
    #[cfg(feature = "test-support")]
    pub fn diagnostic_ensure_ask_tool_item(
        &mut self,
        id: &str,
        summary: &str,
        input: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        self.ensure_ask_tool_item(id, summary, input, cx);
    }

    /// Sync the ask snapshots (render-time path). Diagnostic-only.
    #[cfg(feature = "test-support")]
    pub fn diagnostic_sync_ask_card_snapshots(&mut self, cx: &mut Context<Self>) {
        self.sync_ask_card_snapshots(cx);
    }

    /// Whether the pending ask's card would render interactively: a matching
    /// top-level `ToolCall` item carrying the Workspace-derived snapshot.
    #[cfg(feature = "test-support")]
    pub fn diagnostic_ask_card_interactive(&self, id: &str, cx: &App) -> bool {
        let Some(ix) = self.conversation.read(cx).find_tool(id, cx) else {
            return false;
        };
        self.conversation.read(cx).items()[ix]
            .read(cx)
            .ask_snapshot
            .is_some()
    }

    /// Whether a plan review is currently awaiting a verdict. Diagnostic-only.
    #[cfg(feature = "test-support")]
    pub fn diagnostic_pending_plan_review(&self) -> bool {
        self.pending_plan_review.is_some()
    }

    /// Whether a stashed plan review exists for the given thread.
    /// Diagnostic-only.
    #[cfg(feature = "test-support")]
    pub fn diagnostic_has_stashed_plan(&self, id: &str) -> bool {
        self.pending_plans.contains_key(id)
    }

    /// Whether the conversation's tail item is an *active* plan review card
    /// (verdict buttons rendered). Diagnostic-only.
    #[cfg(feature = "test-support")]
    pub fn diagnostic_tail_plan_active(&self, cx: &App) -> bool {
        let Some(last) = self.conversation.read(cx).items().last() else {
            return false;
        };
        matches!(
            last.read(cx).kind(),
            ConvItem::PlanReview { active: true, .. }
        )
    }

    /// Count of top-level `ToolCall` items with the given id. Diagnostic-only.
    #[cfg(feature = "test-support")]
    pub fn diagnostic_tool_call_count(&self, id: &str, cx: &App) -> usize {
        self.conversation
            .read(cx)
            .items()
            .iter()
            .filter(|item| matches!(item.read(cx).kind(), ConvItem::ToolCall(t) if t.id == id))
            .count()
    }

    /// Remeasure every list row whenever the conversation mutates. A height
    /// change on an off-screen row would otherwise leave the list's cached
    /// height stale until the next width change; this applies that cure
    /// automatically on every conversation notify. Callers must invoke this
    /// after any `self.conversation = ...` reassignment so the subscription
    /// tracks the live entity.
    fn observe_conversation(&mut self, cx: &mut Context<Self>) {
        let list_state = self.list_state.clone();
        self.conversation_sub = Some(cx.observe(&self.conversation, move |_this, _conv, _cx| {
            list_state.remeasure_items(0..list_state.item_count());
        }));
    }

    /// Rebuild the conversation view from the current thread's message
    /// history. The harness-pi restore hook: `PiSession` rebuilds the pi
    /// session asynchronously, then asks the workspace to re-render once the
    /// restored history lands.
    pub(crate) fn rebuild_conversation_from_thread(&mut self, cx: &mut Context<Self>) {
        let messages: Vec<agent::Message> = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.messages.clone())
            .unwrap_or_else(|| self.thread.read(cx).messages().to_vec());
        let display: Vec<agent::db::HistoryEntry> = self
            .store
            .as_ref()
            .and_then(|s| {
                serde_json::from_value::<Vec<agent::db::HistoryEntry>>(
                    s.read(cx).store.display_history.clone(),
                )
                .ok()
            })
            .unwrap_or_else(|| self.thread.read(cx).display_history());
        let usage = self
            .store
            .as_ref()
            .map(|s| {
                s.read(cx)
                    .store
                    .per_request_usage
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            agent::TokenUsage {
                                input_tokens: v.input,
                                output_tokens: v.output,
                                cache_creation_input_tokens: v.cache_creation,
                                cache_read_input_tokens: v.cache_read,
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_else(|| self.thread.read(cx).request_token_usage().clone());
        let role = self.model_label(cx);
        let weak = cx.weak_entity();
        let running = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .unwrap_or_else(|| self.thread.read(cx).is_running());
        let cwd = thread_cwd(&self.thread, &self.store, cx);
        let new_conv = cx.new(|cx| {
            ConversationState::rebuild_from_display(
                &display,
                &usage,
                &role,
                running,
                crate::conversation::ApplyCtx {
                    weak: weak.clone(),
                    cwd,
                },
                cx,
            )
        });
        self.conversation = new_conv;
        self.observe_conversation(cx);
        let count = self.conversation.read(cx).items().len();
        self.list_state.reset(count);
        self.list_count = count;
        // The authoritative list is fully rendered now; future preview
        // batches append from here.
        self.history_rendered = messages.len();
        // `Tail` natively pins to the end and keeps following; an upward user
        // scroll disengages it and landing back at the bottom re-arms it.
        self.list_state.set_follow_mode(FollowMode::Tail);
        // Recover the settled sub-agent observation rows alongside the
        // conversation: a restored transcript is the only record of runs that
        // finished (or were killed) before the restart / switch.
        self.rebuild_subagent_observation(&messages, cx);
        cx.notify();
    }

    fn subscribe_thread(&self, cx: &mut Context<Self>) -> Subscription {
        let thread = self.thread.clone();
        cx.subscribe(&thread, |this, _thread, ev: &ThreadEvent, cx| {
            match ev {
                ThreadEvent::ToolCallAuthorization { id, input, .. } => {
                    // Every interaction — AskUserQuestion calls and bubbled
                    // team-member questions — surfaces as the question card;
                    // the payload carries the decision options.
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
                    let thread_id = this
                        .store
                        .as_ref()
                        .map(|s| s.read(cx).store.id.0.clone())
                        .unwrap_or_else(|| this.thread.read(cx).id.0.clone());
                    let store = agent::thread_store_global();
                    store.with_mut(|s| s.mark_pending_auth(&thread_id, true));
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
                        .update(cx, |t, _| t.set_plan_review_pending(true));
                    // The sidebar row pauses its spinner (blue static) while
                    // the verdict is due; `respond_plan_review` releases it.
                    let thread_id = this
                        .store
                        .as_ref()
                        .map(|s| s.read(cx).store.id.0.clone())
                        .unwrap_or_else(|| this.thread.read(cx).id.0.clone());
                    let store = agent::thread_store_global();
                    store.with_mut(|s| s.mark_pending_plan(&thread_id, true));
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
                ThreadEvent::PermissionModeChanged { .. } => {
                    // Refresh the access chip; no conversation item.
                    cx.notify();
                }
                ThreadEvent::BrowserSuitesChanged { suites } => {
                    // The composer chips are derived state of the thread's
                    // suite mirror (survives thread switches and restores).
                    this.active_browser_suites = suites.clone();
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
                    let messages = this
                        .store
                        .as_ref()
                        .map(|s| s.read(cx).store.messages.clone())
                        .unwrap_or_else(|| this.thread.read(cx).messages().to_vec());
                    if let Some(snapshot) =
                        agent::plan::rebuild_from_messages(&messages).or_else(|| {
                            this.store
                                .as_ref()
                                .and_then(|s| s.read(cx).store.persisted_plan.as_ref())
                                .and_then(|v| {
                                    serde_json::from_value::<agent::plan::PlanSnapshot>(v.clone())
                                        .ok()
                                })
                                .or_else(|| this.thread.read(cx).persisted_plan())
                        })
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
                    if this
                        .store
                        .as_ref()
                        .and_then(|s| {
                            serde_json::from_value::<agent::thread::HistoryPhase>(
                                serde_json::Value::String(s.read(cx).store.history_phase.clone()),
                            )
                            .ok()
                        })
                        .unwrap_or_else(|| this.thread.read(cx).history_phase())
                        .is_loading()
                    {
                        let messages: Vec<agent::Message> = this
                            .store
                            .as_ref()
                            .map(|s| s.read(cx).store.messages.clone())
                            .unwrap_or_else(|| this.thread.read(cx).messages().to_vec());
                        if messages.len() > this.history_rendered {
                            let new_messages = messages[this.history_rendered..].to_vec();
                            let usage = this
                                .store
                                .as_ref()
                                .map(|s| {
                                    s.read(cx)
                                        .store
                                        .per_request_usage
                                        .iter()
                                        .map(|(k, v)| {
                                            (
                                                k.clone(),
                                                agent::TokenUsage {
                                                    input_tokens: v.input,
                                                    output_tokens: v.output,
                                                    cache_creation_input_tokens: v.cache_creation,
                                                    cache_read_input_tokens: v.cache_read,
                                                },
                                            )
                                        })
                                        .collect()
                                })
                                .unwrap_or_else(|| {
                                    this.thread.read(cx).request_token_usage().clone()
                                });
                            let role = this.model_label(cx);
                            let cwd = thread_cwd(&this.thread, &this.store, cx);
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
                ThreadEvent::ModelChanged { from, to } => {
                    // Persist a model_change event to the thread's event stream.
                    // The conversation view itself stays unchanged (no item).
                    let _ = (from, to);
                    cx.notify();
                }
                ThreadEvent::ReasoningEffortChanged { .. } => {
                    // Persist effort change to the thread record immediately.
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
                    let thread_id = this
                        .store
                        .as_ref()
                        .map(|s| s.read(cx).store.id.0.clone())
                        .unwrap_or_else(|| this.thread.read(cx).id.0.clone());
                    let store = agent::thread_store_global();
                    store.with_mut(|s| {
                        s.mark_running(&thread_id);
                        s.set_errored(&thread_id, false);
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
                    let cwd = thread_cwd(&this.thread, &this.store, cx);
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
                    let thread_id = this
                        .store
                        .as_ref()
                        .map(|s| s.read(cx).store.id.0.clone())
                        .unwrap_or_else(|| this.thread.read(cx).id.0.clone());
                    refresh_thread_list();
                    // Sidebar running indicator: the turn released the running
                    // slot, so the row stops spinning. A successful or
                    // cancelled turn also supersedes a stale error flag. The
                    // pending-plan badge survives a normal settle while a
                    // review is still up (the card stays active below) — it
                    // clears only when the verdict is moot or absent.
                    let keep_plan_badge =
                        !(*cancelled || *failed) && this.pending_plan_review.is_some();
                    let store = agent::thread_store_global();
                    store.with_mut(|s| {
                        s.mark_idle(&thread_id);
                        if !keep_plan_badge {
                            s.mark_pending_plan(&thread_id, false);
                        }
                        if !*failed {
                            s.set_errored(&thread_id, false);
                        }
                    });
                    this.turn_active = false;
                    this.background_threads
                        .retain(|b| b.entity.read(cx).id.0 != thread_id);
                    // Only a cancelled/failed turn demotes an outstanding plan
                    // review — the verdict is moot once the loop released the
                    // turn abnormally. A normal settle right after
                    // `ProposePlan` keeps the card active so the user can
                    // still choose a verdict; demoting it here made the card
                    // flash its buttons and collapse to a plain record.
                    if (*cancelled || *failed) && this.pending_plan_review.take().is_some() {
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
                    let usage =
                        this.store
                            .as_ref()
                            .and_then(|s| {
                                s.read(cx).store.last_token_usage.as_ref().map(|u| {
                                    agent::TokenUsage {
                                        input_tokens: u.input,
                                        output_tokens: u.output,
                                        cache_creation_input_tokens: u.cache_creation,
                                        cache_read_input_tokens: u.cache_read,
                                    }
                                })
                            })
                            .or_else(|| this.thread.read(cx).last_request_token_usage());
                    let cwd = thread_cwd(&this.thread, &this.store, cx);
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
                        refresh_thread_list();
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
                                        && this
                                            .store
                                            .as_ref()
                                            .and_then(|s| s.read(cx).store.goal.clone())
                                            .and_then(|v| {
                                                serde_json::from_value::<agent::goal::ThreadGoal>(v)
                                                    .ok()
                                            })
                                            .or_else(|| this.thread.read(cx).goal())
                                            .is_some()
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
                        let thread_id = this
                            .store
                            .as_ref()
                            .map(|s| s.read(cx).store.id.0.clone())
                            .unwrap_or_else(|| this.thread.read(cx).id.0.clone());
                        let store = agent::thread_store_global();
                        store.with_mut(|s| {
                            s.mark_background_work(
                                &thread_id,
                                agent::background_task::thread_has_running_tasks(&thread_id),
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
                        let thread_id = this
                            .store
                            .as_ref()
                            .map(|s| s.read(cx).store.id.0.clone())
                            .unwrap_or_else(|| this.thread.read(cx).id.0.clone());
                        let store = agent::thread_store_global();
                        store.with_mut(|s| s.mark_pending_auth(&thread_id, false));
                    }
                    // `Error` is a terminal signal symmetric to a terminal
                    // `Stop`: the turn aborted, so this thread is no longer
                    // running. Pulled out of the catch-all rather than given a
                    // dedicated arm because the conversation still needs the
                    // generic `apply` below to render the error item.
                    if let ThreadEvent::Error(e) = ev {
                        let thread_id = this
                            .store
                            .as_ref()
                            .map(|s| s.read(cx).store.id.0.clone())
                            .unwrap_or_else(|| this.thread.read(cx).id.0.clone());
                        // Sidebar running indicator: the turn aborted, so the
                        // row stops spinning, flags the error, and surfaces
                        // the unread state for the failed turn. A dead loop
                        // can no longer self-advance, so any background-work
                        // flag is stale and the row goes fully static.
                        let store = agent::thread_store_global();
                        store.with_mut(|s| {
                            s.mark_idle(&thread_id);
                            s.mark_background_work(&thread_id, false);
                            s.mark_pending_plan(&thread_id, false);
                            s.set_errored(&thread_id, true);
                            s.set_unread(&thread_id, true);
                        });
                        this.turn_active = false;
                        this.background_threads
                            .retain(|b| b.entity.read(cx).id.0 != thread_id);
                        // Persist the error card so a reloaded thread reproduces
                        // what went wrong at the failed turn's position. The
                        // append rides the actor queue behind the settling run;
                        // a crash before the actor drains it loses the card
                        // (accepted window for an annotation).
                        this.append_ui_note(agent::db::UiNoteKind::Error, e.to_string(), None, cx);
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
                        health,
                        ..
                    } = ev
                    {
                        let id = id.clone();
                        let subagent_type = subagent_type.clone();
                        let latest_activity = latest_activity.clone();
                        let health = health.clone();
                        this.context_rail.update(cx, |r, cx| {
                            r.apply_subagent_progress(
                                &id,
                                &subagent_type,
                                latest_activity.as_deref(),
                                *status,
                                health.as_deref(),
                                cx,
                            );
                        });
                        // Record the completion text so a panel opened later
                        // (after the Agent tool-result is gone) can show it.
                        if matches!(
                            *status,
                            agent::ToolCallStatus::Success
                                | agent::ToolCallStatus::Error
                                | agent::ToolCallStatus::Denied
                        ) && let Some(text) = &latest_activity
                        {
                            this.subagent_final_text.insert(id.clone(), text.clone());
                        }
                        if let Some(panel) = this.subagent_panels.get(&id) {
                            panel.update(cx, |p, cx| p.set_status(*status, cx));
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
                    // Capture the Captain's dispatch prompt from the Steer
                    // tool call so the subagent panel can show the opening
                    // user message even before the child streams anything.
                    if let ThreadEvent::ToolCall { name, input, .. } = ev
                        && name == "Steer"
                        && let Some(args) = input
                    {
                        // Only a Dispatch (to.spawn set) establishes the
                        // opening prompt; a later Inject must not overwrite the
                        // panel's first user bubble with a mid-run message.
                        let is_dispatch = args.get("to").and_then(|t| t.get("spawn")).is_some();
                        let addr = args
                            .get("to")
                            .and_then(|t| t.get("agent_address"))
                            .and_then(|v| v.as_str());
                        let prompt = args.get("prompt").and_then(|v| v.as_str());
                        if is_dispatch && let (Some(addr), Some(prompt)) = (addr, prompt) {
                            this.subagent_prompts
                                .insert(addr.to_string(), prompt.to_string());
                        }
                    }
                    let weak = cx.weak_entity();
                    let role = this.model_label(cx);
                    let usage =
                        this.store
                            .as_ref()
                            .and_then(|s| {
                                s.read(cx).store.last_token_usage.as_ref().map(|u| {
                                    agent::TokenUsage {
                                        input_tokens: u.input,
                                        output_tokens: u.output,
                                        cache_creation_input_tokens: u.cache_creation,
                                        cache_read_input_tokens: u.cache_read,
                                    }
                                })
                            })
                            .or_else(|| this.thread.read(cx).last_request_token_usage());
                    let cwd = thread_cwd(&this.thread, &this.store, cx);
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
        cx.subscribe(
            &thread,
            move |this, _thread, ev: &ThreadEvent, cx| match ev {
                ThreadEvent::TurnStarted => {
                    let store = agent::thread_store_global();
                    store.with_mut(|s| s.mark_running(&id));
                }
                ThreadEvent::ToolCallAuthorization { .. } => {
                    // A parked thread's question card is not visible; the
                    // sidebar badge is the only signal until the user
                    // switches back and the card re-surfaces.
                    let store = agent::thread_store_global();
                    store.with_mut(|s| s.mark_pending_auth(&id, true));
                }
                ThreadEvent::ToolCall { status, .. }
                    if !matches!(status, agent::thread::ToolCallStatus::PendingApproval) =>
                {
                    // Tool traffic past a parked authorization proves the run
                    // resumed (the verdict resolved); drop the badge. The
                    // `PendingApproval` card itself precedes the authorization
                    // event and must not clear it.
                    let store = agent::thread_store_global();
                    store.with_mut(|s| s.mark_pending_auth(&id, false));
                }
                ThreadEvent::ToolResult { .. } | ThreadEvent::ToolOutput { .. } => {
                    let store = agent::thread_store_global();
                    store.with_mut(|s| s.mark_pending_auth(&id, false));
                }
                ThreadEvent::SteerInjected { message_id } => {
                    this.consume_background_steer(&id, message_id);
                }
                ThreadEvent::PlanReady { plan_file, title } => {
                    // A plan proposed while the thread was parked never reaches
                    // the foreground handler: stash the review so the
                    // switch-back restore (`attach_thread` drains `pending_plans`)
                    // re-surfaces the verdict card, and persist the sidecar flag
                    // so a restart re-emits PlanReady on Ready. The sidebar
                    // keeps the blue-static wait until the verdict lands.
                    let content = std::fs::read_to_string(plan_file).unwrap_or_default();
                    this.pending_plans.insert(
                        id.clone(),
                        PendingPlanReview {
                            plan_file: plan_file.clone(),
                            title: title.clone(),
                            content,
                        },
                    );
                    _thread.update(cx, |t, _| t.set_plan_review_pending(true));
                    let store = agent::thread_store_global();
                    store.with_mut(|s| s.mark_pending_plan(&id, true));
                }
                ThreadEvent::TurnFinished {
                    cancelled,
                    failed,
                    stranded_steer_ids,
                    ..
                } => {
                    // A cancelled/failed turn voids a stashed plan review
                    // (mirroring the foreground demote); a normal settle keeps
                    // it so the switch-back restore re-surfaces the card. The
                    // pending-plan badge stays up while a review is stashed —
                    // the card is not visible until the user switches back.
                    if *cancelled || *failed {
                        this.pending_plans.remove(&id);
                    }
                    let has_stashed_plan = this.pending_plans.contains_key(&id);
                    let store = agent::thread_store_global();
                    store.with_mut(|s| {
                        s.mark_idle(&id);
                        if !has_stashed_plan {
                            s.mark_pending_plan(&id, false);
                        }
                        s.mark_pending_auth(&id, false);
                        if !*failed {
                            s.set_errored(&id, false);
                        }
                        s.set_unread(&id, true);
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
                    // An errored run voids a stashed plan review (the verdict
                    // is moot once the loop bailed out).
                    this.pending_plans.remove(&id);
                    let store = agent::thread_store_global();
                    store.with_mut(|s| {
                        s.mark_idle(&id);
                        s.mark_background_work(&id, false);
                        s.mark_pending_plan(&id, false);
                        s.mark_pending_auth(&id, false);
                        s.set_errored(&id, true);
                        s.set_unread(&id, true);
                    });
                }
                ThreadEvent::BackgroundTaskUpdated { .. } => {
                    let store = agent::thread_store_global();
                    store.with_mut(|s| {
                        s.mark_background_work(
                            &id,
                            agent::background_task::thread_has_running_tasks(&id),
                        );
                        s.set_unread(&id, true);
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
                thread.update(cx, |t, _| {
                    t.insert_user_message_with_content_and_ui_metadata(content, Some(ui));
                });
                flushed = true;
            } else {
                retain.push_back(item);
            }
        }
        if flushed {
            thread.update(cx, |t, _| t.run_turn());
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
                SidebarEvent::SpawnExternalSession(kind, provider, model, wire, project) => {
                    this.spawn_external_session(
                        ExternalSpawn {
                            kind: *kind,
                            provider_name: provider.clone(),
                            model_id: model.clone(),
                            wire_api: wire.clone(),
                            project_cwd: project.clone(),
                        },
                        SessionPlacement::FullWindow,
                        window,
                        cx,
                    );
                }
                SidebarEvent::SpawnPlainSession(kind, project) => {
                    this.spawn_plain_session(
                        *kind,
                        project.clone(),
                        SessionPlacement::FullWindow,
                        window,
                        cx,
                    );
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
                    let is_current = this
                        .store
                        .as_ref()
                        .map(|s| s.read(cx).store.id.0 == *id)
                        .unwrap_or_else(|| this.thread.read(cx).id.0 == *id);
                    let store = agent::thread_store_global();
                    store.with_mut(|s| s.archive_thread(id, *archived));
                    // Sync the in-memory flag so the title-bar menu label stays
                    // fresh when the sidebar archives the currently active thread.
                    if is_current
                        && !this.send_note(|sid| manox_protocol::ClientNote::ArchiveThread {
                            session_id: sid.into(),
                            archived: *archived,
                        })
                    {
                        this.thread.update(cx, |t, _| t.set_archived(*archived));
                    }
                    // Archiving the active thread navigates away to a fresh
                    // empty thread (Hero view) so the user doesn't stare at a
                    // ghost conversation that just vanished from the sidebar.
                    if *archived && is_current {
                        this.start_new_thread(None, window, cx);
                    }
                }
                SidebarEvent::SetThreadTag(id, tag) => {
                    let store = agent::thread_store_global();
                    store.with_mut(|s| s.set_thread_tag(id, tag.clone()));
                }
                SidebarEvent::RemoveProject(path) => {
                    // Unregister the folder; the sidebar drops the group and
                    // its threads fall back to the loose Conversations list.
                    // Conversation history is never touched.
                    let store = agent::thread_store_global();
                    store.with_mut(|s| s.remove_project(&path.to_string_lossy()));
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
            Some(dir) => cx.new(|cx| ThreadProxy::new(Thread::new_in_project(id, dir.clone()), cx)),
            None => cx.new(|cx| ThreadProxy::new(Thread::new_fresh(id, self.cwd.clone()), cx)),
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
            let terminal = match Terminal::spawn(id, self.cwd.clone(), 80, 24, pty) {
                Ok(t) => cx.new(|cx| TerminalProxy::new(t, cx)),
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
    pub(crate) fn spawn_external_session(
        &mut self,
        spawn: ExternalSpawn,
        placement: SessionPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ExternalSpawn {
            kind,
            provider_name,
            model_id,
            wire_api,
            project_cwd,
        } = spawn;
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
        // claude's conversation id is assigned here (up front) so the sidecar
        // targets it on resume without racing any on-disk discovery; codex /
        // copilot name their own sessions (codex is captured by the watcher
        // while live).
        let spawn_cli_session_id =
            (kind == SessionKind::ClaudeCode).then(|| uuid::Uuid::new_v4().to_string());
        let mut spawn_args: Vec<String> = Vec::new();
        if let Some(sid) = &spawn_cli_session_id {
            spawn_args.push("--session-id".into());
            spawn_args.push(sid.clone());
        }
        let mut builder = cx::AgentBuilder::new()
            .agent(agent)
            .pty(true)
            .provider(provider_name.clone())
            .model(model_id.clone())
            .cwd(cwd.clone())
            .passthrough(spawn_args);
        if let Some(w) = &wire_api {
            builder = builder.wire_api(w.clone());
        }
        let handle = match builder.spawn() {
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
        let terminal = match Terminal::spawn(id.clone(), cwd.clone(), 80, 24, Box::new(source)) {
            Ok(t) => cx.new(|cx| TerminalProxy::new(t, cx)),
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
        // `~/.manox/sessions/<id>.sock`, surfaced in the sidebar tag +
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
        // Thread-bound (right-pane tab) sessions are resources of their
        // thread: no sidebar row and no sidecar, so a restart drops them with
        // the tab instead of resurfacing them as top-level resumable rows.
        let thread_bound = matches!(placement, SessionPlacement::RightPane { .. });
        // Persist the session as unclosed from the first frame: the sidecar
        // survives this process (crash or quit), is kept in sync while live,
        // and is deleted only on an explicit close — so the next launch offers
        // exactly the sessions the user never closed.
        let sidecar = (!thread_bound).then(|| ResumeSidecar {
            id: id.clone(),
            agent_id: agent_id.into(),
            cwd: cwd.to_string_lossy().into_owned(),
            project: project_cwd.clone(),
            created_at,
            provider: provider_name,
            model: model_id,
            wire_api: wire_api.clone(),
            title: None,
            cli_session_id: spawn_cli_session_id,
        });
        let watch_cli_session_id = sidecar.as_ref().and_then(|s| s.cli_session_id.clone());
        if let Some(sidecar) = &sidecar
            && let Err(e) = write_sidecar(sidecar)
        {
            tracing::warn!(error = %e, id, "external session sidecar write failed");
        }
        self.external_sessions.push(ExternalSession {
            id: id.clone(),
            kind,
            created_at,
            project: project_cwd,
            title: None,
            cx_session_id,
            socket_path,
            terminal_view: view,
            handle: Some(handle),
            _exit_sub: exit_sub,
            thread_bound,
            sidecar,
        });
        // The watcher's only job is capturing the CLI session id into the
        // sidecar; thread-bound sessions carry none, and starting it would
        // only claim ledger files a top-level watcher in the same cwd needs
        // to record its own sidecar's resume target.
        if !thread_bound {
            self.start_cli_session_watch(&id, kind, &cwd, watch_cli_session_id, cx);
        }
        self.sync_sidebar_external(cx);
        self.place_external_session(&id, placement, window, cx);
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
        placement: SessionPlacement,
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
        let terminal = match Terminal::spawn(id.clone(), cwd, 80, 24, source) {
            Ok(t) => cx.new(|cx| TerminalProxy::new(t, cx)),
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
            thread_bound: matches!(placement, SessionPlacement::RightPane { .. }),
            sidecar: None,
        });
        self.sync_sidebar_external(cx);
        self.place_external_session(&id, placement, window, cx);
    }

    /// Observe a session terminal's lifecycle, shared by the agent and plain
    /// spawn paths: `ChildExit` tears the session down on a natural exit
    /// (e.g. `/exit` or `exit`), and `Title` mirrors the TUI's OSC title into
    /// the sidebar row + titlebar as it changes. The subscription lives on
    /// the session so a later close detaches it before any spurious event.
    fn subscribe_session_terminal(
        &self,
        terminal: &Entity<TerminalProxy>,
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
        let args = resume_args(&sidecar.agent_id, sidecar.cli_session_id.as_deref());
        let provider = sidecar.provider.clone();
        let model = sidecar.model.clone();
        let wire = sidecar.wire_api.clone();
        let cwd = PathBuf::from(&sidecar.cwd);
        let project = sidecar.project.clone();
        let created_at = sidecar.created_at;
        let this = cx.weak_entity();
        cx.spawn_in(window, async move |_window, cx| {
            let handle = match cx
                .background_spawn(async move {
                    let mut builder = cx::AgentBuilder::new()
                        .agent(agent)
                        .pty(true)
                        .provider(provider)
                        .model(model)
                        .cwd(cwd)
                        .passthrough(args);
                    if let Some(w) = wire {
                        builder = builder.wire_api(w);
                    }
                    builder.spawn()
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
                    match terminal::Terminal::spawn(id.clone(), PathBuf::from(&sidecar.cwd), 80, 24, Box::new(source))
                    {
                        Ok(t) => cx.new(|cx| TerminalProxy::new(t, cx)),
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
                // Watch inputs captured before `sidecar` moves into the session.
                let watch_cwd = PathBuf::from(&sidecar.cwd);
                let watch_initial = sidecar.cli_session_id.clone();
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
                    // keeps the row fresh for a possible later exit. Resumes
                    // are always top-level (sidebar row path), never
                    // thread-bound.
                    thread_bound: false,
                    sidecar: Some(sidecar),
                });
                this.start_cli_session_watch(&id, kind, &watch_cwd, watch_initial, cx);
                this.resuming_external.remove(&id);
                this.sync_sidebar_external(cx);
                this.attach_external_session(&id, window, cx);
            });
        })
        .detach();
    }

    /// Watch the CLI's on-disk conversation storage for this live session and
    /// record its native session id in the sidecar the moment it appears —
    /// the capture that lets a later resume target exactly this session's
    /// conversation. claude's id is assigned by manox at spawn
    /// (`--session-id`) and seeded into the snapshot + claims ledger
    /// synchronously, so the watcher there only tracks later forks
    /// (`/clear`, resume-forks); codex names its own sessions, so the watcher
    /// is the primary capture. Runs for fresh spawns and resumes alike. The
    /// task polls every 500ms, self-terminates when the session is removed,
    /// and releases its ledger claims on exit. No-op for kinds without
    /// capture support (copilot / plain terminal).
    fn start_cli_session_watch(
        &mut self,
        id: &str,
        kind: SessionKind,
        cwd: &Path,
        initial_cli_session_id: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // Watch root + synchronous pre-session snapshot: any conversation
        // file appearing after this point belongs to this session.
        let (watch_dir, snapshot, nested, seed_claims) = match kind {
            SessionKind::ClaudeCode => {
                let Some(dir) = claude_project_dir_for_cwd(cwd) else {
                    return;
                };
                let mut snap = list_top_level_jsonl(&dir);
                let mut seeds: Vec<String> = Vec::new();
                if let Some(sid) = &initial_cli_session_id {
                    let name = format!("{sid}.jsonl");
                    snap.insert(name.clone());
                    // Seed the ledger synchronously: without this, a second
                    // same-cwd watcher started inside this watcher's first
                    // tick window (~500ms) could claim this session's file
                    // and record the wrong id in its own sidecar.
                    self.cli_session_claims
                        .entry(dir.clone())
                        .or_default()
                        .insert(name.clone());
                    seeds.push(name);
                }
                (dir, snap, false, seeds)
            }
            SessionKind::Codex => {
                let Some(dir) = codex_sessions_dir() else {
                    return;
                };
                // Full recursive walk of the sessions tree every tick — fine
                // at current scale; date-dir pruning or a slower cadence is
                // the lever if the history grows huge.
                let snap = list_nested_jsonl(&dir);
                (dir, snap, true, Vec::new())
            }
            SessionKind::GithubCopilot | SessionKind::Terminal => return,
        };
        // claude only: a newly appearing sibling project dir whose transcripts
        // carry the session's cwd wins over the computed slug (adoption).
        let canonical_cwd = std::fs::canonicalize(cwd)
            .ok()
            .and_then(|p| p.to_str().map(str::to_string));
        let mut root_dir_names: HashSet<String> = watch_dir
            .parent()
            .and_then(|root| std::fs::read_dir(root).ok())
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        let session_id_key = id.to_string();
        let this = cx.weak_entity();
        cx.spawn(async move |_this, cx| {
            let mut watch_dir = watch_dir;
            let mut snapshot = snapshot;
            let mut claimed_any = initial_cli_session_id.is_some();
            let mut adoption_done = false;
            let mut my_claims: Vec<String> = seed_claims;
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let dir = watch_dir.clone();
                let snap = snapshot.clone();
                let roots = root_dir_names.clone();
                let canonical = canonical_cwd.clone();
                let scan_nested = nested;
                let do_adoption = !nested && !claimed_any && !adoption_done;
                let (candidates, adopted) = cx
                    .background_spawn(async move {
                        let current = if scan_nested {
                            list_nested_jsonl(&dir)
                        } else {
                            list_top_level_jsonl(&dir)
                        };
                        let candidates: Vec<(String, Option<String>)> =
                            new_file_names(&snap, &current)
                                .into_iter()
                                .map(|name| {
                                    let sid = if scan_nested {
                                        codex_session_id_from_rollout(&dir.join(&name))
                                    } else {
                                        claude_session_id_from_file_name(&name)
                                    };
                                    (name, sid)
                                })
                                .collect();
                        let mut adopted: Option<(PathBuf, HashSet<String>)> = None;
                        if do_adoption
                            && let Some(canonical) = &canonical
                            && let Some(root) = dir.parent()
                            && let Ok(entries) = std::fs::read_dir(root)
                        {
                            for e in entries.flatten() {
                                let name = e.file_name().to_string_lossy().into_owned();
                                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                                if !is_dir || roots.contains(&name) {
                                    continue;
                                }
                                let candidate_dir = e.path();
                                let listing = list_top_level_jsonl(&candidate_dir);
                                let matches = listing.iter().any(|f| {
                                    claude_cwd_from_file_head(&candidate_dir.join(f), 8 * 1024)
                                        .as_deref()
                                        == Some(canonical.as_str())
                                });
                                if matches {
                                    adopted = Some((candidate_dir, listing));
                                    break;
                                }
                            }
                        }
                        (candidates, adopted)
                    })
                    .await;
                // Candidates whose id resolved join the snapshot whatever the
                // claim outcome (claimed here, or owned by another watcher);
                // id-less codex rollouts stay out so the next tick retries.
                let resolved_names: Vec<String> = candidates
                    .iter()
                    .filter(|(_, sid)| sid.is_some())
                    .map(|(name, _)| name.clone())
                    .collect();
                let dir_for_claims = watch_dir.clone();
                let claimed_files = this
                    .update(cx, |this, _cx| -> Option<Vec<String>> {
                        if !this
                            .external_sessions
                            .iter()
                            .any(|s| s.id == session_id_key)
                        {
                            return None;
                        }
                        let ledger = this.cli_session_claims.entry(dir_for_claims).or_default();
                        let mut claimed = Vec::new();
                        for (file, sid) in candidates {
                            let Some(sid) = sid else { continue };
                            if !ledger.insert(file.clone()) {
                                continue;
                            }
                            if let Some(session) = this
                                .external_sessions
                                .iter_mut()
                                .find(|s| s.id == session_id_key)
                                && let Some(sidecar) = session.sidecar.as_mut()
                            {
                                sidecar.cli_session_id = Some(sid);
                                if let Err(e) = write_sidecar(sidecar) {
                                    tracing::warn!(
                                        error = %e,
                                        id = session_id_key,
                                        "external session sidecar cli-session-id update failed"
                                    );
                                }
                                claimed.push(file);
                            }
                        }
                        Some(claimed)
                    })
                    .unwrap_or(None);
                let Some(claimed_files) = claimed_files else {
                    break;
                };
                snapshot.extend(resolved_names);
                if !claimed_files.is_empty() {
                    claimed_any = true;
                    my_claims.extend(claimed_files);
                }
                if let Some((dir, _listing)) = adopted {
                    // The adopted dir did not exist at session start, so every
                    // file in it belongs to this session: reset the snapshot to
                    // empty and let the next tick claim them all.
                    watch_dir = dir;
                    snapshot = HashSet::new();
                    root_dir_names.clear();
                    adoption_done = true;
                }
            }
            // The session is gone: release this watcher's claims so the
            // ledger does not grow unboundedly (future watchers snapshot the
            // dirs afresh at their own spawn).
            if !my_claims.is_empty() {
                let _ = this.update(cx, |this, _cx| {
                    this.cli_session_claims.retain(|_dir, claimed| {
                        for name in &my_claims {
                            claimed.remove(name);
                        }
                        !claimed.is_empty()
                    });
                });
            }
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

    /// Route a freshly spawned external session to its placement: full-window
    /// attach (the sidebar-row path) or a right-pane Session tab.
    fn place_external_session(
        &mut self,
        id: &str,
        placement: SessionPlacement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match placement {
            SessionPlacement::FullWindow => self.attach_external_session(id, window, cx),
            SessionPlacement::RightPane { replace_tab } => {
                self.mount_session_tab(id, replace_tab, window, cx)
            }
        }
    }

    /// Mount an external session's terminal on the right pane: replace the
    /// Launcher tab at `replace_tab` while it still holds one, else append a
    /// fresh tab. The terminal gains focus on the next frame so the TUI
    /// receives keystrokes immediately.
    fn mount_session_tab(
        &mut self,
        id: &str,
        replace_tab: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(self.right_tabs.get(replace_tab), Some(RightTab::Launcher)) {
            self.right_tabs[replace_tab] = RightTab::Session(id.to_string());
            self.set_active_right_tab(replace_tab, cx);
        } else {
            self.right_tabs.push(RightTab::Session(id.to_string()));
            self.set_active_right_tab(self.right_tabs.len() - 1, cx);
        }
        self.right_pane_visible = true;
        if let Some(view) = self
            .external_sessions
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.terminal_view.clone())
        {
            self.focus_external_view(view, window, cx);
        }
        cx.notify();
    }

    /// Mirror an external agent's OSC title (`TerminalEvent::Title`) into its
    /// `ExternalSession`, keep the durable sidecar's title in sync, and
    /// refresh the sidebar projection + titlebar when a visible title
    /// changed. No-op when the session was already removed (a spurious title
    /// after close) or the title sanitizes to the stored value.
    fn set_external_title(&mut self, id: &str, title: Option<String>, cx: &mut Context<Self>) {
        let active = self.active_external.as_deref() == Some(id);
        // TUI-set titles arrive verbatim; sanitize so the stored form is a
        // single printable line (control bytes / line breaks would stretch
        // the sidebar row).
        let new = title
            .as_deref()
            .and_then(crate::external_session::sanitize_osc_title);
        let Some(session) = self.external_sessions.iter_mut().find(|s| s.id == id) else {
            return;
        };
        if session.title == new {
            return;
        }
        let thread_bound = session.thread_bound;
        session.title = new.clone();
        // Keep the durable sidecar's title in sync so a later exit offers
        // the resumable row under the latest OSC title rather than the one
        // captured at spawn / last resume.
        if let Some(sidecar) = session.sidecar.as_mut() {
            sidecar.title = new.clone();
            if let Err(e) = write_sidecar(sidecar) {
                tracing::warn!(error = %e, id, "external session sidecar title update failed");
            }
        }
        // Thread-bound sessions never project into the sidebar; rebuilding
        // the projection on their title churn would be pure waste.
        if !thread_bound {
            self.sync_sidebar_external(cx);
        }
        if active {
            cx.notify();
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
        // A Session tab embedding this terminal must not outlive the session
        // (a natural `/exit` closes its tab instead of stranding a dead PTY).
        if let Some(ix) = self
            .right_tabs
            .iter()
            .position(|t| matches!(t, RightTab::Session(sid) if *sid == id))
        {
            self.right_tabs.remove(ix);
            self.reseat_active_after_close(ix, cx);
            self.hide_right_pane_if_empty(cx);
            self.editor_open = self
                .right_tabs
                .get(self.active_right_tab)
                .is_some_and(|t| matches!(t, RightTab::Editor));
        }
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
        // Thread-bound (right-pane) sessions are resources of their thread
        // and never surface in the top-level list.
        let live: Vec<_> = self
            .external_sessions
            .iter()
            .filter(|s| !s.thread_bound)
            .map(|s| s.summary())
            .collect();
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
        let tab_id = self.restore_browser_tab(url, window, cx);
        self.right_pane_visible = true;
        self.right_tabs.push(RightTab::Browser(tab_id));
        self.set_active_right_tab(self.right_tabs.len() - 1, cx);
        tab_id
    }

    /// Poll every browser tab's `<title>` through the host so right-pane tab
    /// labels track the loaded page. Mirrors the thinking-ticker pattern:
    /// bumping `browser_title_ticker_gen` (last browser tab closed, or a new
    /// ticker superseding this one) or an emptied `browser_views` map
    /// self-terminates the loop.
    fn spawn_browser_title_ticker(&mut self, cx: &mut Context<Self>) {
        self.browser_title_ticker_gen = self.browser_title_ticker_gen.wrapping_add(1);
        let entity = cx.entity().clone();
        let ticker_gen = self.browser_title_ticker_gen;
        cx.spawn(async move |_this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(2))
                    .await;
                let alive = entity.read_with(cx, |this, _| {
                    this.browser_title_ticker_gen == ticker_gen && !this.browser_views.is_empty()
                });
                if !alive {
                    break;
                }
                let Some(host) = crate::browser_host::WorkspaceBrowserHost::concrete() else {
                    break;
                };
                // Only tabs on screen need fresh titles: hidden panes and
                // tabs stashed under other threads keep their (hidden)
                // webviews idle instead of polling JS into them every tick.
                let ids: Vec<BrowserTabId> = entity.read_with(cx, |this, _| {
                    if this.right_pane_open() {
                        this.right_tabs
                            .iter()
                            .filter_map(|t| match t {
                                RightTab::Browser(id) => Some(*id),
                                _ => None,
                            })
                            .collect()
                    } else {
                        Vec::new()
                    }
                });
                if ids.is_empty() {
                    continue;
                }
                for id in ids {
                    // A superseded / dead ticker must stop issuing evals.
                    let still =
                        entity.read_with(cx, |this, _| this.browser_title_ticker_gen == ticker_gen);
                    if !still {
                        break;
                    }
                    // `page_title` reads the Workspace inside `inject_script`,
                    // so it must not run under a Workspace lease — plain
                    // `AsyncApp::update`, not `entity.update`.
                    let task = cx.update(|cx| host.page_title(id, cx));
                    if let Ok(title) = task.await
                        && !title.is_empty()
                    {
                        entity.update(cx, |this, cx| {
                            if let Some(view) = this.browser_views.get(&id).cloned() {
                                view.update(cx, |v, cx| v.set_title(title.clone(), cx));
                            }
                        });
                    }
                }
            }
        })
        .detach();
    }

    /// Show only the active browser tab's native webview and hide the rest.
    /// No webview may draw while the pane is hidden or another view mode is
    /// up (the platform view is not a gpui element and ignores layout).
    fn sync_browser_visibility(&mut self, cx: &mut Context<Self>) {
        let active_browser =
            if matches!(self.view_mode, ViewMode::Workspace) && self.right_pane_open() {
                match self.right_tabs.get(self.active_right_tab) {
                    Some(RightTab::Browser(id)) => Some(*id),
                    _ => None,
                }
            } else {
                None
            };
        for (id, view) in self.browser_views.iter() {
            let should_show = active_browser == Some(*id);
            view.update(cx, |v, cx| {
                let wv = v.webview().clone();
                match (should_show, wv.read(cx).visible()) {
                    (true, false) => wv.update(cx, |w, _| w.show()),
                    (false, true) => wv.update(cx, |w, _| w.hide()),
                    _ => {}
                }
            });
        }
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
        if self.browser_views.is_empty() {
            // Last browser tab gone — retire the title ticker.
            self.browser_title_ticker_gen = self.browser_title_ticker_gen.wrapping_add(1);
        }
        if let Some(ix) = self
            .right_tabs
            .iter()
            .position(|t| matches!(t, RightTab::Browser(id) if *id == tab_id))
        {
            self.right_tabs.remove(ix);
            self.reseat_active_after_close(ix, cx);
            self.hide_right_pane_if_empty(cx);
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
                InputEvent::Change => {
                    this.exit_diverged_recall(cx);
                    this.sync_completion(window, cx);
                }
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

    /// Splice a single newly inserted conversation item at `ix` into
    /// `list_state`. Mid-list insertions (anchored notices) can't ride the
    /// tail-diff in `sync_list_count`, so this splices at the exact position
    /// and bumps `list_count` to keep the two reconciliations consistent (a
    /// later `sync_list_count` sees an equal count and is a no-op).
    fn apply_list_insert(&mut self, ix: usize) {
        self.list_state.splice(ix..ix, 1);
        self.list_count += 1;
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
        self.clear_subagent_observation(cx);
        // The right pane belongs to a thread: stash the outgoing pane (live
        // tabs, active index, visibility) to the in-session map + threads.db
        // before the incoming thread rebinds. Subagent tabs were already
        // stripped above, so the stash never carries ephemeral entries.
        self.stash_right_pane(old_id.clone(), cx);

        // Save the outgoing thread's unsent composer text before switching, so
        // a draft survives a round-trip through another thread (Bug 1). A
        // thread that just submitted already cleared its input, storing "".
        self.drafts.insert(
            old_id.clone(),
            self.input_state.read(cx).value().to_string(),
        );
        // A recall walk belongs to the outgoing thread's input; the derived
        // state would self-correct anyway, but the draft stash is the
        // natural place to drop it.
        self.recall_index = -1;
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
                store.with_mut(|s| s.mark_running(&old_id));
            }
            if agent::background_task::thread_has_running_tasks(&old_id) {
                let store = agent::thread_store_global();
                store.with_mut(|s| s.mark_background_work(&old_id, true));
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

        self.thread = new_thread;
        // The chips derive from the bound thread's authoritative mirror; the
        // previous thread's suites never bleed across the switch.
        self.active_browser_suites = self
            .store
            .as_ref()
            .map(|s| {
                s.read(cx)
                    .store
                    .browser_suites
                    .iter()
                    .filter_map(|b| {
                        serde_json::from_value::<agent::pi_engine::BrowserSuite>(
                            serde_json::Value::String(b.clone()),
                        )
                        .ok()
                    })
                    .collect()
            })
            .unwrap_or_else(|| self.thread.read(cx).browser_suites());
        let id = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.id.0.clone())
            .unwrap_or_else(|| self.thread.read(cx).id.0.clone());
        let messages: Vec<agent::Message> = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.messages.clone())
            .unwrap_or_else(|| self.thread.read(cx).messages().to_vec());
        let display: Vec<agent::db::HistoryEntry> = self
            .store
            .as_ref()
            .and_then(|s| {
                serde_json::from_value::<Vec<agent::db::HistoryEntry>>(
                    s.read(cx).store.display_history.clone(),
                )
                .ok()
            })
            .unwrap_or_else(|| self.thread.read(cx).display_history());
        let usage = self
            .store
            .as_ref()
            .map(|s| {
                s.read(cx)
                    .store
                    .per_request_usage
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.clone(),
                            agent::TokenUsage {
                                input_tokens: v.input,
                                output_tokens: v.output,
                                cache_creation_input_tokens: v.cache_creation,
                                cache_read_input_tokens: v.cache_read,
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_else(|| self.thread.read(cx).request_token_usage().clone());
        let background_tasks = self
            .store
            .as_ref()
            .map(|s| {
                s.read(cx)
                    .store
                    .background_tasks
                    .iter()
                    .filter_map(|t| {
                        serde_json::from_value::<agent::background_task::TaskSnapshot>(t.clone())
                            .ok()
                    })
                    .collect()
            })
            .unwrap_or_else(|| self.thread.read(cx).background_task_snapshots());
        let role = self.model_label(cx);
        let weak = cx.weak_entity();
        let running = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .unwrap_or_else(|| self.thread.read(cx).is_running());
        let cwd = thread_cwd(&self.thread, &self.store, cx);
        let new_conv = cx.new(|cx| {
            let mut conversation = ConversationState::rebuild_from_display(
                &display,
                &usage,
                &role,
                running,
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
        self.observe_conversation(cx);
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
        // Restore the incoming thread's right pane: the in-session stash,
        // else the threads.db snapshot; a thread with neither gets the empty
        // hidden pane.
        self.restore_right_pane(&new_id, window, cx);
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
        let restored_plan = agent::plan::rebuild_from_messages(&messages).or_else(|| {
            self.store
                .as_ref()
                .and_then(|s| s.read(cx).store.persisted_plan.as_ref())
                .and_then(|v| serde_json::from_value::<agent::plan::PlanSnapshot>(v.clone()).ok())
                .or_else(|| self.thread.read(cx).persisted_plan())
        });
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
        // Recover the settled sub-agent observation rows for the incoming
        // thread after the rail reset above (a restoring thread has no
        // messages yet — its rows land with the `HistoryRestored` rebuild).
        self.rebuild_subagent_observation(&messages, cx);
        // If the new thread has pending interactions (e.g. it was parked
        // waiting for a user answer), re-surface them so the question card
        // appears immediately upon switching back.
        self.resurface_pending_auths(cx);
        self.sidebar
            .update(cx, |s, cx| s.set_selected(Some(id.clone()), cx));
        // The user is now viewing this thread: clear any unread red dot it
        // carried from a prior background completion, and any pending-auth
        // badge (the verdict card re-surfaced below if one is outstanding).
        let store = agent::thread_store_global();
        store.with_mut(|s| {
            s.set_unread(&id, false);
            s.mark_pending_auth(&id, false);
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
        let cwd = self
            .store
            .as_ref()
            .map(|s| std::path::PathBuf::from(s.read(cx).store.cwd.clone()))
            .unwrap_or_else(|| self.thread.read(cx).cwd().to_path_buf());
        let worktree_branch = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.branch.clone())
            .or_else(|| self.thread.read(cx).worktree_branch());
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

    /// Re-surface any pending interaction on the current thread that was
    /// emitted while the thread was in the background (no subscription). Called
    /// after switching threads so the question card appears without requiring
    /// the user to wait for the next event.
    fn resurface_pending_auths(&mut self, cx: &mut Context<Self>) {
        // Query the thread for any pending interaction metadata that was
        // stored when the auth event was originally emitted. If the thread was
        // parked waiting for a user answer while in the background, re-surface
        // the events so the question card appears immediately upon switching
        // back. Every interaction surfaces as the AskUserQuestion card.
        let entries: Vec<(String, String, serde_json::Value)> = self
            .thread
            .read(cx)
            .pending_auth_entries()
            .into_iter()
            .map(|(id, meta)| (id, meta.summary, meta.input))
            .collect();
        for (id, summary, input) in entries {
            self.pending_ask = parse_pending_ask(id.clone(), input.clone());
            self.ask_step = 0;
            self.ask_transition_gen = self.ask_transition_gen.wrapping_add(1);
            // The card renders on a top-level `ToolCall` item. The rebuilt
            // conversation may lack it (a parked thread never saw the live
            // `ToolCall` event, or a stale mirror missed the ask). Synthesize
            // the card the gate would have created live so the interactive
            // drawer renders.
            if self.pending_ask.is_some() {
                self.ensure_ask_tool_item(&id, &summary, input, cx);
            }
        }
        if self.pending_ask.is_some() {
            cx.notify();
        }
    }

    /// Synthesize the top-level AskUserQuestion card when the rebuilt
    /// conversation lacks the matching `ToolCall` item. The live card is
    /// created by the gate's `ToolCall` event, which a parked thread never
    /// sees; the rebuild can miss the mirror's sync window. Without the item
    /// the ask snapshot cannot attach, so the interaction UI never renders.
    fn ensure_ask_tool_item(
        &mut self,
        id: &str,
        summary: &str,
        input: serde_json::Value,
        cx: &mut Context<Self>,
    ) {
        if self.conversation.read(cx).find_tool(id, cx).is_some() {
            return;
        }
        let title = if summary.trim().is_empty() {
            i18n::t("workspace-clarify-title").to_string()
        } else {
            summary.to_string()
        };
        let role = self.model_label(cx);
        let weak = cx.weak_entity();
        self.conversation.update(cx, |conversation, cx| {
            conversation.push_tool_call(
                crate::conversation::ToolCallItem {
                    id: id.to_string(),
                    name: agent::tools::ASK_USER_QUESTION.to_string(),
                    title,
                    status: agent::thread::ToolCallStatus::PendingApproval,
                    output: String::new(),
                    is_error: false,
                    input,
                    streaming: false,
                    collapsed: false,
                    user_toggled: false,
                    panel: None,
                },
                role,
                weak,
                cx,
            );
        });
        self.sync_list_count(cx);
        self.list_state.set_follow_mode(FollowMode::Tail);
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
        if self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .unwrap_or_else(|| self.thread.read(cx).is_running())
        {
            return false;
        }
        let id = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.id.0.clone())
            .unwrap_or_else(|| self.thread.read(cx).id.0.clone());
        if !self.send_note(|sid| manox_protocol::ClientNote::ArchiveThread {
            session_id: sid.into(),
            archived: true,
        }) {
            self.thread.update(cx, |t, _| t.set_archived(true));
        }
        let store = agent::thread_store_global();
        store.with_mut(|s| s.archive_thread(&id, true));
        true
    }

    /// Archive the active thread and open a fresh one that inherits the
    /// outgoing thread's project, model, permission mode, and reasoning effort —
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
        let cwd = self
            .store
            .as_ref()
            .map(|s| std::path::PathBuf::from(s.read(cx).store.cwd.clone()))
            .unwrap_or_else(|| old.read(cx).cwd().to_path_buf());
        let project = self
            .store
            .as_ref()
            .and_then(|s| {
                s.read(cx)
                    .store
                    .project
                    .clone()
                    .map(std::path::PathBuf::from)
            })
            .or_else(|| old.read(cx).project());
        let model = old.read(cx).model();
        let effort = self
            .store
            .as_ref()
            .and_then(|s| {
                serde_json::from_value::<agent::language_model::ReasoningEffort>(
                    serde_json::Value::String(s.read(cx).store.reasoning_effort.clone()),
                )
                .ok()
            })
            .unwrap_or_else(|| old.read(cx).reasoning_effort());
        let permission = self
            .store
            .as_ref()
            .and_then(|s| {
                serde_json::from_value::<agent::thread::PermissionMode>(serde_json::Value::String(
                    s.read(cx).store.permission_mode.clone(),
                ))
                .ok()
            })
            .unwrap_or_else(|| old.read(cx).permission_mode());
        let new = match &project {
            Some(dir) => cx.new(|cx| {
                ThreadProxy::new(
                    Thread::new_in_project(ThreadId(uuid::Uuid::new_v4().to_string()), dir.clone()),
                    cx,
                )
            }),
            None => cx.new(|cx| {
                ThreadProxy::new(
                    Thread::new_fresh(ThreadId(uuid::Uuid::new_v4().to_string()), cwd),
                    cx,
                )
            }),
        };
        new.update(cx, |t, _| {
            if let Some(model) = model {
                t.set_model(model);
            }
            t.set_reasoning_effort(effort);
            t.set_permission_mode(permission);
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
        if !self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .unwrap_or_else(|| self.thread.read(cx).is_running())
        {
            return;
        }
        let project = self
            .store
            .as_ref()
            .and_then(|s| {
                s.read(cx)
                    .store
                    .project
                    .clone()
                    .map(std::path::PathBuf::from)
            })
            .or_else(|| self.thread.read(cx).project());
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
        let Some(loaded) = store.with_mut(|s| s.load_thread(&id)) else {
            return;
        };
        let loaded = cx.new(|cx| ThreadProxy::new(loaded, cx));
        self.attach_thread(loaded, window, cx);
    }

    pub(crate) fn submit_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // History is still loading: nothing may be submitted (the composer is
        // hidden, this guard covers keyboard paths that bypass the render).
        if self
            .store
            .as_ref()
            .and_then(|s| {
                serde_json::from_value::<agent::thread::HistoryPhase>(serde_json::Value::String(
                    s.read(cx).store.history_phase.clone(),
                ))
                .ok()
            })
            .unwrap_or_else(|| self.thread.read(cx).history_phase())
            .is_loading()
        {
            return;
        }
        let text = self.input_state.read(cx).value().to_string();
        let attachments = std::mem::take(&mut self.pending_attachments);
        if self.pending_ask.is_some() {
            self.pending_attachments = attachments;
            if !text.trim().is_empty() || self.pending_ask_has_selection() {
                self.input_state
                    .update(cx, |state, cx| state.set_value("", window, cx));
                // Submission empties the composer; the walk is derived state
                // an empty value can never satisfy, so drop it explicitly
                // (set_value emits no Change to trigger the divergence reset).
                self.recall_index = -1;
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
        // Submission empties the composer; the walk is derived state an empty
        // value can never satisfy, so drop it explicitly (set_value emits no
        // Change to trigger the divergence reset).
        self.recall_index = -1;
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
        if !self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .unwrap_or_else(|| self.thread.read(cx).is_running())
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
                                let format = match img.format {
                                    gpui::ImageFormat::Png => {
                                        Some(agent::image::PastedImageFormat::Png)
                                    }
                                    gpui::ImageFormat::Jpeg => {
                                        Some(agent::image::PastedImageFormat::Jpeg)
                                    }
                                    gpui::ImageFormat::Webp => {
                                        Some(agent::image::PastedImageFormat::Webp)
                                    }
                                    gpui::ImageFormat::Gif => {
                                        Some(agent::image::PastedImageFormat::Gif)
                                    }
                                    gpui::ImageFormat::Bmp => {
                                        Some(agent::image::PastedImageFormat::Bmp)
                                    }
                                    gpui::ImageFormat::Tiff => {
                                        Some(agent::image::PastedImageFormat::Tiff)
                                    }
                                    // SVG/ICO/PNM aren't raster-decodable; drop them.
                                    _ => None,
                                };
                                let content = format.and_then(|format| {
                                    agent::image::pasted_image_to_message_content(
                                        &agent::image::PastedImage {
                                            format,
                                            bytes: img.bytes.to_vec(),
                                        },
                                    )
                                });
                                match content {
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
                    this.add_info_message(
                        i18n::t("composer-image-process-failed").to_string(),
                        NoticeAnchor::TurnEnd,
                        None,
                        cx,
                    );
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
        let hit = match kind {
            RegistryTurnKind::Command => {
                agent::command::try_global().is_some_and(|r| r.get(key).is_some())
            }
            RegistryTurnKind::Skill => {
                agent::skill::try_global().is_some_and(|r| r.get(key).is_some())
            }
        };
        if hit {
            let submit_text = format!("/{key} {args}");
            if !self.send_note(|sid| manox_protocol::ClientNote::Submit {
                session_id: sid.into(),
                text: submit_text,
                images: Vec::new(),
                client_id: None,
            }) {
                self.thread.update(cx, |thread, _| match kind {
                    RegistryTurnKind::Command => thread.submit_command(key, args, Some(ui.clone())),
                    RegistryTurnKind::Skill => thread.submit_skill(key, args, Some(ui)),
                });
            }
        } else {
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
        refresh_thread_list();
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
        if self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .unwrap_or_else(|| self.thread.read(cx).is_running())
        {
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
                .update(cx, |t, _| t.set_plan_review_pending(false));
            self.conversation
                .update(cx, |c, cx| c.consume_plan_review(cx));
            // The demoted plan card stays in place but flips inactive —
            // remeasure so the list's cached height for it stays honest.
            self.list_state.remeasure();
            // Persist the dismissed plan as a UI note so the collapsed record
            // survives a thread switch / reload — the live card is UI-only and
            // never enters `Thread::messages`. The append lands before the
            // dismissing message's prompt dispatch below (single actor
            // queue), so the rebuilt conversation shows the card ahead of
            // this dismissing message — matching the live order.
            self.append_ui_note(
                agent::db::UiNoteKind::PlanReview,
                review.content.clone(),
                None,
                cx,
            );
        }
        self.append_and_run_user_turn(turn, weak, cx);
        // The conversation exists the moment the message is sent: refresh
        // the sidebar list now (the transcript-side refresh on the user
        // MessageEnd notice then fills in the summary text).
        refresh_thread_list();
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
            .update(cx, |thread, _| thread.enqueue_steer(content, Some(ui)))
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
        use agent::language_model::MessageContent;
        use base64::Engine as _;
        // UI state tracking (always) — conversation bubble + list housekeeping.
        self.conversation.update(cx, |c, cx| {
            c.push_user(
                turn.text.clone(),
                turn.user_images.clone(),
                turn.meta.clone(),
                weak,
                cx,
            )
        });
        self.sync_list_count(cx);
        self.follow_message_tail();
        // Dual-path: protocol Submit (kernel inserts + runs) vs direct insert + run.
        let attachments: Vec<manox_protocol::ImageAttachment> = turn
            .images
            .iter()
            .filter_map(|c| match c {
                MessageContent::Image { data, mime_type } => {
                    base64::engine::general_purpose::STANDARD
                        .decode(data.as_bytes())
                        .ok()
                        .map(|bytes| manox_protocol::ImageAttachment {
                            data: bytes,
                            mime_type: mime_type.clone(),
                        })
                }
                _ => None,
            })
            .collect();
        if !self.send_note(|sid| manox_protocol::ClientNote::Submit {
            session_id: sid.into(),
            text: turn.text.clone(),
            images: attachments,
            client_id: None,
        }) {
            self.thread.update(cx, |thread, _| {
                if turn.images.is_empty() {
                    thread.insert_user_message_with_ui_metadata(turn.text, Some(turn.ui));
                } else {
                    let mut content = Vec::with_capacity(turn.images.len() + 1);
                    if !turn.text.trim().is_empty() {
                        content.push(MessageContent::Text(turn.text));
                    }
                    content.extend(turn.images);
                    thread.insert_user_message_with_content_and_ui_metadata(content, Some(turn.ui));
                }
            });
            self.thread.update(cx, |thread, _| thread.run_turn());
        }
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
        use agent::language_model::MessageContent;
        use base64::Engine as _;
        let weak = cx.weak_entity();
        let follow_tail = self.list_state.is_following_tail();
        let mut retain: Vec<QueuedFollowUp> = Vec::new();
        let mut drained_turns: Vec<DeferredUserTurn> = Vec::new();
        while let Some(item) = self.queued_follow_ups.pop_front() {
            match item.state {
                FollowUpState::Queued => {
                    self.conversation.update(cx, |c, cx| {
                        c.push_user(
                            item.turn.text.clone(),
                            item.turn.user_images.clone(),
                            item.turn.meta.clone(),
                            weak.clone(),
                            cx,
                        )
                    });
                    self.sync_list_count(cx);
                    if follow_tail {
                        self.follow_message_tail();
                    }
                    drained_turns.push(item.turn);
                }
                _ => retain.push(item),
            }
        }
        self.queued_follow_ups.extend(retain);
        if drained_turns.is_empty() {
            cx.notify();
            return;
        }
        // Dual-path: protocol (AppendUserMessage for all-but-last + Submit for
        // last) vs direct (insert each + run_turn once).
        let protocol = self.client_conn.is_some() && self.session_id.is_some();
        if protocol {
            let n = drained_turns.len();
            for (i, turn) in drained_turns.into_iter().enumerate() {
                let attachments: Vec<manox_protocol::ImageAttachment> = turn
                    .images
                    .iter()
                    .filter_map(|c| match c {
                        MessageContent::Image { data, mime_type } => {
                            base64::engine::general_purpose::STANDARD
                                .decode(data.as_bytes())
                                .ok()
                                .map(|bytes| manox_protocol::ImageAttachment {
                                    data: bytes,
                                    mime_type: mime_type.clone(),
                                })
                        }
                        _ => None,
                    })
                    .collect();
                if i + 1 < n {
                    // All but the last: insert without running.
                    let _ = self.send_note(|sid| manox_protocol::ClientNote::AppendUserMessage {
                        session_id: sid.into(),
                        text: turn.text.clone(),
                        images: attachments,
                    });
                } else {
                    // Last: insert + start the turn.
                    let _ = self.send_note(|sid| manox_protocol::ClientNote::Submit {
                        session_id: sid.into(),
                        text: turn.text.clone(),
                        images: attachments,
                        client_id: None,
                    });
                }
            }
        } else {
            for turn in drained_turns {
                self.thread.update(cx, |thread, _| {
                    if turn.images.is_empty() {
                        thread.insert_user_message_with_ui_metadata(turn.text, Some(turn.ui));
                    } else {
                        let mut content = Vec::with_capacity(turn.images.len() + 1);
                        if !turn.text.trim().is_empty() {
                            content.push(MessageContent::Text(turn.text));
                        }
                        content.extend(turn.images);
                        thread.insert_user_message_with_content_and_ui_metadata(
                            content,
                            Some(turn.ui),
                        );
                    }
                });
            }
            self.thread.update(cx, |thread, _| thread.run_turn());
        }
        refresh_thread_list();
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
        let running = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .unwrap_or_else(|| self.thread.read(cx).is_running());
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

    /// Whether the right pane is actually on screen: the visibility gate is
    /// up AND at least one tab exists. Everything layout-related (main view
    /// sub-columns, composer/rail suppression, width math) keys off this.
    fn right_pane_open(&self) -> bool {
        self.right_pane_visible && !self.right_tabs.is_empty()
    }

    /// Closing the last tab hides the pane; the TitleBar toggle restores the
    /// surviving tabs on the next open.
    fn hide_right_pane_if_empty(&mut self, cx: &mut Context<Self>) {
        if self.right_tabs.is_empty() {
            self.right_pane_visible = false;
        }
        if self
            .hovered_right_tab
            .is_some_and(|ix| ix >= self.right_tabs.len())
        {
            self.hovered_right_tab = None;
        }
        self.persist_right_pane(cx);
    }

    /// Snapshot the live pane into its persistable form: subagent tabs drop
    /// out (ephemeral) and the active index remaps into the filtered list
    /// (falling back to the head when the active tab was one).
    fn persisted_right_pane(&self, cx: &App) -> PersistedRightPane {
        let mut tabs: Vec<PersistedRightTab> = Vec::new();
        let mut active = 0usize;
        for (ix, tab) in self.right_tabs.iter().enumerate() {
            let persisted = match tab {
                RightTab::Editor => Some(PersistedRightTab::Editor),
                RightTab::Launcher => Some(PersistedRightTab::Launcher),
                RightTab::Browser(id) => {
                    let url = self
                        .browser_views
                        .get(id)
                        .map(|v| v.read(cx).url().to_string())
                        .unwrap_or_default();
                    Some(PersistedRightTab::Browser { url })
                }
                RightTab::Session(id) => Some(PersistedRightTab::Session { id: id.clone() }),
                RightTab::Subagent(_) => None,
            };
            if let Some(p) = persisted {
                if ix == self.active_right_tab {
                    active = tabs.len();
                }
                tabs.push(p);
            }
        }
        PersistedRightPane {
            visible: self.right_pane_visible,
            active,
            tabs,
        }
    }

    /// Write the foreground thread's live pane state to `threads.db`. The
    /// mutation primitives (tab activate/close, visibility toggle) call this,
    /// so the persisted row tracks the pane on every change.
    fn persist_right_pane(&mut self, cx: &mut Context<Self>) {
        let thread_id = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.id.0.clone())
            .unwrap_or_else(|| self.thread.read(cx).id.0.clone());
        let persisted = self.persisted_right_pane(cx);
        let json = match serde_json::to_string(&persisted) {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "serialize right pane failed");
                return;
            }
        };
        let db = agent::thread_store_global().read(|s| s.db().clone());
        if let Err(e) = db.upsert_right_pane(&thread_id, &json) {
            tracing::warn!(error = %e, thread_id = %thread_id, "persist right pane failed");
        }
    }

    /// Per-thread isolation: move the outgoing thread's live pane (tabs,
    /// active index, visibility) into the in-session stash and persist it to
    /// `threads.db`, then reset the pane to the empty state for the incoming
    /// thread.
    fn stash_right_pane(&mut self, thread_id: String, cx: &mut Context<Self>) {
        let persisted = self.persisted_right_pane(cx);
        let json = serde_json::to_string(&persisted)
            .inspect_err(|e| tracing::warn!(error = %e, "serialize right pane failed"))
            .ok();
        if let Some(json) = json {
            let db = agent::thread_store_global().read(|s| s.db().clone());
            if let Err(e) = db.upsert_right_pane(&thread_id, &json) {
                tracing::warn!(error = %e, thread_id = %thread_id, "persist right pane failed");
            }
        }
        // The cascade belongs to the outgoing thread's launcher row; a
        // leftover popup would resurface stale (and with a stale tab index)
        // when the pane restores.
        self.close_launcher_menu();
        self.right_pane_by_thread.insert(
            thread_id,
            RightPaneSnapshot {
                tabs: std::mem::take(&mut self.right_tabs),
                active: self.active_right_tab,
                visible: self.right_pane_visible,
            },
        );
        self.active_right_tab = 0;
        self.right_pane_visible = false;
        self.hovered_right_tab = None;
        self.editor_open = false;
    }

    /// Restore the incoming thread's pane: the in-session stash first (live
    /// tabs keep their webview/panel entities), else the `threads.db`
    /// snapshot re-materialized; a thread with neither gets the empty hidden
    /// pane.
    fn restore_right_pane(&mut self, thread_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        match self.right_pane_by_thread.remove(thread_id) {
            Some(snapshot) => {
                // A stashed tab may outlive its backing entity: browser
                // views can be closed and external sessions can exit while
                // another thread is foreground. Drop the orphaned tabs.
                let mut tabs = snapshot.tabs;
                tabs.retain(|t| match t {
                    RightTab::Browser(id) => self.browser_views.contains_key(id),
                    RightTab::Session(id) => self.external_sessions.iter().any(|s| s.id == *id),
                    _ => true,
                });
                self.right_tabs = tabs;
                self.right_pane_visible = snapshot.visible;
                self.hovered_right_tab = None;
                if self.right_tabs.is_empty() {
                    self.active_right_tab = 0;
                    self.right_pane_visible = false;
                    self.editor_open = false;
                } else {
                    let active = snapshot.active.min(self.right_tabs.len() - 1);
                    self.set_active_right_tab(active, cx);
                }
            }
            None => {
                let loaded =
                    agent::thread_store_global().read(|s| s.db().load_right_pane(thread_id));
                let persisted = match loaded {
                    Ok(Some(json)) => match serde_json::from_str::<PersistedRightPane>(&json) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            tracing::warn!(error = %e, thread_id = %thread_id, "decode right pane snapshot failed");
                            None
                        }
                    },
                    Ok(None) => None,
                    Err(e) => {
                        tracing::warn!(error = %e, thread_id = %thread_id, "load right pane snapshot failed");
                        None
                    }
                };
                self.right_tabs.clear();
                self.hovered_right_tab = None;
                if let Some(persisted) = persisted {
                    self.materialize_right_pane(persisted, window, cx);
                } else {
                    self.active_right_tab = 0;
                    self.right_pane_visible = false;
                    self.editor_open = false;
                }
            }
        }
    }

    /// Rebuild a persisted pane into live tabs. Browser tabs recreate their
    /// webview from the stored URL (the app-restart path); session tabs
    /// restore only while the external session is still alive — everything
    /// else drops silently.
    fn materialize_right_pane(
        &mut self,
        persisted: PersistedRightPane,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for tab in persisted.tabs {
            match tab {
                PersistedRightTab::Editor => self.right_tabs.push(RightTab::Editor),
                PersistedRightTab::Launcher => self.right_tabs.push(RightTab::Launcher),
                PersistedRightTab::Browser { url } => {
                    let id = self.restore_browser_tab(&url, window, cx);
                    self.right_tabs.push(RightTab::Browser(id));
                }
                PersistedRightTab::Session { id } => {
                    if self.external_sessions.iter().any(|s| s.id == id) {
                        self.right_tabs.push(RightTab::Session(id));
                    }
                }
            }
        }
        self.right_pane_visible = persisted.visible && !self.right_tabs.is_empty();
        if self.right_tabs.is_empty() {
            self.active_right_tab = 0;
            self.editor_open = false;
        } else {
            let active = persisted.active.min(self.right_tabs.len() - 1);
            self.set_active_right_tab(active, cx);
        }
    }

    /// Recreate a persisted browser tab's webview (app-restart path): build
    /// the view, register it in the host routing table, arm the title poll —
    /// tab-list placement is the caller's.
    fn restore_browser_tab(
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
        if let Some(host) = crate::browser_host::WorkspaceBrowserHost::concrete() {
            host.register_ui_tab(tab_id, self.thread.downgrade());
        }
        self.spawn_browser_title_ticker(cx);
        tab_id
    }

    /// Toggle the right pane's visibility from the TitleBar button. Hiding
    /// keeps every tab alive; showing with an empty tab list lands on a fresh
    /// Launcher tab so the button always opens something real.
    fn toggle_right_pane(&mut self, cx: &mut Context<Self>) {
        if self.right_pane_open() {
            self.right_pane_visible = false;
        } else {
            self.right_pane_visible = true;
            if self.right_tabs.is_empty() {
                self.right_tabs.push(RightTab::Launcher);
                self.active_right_tab = 0;
                self.editor_open = false;
            }
        }
        self.persist_right_pane(cx);
        cx.notify();
    }

    /// Open (or focus) an empty Launcher tab.
    fn open_launcher_tab(&mut self, cx: &mut Context<Self>) {
        if let Some(ix) = self
            .right_tabs
            .iter()
            .position(|t| matches!(t, RightTab::Launcher))
        {
            self.right_pane_visible = true;
            self.set_active_right_tab(ix, cx);
            return;
        }
        self.right_pane_visible = true;
        self.right_tabs.push(RightTab::Launcher);
        self.set_active_right_tab(self.right_tabs.len() - 1, cx);
    }

    /// The Launcher tab's body: five centered integration shortcuts, with the
    /// open provider→model cascade anchored under its CLI-agent row.
    fn render_launcher_content(
        &mut self,
        launcher_ix: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let weak = cx.weak_entity();
        let on_pick = move |pick: LauncherPick, window: &mut Window, cx: &mut App| {
            if let Some(ws) = weak.upgrade() {
                ws.update(cx, |this, cx| {
                    this.launcher_pick(pick, launcher_ix, window, cx)
                });
            }
        };
        let dropdown = self.launcher_menu_kind.zip(self.launcher_menu.clone());
        crate::views::launcher::render_launcher_tab(&theme, Rc::new(on_pick), dropdown)
    }

    /// Route a launcher shortcut to its view, opening it on the launcher's own
    /// tab. Terminal and the three CLI agents spawn at the active thread's
    /// cwd; CLI agents pick their provider/model through the cascade first.
    fn launcher_pick(
        &mut self,
        pick: LauncherPick,
        launcher_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match pick {
            LauncherPick::Browser => {
                if matches!(self.right_tabs.get(launcher_ix), Some(RightTab::Launcher)) {
                    self.right_tabs.remove(launcher_ix);
                    self.reseat_active_after_close(launcher_ix, cx);
                }
                let tab_id =
                    self.open_browser_tab(crate::views::browser_view::DEFAULT_URL, window, cx);
                // Keep the browser view on the launcher's own tab slot.
                if let Some(new_ix) = self
                    .right_tabs
                    .iter()
                    .position(|t| matches!(t, RightTab::Browser(id) if *id == tab_id))
                {
                    let insert_at = launcher_ix.min(self.right_tabs.len() - 1);
                    if new_ix != insert_at {
                        let tab = self.right_tabs.remove(new_ix);
                        self.right_tabs.insert(insert_at, tab);
                    }
                    self.set_active_right_tab(insert_at, cx);
                }
            }
            LauncherPick::Terminal => {
                let cwd = self.launcher_thread_cwd(cx);
                self.spawn_plain_session(
                    SessionKind::Terminal,
                    cwd,
                    SessionPlacement::RightPane {
                        replace_tab: launcher_ix,
                    },
                    window,
                    cx,
                );
            }
            LauncherPick::Agent(kind) => {
                self.open_launcher_cascade(kind, launcher_ix, window, cx);
            }
        }
    }

    /// The active thread's cwd for launcher spawns (`None` when unset — the
    /// spawn paths fall back to the workspace cwd, parity with the
    /// Conversations-header spawns).
    fn launcher_thread_cwd(&self, cx: &App) -> Option<PathBuf> {
        let cwd = self
            .store
            .as_ref()
            .map(|s| std::path::PathBuf::from(s.read(cx).store.cwd.clone()))
            .unwrap_or_else(|| self.thread.read(cx).cwd().to_path_buf());
        if cwd.as_os_str().is_empty() {
            None
        } else {
            Some(cwd)
        }
    }

    /// Open the provider→model cascade for a launcher CLI-agent row. Mirrors
    /// the model-selector popup pattern: entity + dismiss subscription,
    /// dropped on close.
    fn open_launcher_cascade(
        &mut self,
        kind: SessionKind,
        launcher_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.launcher_menu_kind == Some(kind) {
            self.close_launcher_menu();
            cx.notify();
            return;
        }
        self.close_launcher_menu();
        let cwd = self.launcher_thread_cwd(cx);
        let ws = cx.entity().downgrade();
        let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
            crate::views::model_cascade::build_model_cascade(
                menu,
                kind.agent_id(),
                window,
                cx,
                move |provider, model, wire, window, cx| {
                    if let Some(ws) = ws.upgrade() {
                        ws.update(cx, |this, cx| {
                            this.close_launcher_menu();
                            this.spawn_external_session(
                                ExternalSpawn {
                                    kind,
                                    provider_name: provider,
                                    model_id: model,
                                    wire_api: wire,
                                    project_cwd: cwd.clone(),
                                },
                                SessionPlacement::RightPane {
                                    replace_tab: launcher_ix,
                                },
                                window,
                                cx,
                            );
                            cx.notify();
                        });
                    }
                },
            )
        });
        let sub = cx.subscribe(&menu, |this, _menu, _: &DismissEvent, cx| {
            this.close_launcher_menu();
            cx.notify();
        });
        self.launcher_menu = Some(menu);
        self.launcher_menu_sub = Some(sub);
        self.launcher_menu_kind = Some(kind);
        cx.notify();
    }

    fn close_launcher_menu(&mut self) {
        self.launcher_menu = None;
        self.launcher_menu_sub = None;
        self.launcher_menu_kind = None;
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
        self.persist_right_pane(cx);
        cx.notify();
    }

    /// Keep `active_right_tab` pointing at the same tab after the tab at
    /// `removed_ix` was just removed from `right_tabs`. Tabs after
    /// `removed_ix` shift left by one, so a still-live active tab to the right
    /// must decrement; the active tab itself being removed (and being the
    /// last) falls back to the new last tab.
    fn reseat_active_after_close(&mut self, removed_ix: usize, cx: &mut Context<Self>) {
        if self.active_right_tab > removed_ix {
            self.active_right_tab -= 1;
        } else if self.active_right_tab >= self.right_tabs.len() {
            self.active_right_tab = self.right_tabs.len().saturating_sub(1);
        }
        // Tab indices shifted — the cached hover index would name the wrong
        // tab until the next mouse move recomputes it.
        self.hovered_right_tab = None;
        self.persist_right_pane(cx);
    }

    /// Open the markdown editor: hide the inline composer and transfer its draft
    /// into the editor so writing continues there. If an Editor tab is already
    /// present, just focus it. Submit from the editor with Cmd-Enter; close with
    /// Ctrl-G / Cmd-W to move the draft back.
    fn open_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Close any open inline menus so they don't linger behind the hidden footer.
        self.close_completion(cx);
        self.close_plus_menu();
        self.right_pane_visible = true;
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
        self.reseat_active_after_close(ix, cx);
        self.hide_right_pane_if_empty(cx);
        self.editor_open = self
            .right_tabs
            .get(self.active_right_tab)
            .is_some_and(|t| matches!(t, RightTab::Editor));
        cx.notify();
    }

    /// Close a right-pane tab by index. The Editor tab routes through
    /// `close_editor` (draft-transfer semantics); a Session tab routes
    /// through `close_external_session` (kill + teardown, whose removal path
    /// drops the tab); the rest remove in place.
    fn close_right_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        match self.right_tabs.get(ix).cloned() {
            Some(RightTab::Editor) => self.close_editor(window, cx),
            Some(RightTab::Browser(id)) => self.close_browser_tab(id, cx),
            Some(RightTab::Session(id)) => self.close_external_session(&id, cx),
            Some(RightTab::Subagent(id)) => {
                self.subagent_panels.remove(&id);
                self.right_tabs.remove(ix);
                self.reseat_active_after_close(ix, cx);
                self.hide_right_pane_if_empty(cx);
                cx.notify();
            }
            Some(RightTab::Launcher) | None => {
                if ix < self.right_tabs.len() {
                    self.close_launcher_menu();
                    self.right_tabs.remove(ix);
                    self.reseat_active_after_close(ix, cx);
                    self.hide_right_pane_if_empty(cx);
                    cx.notify();
                }
            }
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

    /// Open (or focus) a sub-agent observation panel in the right pane. The
    /// tab label is the subagent's address (`id`); the panel's banner shows
    /// `topic` — the shared `subagent_topic` derivation the rail and
    /// conversation rows use.
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
                .or_else(|| self.subagent_final_text.get(id).cloned())
        } else {
            None
        };
        let banner = if topic.is_empty() {
            id.to_string()
        } else {
            topic.to_string()
        };
        let role = if subagent_type.is_empty() {
            "sub-agent".to_string()
        } else {
            subagent_type.to_string()
        };
        let prompt = self.subagent_prompts.get(id).cloned();
        let panel = crate::views::subagent_panel::SubagentPanel::new(
            banner,
            role,
            status,
            &backfill,
            prompt,
            final_text,
            cx.weak_entity(),
            cx,
        );
        self.subagent_panels.insert(id.to_string(), panel);
        self.right_pane_visible = true;
        self.right_tabs.push(RightTab::Subagent(id.to_string()));
        self.set_active_right_tab(self.right_tabs.len() - 1, cx);
        cx.notify();
    }

    /// Final answer of a finished Agent call, for panels opened after a
    /// reload when no live transcript was accumulated.
    fn agent_final_text(&self, id: &str, cx: &App) -> Option<String> {
        use agent::language_model::MessageContent;
        self.store
            .as_ref()
            .map(|s| s.read(cx).store.messages.clone())
            .unwrap_or_else(|| self.thread.read(cx).messages().to_vec())
            .iter()
            .flat_map(|m| m.content.iter())
            .find_map(|c| match c {
                MessageContent::ToolResult(r) if r.tool_use_id == id => Some(r.content.clone()),
                _ => None,
            })
    }

    /// Drop per-thread sub-agent observation state and close its tabs.
    fn clear_subagent_observation(&mut self, cx: &mut Context<Self>) {
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
        self.hide_right_pane_if_empty(cx);
    }

    /// Rebuild the per-thread sub-agent observation state (rail rows + panel
    /// prompt/final-text) from the restored transcript. Live rows are fed by
    /// `SubagentProgress` events, which die with the process; this recovers
    /// the settled rows after a restart or a thread switch-back so the rail
    /// and panels are not left empty. Idempotent: rows upsert by address.
    fn rebuild_subagent_observation(
        &mut self,
        messages: &[agent::Message],
        cx: &mut Context<Self>,
    ) {
        for row in agent::subagent_restore::rebuild_from_messages(messages) {
            let first_line = agent::steer_bus::first_line(&row.prompt);
            self.subagent_prompts
                .insert(row.address.clone(), row.prompt.clone());
            if let Some(text) = &row.final_text {
                self.subagent_final_text
                    .insert(row.address.clone(), text.clone());
            }
            self.context_rail.update(cx, |r, cx| {
                r.apply_subagent_progress(
                    &row.address,
                    &row.subagent_type,
                    first_line.as_deref(),
                    row.status,
                    None,
                    cx,
                );
            });
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
            self.store
                .as_ref()
                .and_then(|s| {
                    serde_json::from_value::<agent::thread::HistoryPhase>(
                        serde_json::Value::String(s.read(cx).store.history_phase.clone()),
                    )
                    .ok()
                })
                .unwrap_or_else(|| self.thread.read(cx).history_phase())
                .is_loading(),
            self.store
                .as_ref()
                .map(|s| s.read(cx).store.running)
                .unwrap_or_else(|| self.thread.read(cx).is_running()),
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
        if !self.send_note(|sid| manox_protocol::ClientNote::Submit {
            session_id: sid.into(),
            text: text.clone(),
            images: Vec::new(),
            client_id: None,
        }) {
            self.thread.update(cx, |thread, _| {
                thread.insert_user_message_with_ui_metadata(text, Some(ui));
                thread.run_turn();
            });
        }
        refresh_thread_list();
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
        let permission_mode = self
            .store
            .as_ref()
            .and_then(|s| {
                serde_json::from_value::<agent::thread::PermissionMode>(serde_json::Value::String(
                    s.read(cx).store.permission_mode.clone(),
                ))
                .ok()
            })
            .unwrap_or_else(|| self.thread.read(cx).permission_mode());
        UserTurnMeta::new(
            chrono::Utc::now().timestamp(),
            self.model_label(cx),
            Some(permission_mode),
        )
    }

    fn message_ui_metadata(meta: &UserTurnMeta) -> agent::MessageUiMetadata {
        agent::MessageUiMetadata {
            model_id: (!meta.model_id.is_empty()).then(|| meta.model_id.clone()),
            approval_mode: meta.approval_mode.map(|mode| mode.as_i64()),
            steered: meta.steered.then_some(true),
            external_event: None,
            author: meta.author.clone(),
            peer: meta.peer,
            display_text: None,
        }
    }

    pub(crate) fn model_label(&self, cx: &mut Context<Self>) -> String {
        {
            self.thread
                .read(cx)
                .model()
                .map(|model| agent::pi_providers::display_name(&model))
                .unwrap_or_else(|| i18n::t("workspace-no-model").to_string())
        }
    }

    /// Push a system-styled notice into the conversation (no thread message,
    /// no model turn). Used by slash commands and mode toggles to report
    /// outcomes — e.g. the mode-change acknowledgement. Renders as a
    /// neutral-toned `ConvItem::Notice` card (distinct from the red
    /// `ConvItem::Error`).
    ///
    /// The notice is inserted at `anchor` — `TurnEnd` (the end of the current
    /// turn, i.e. the list tail when idle) or `After(ix)` for a tool-call-
    /// adjacent record. Also persists the notice as a session `custom` entry
    /// so a reloaded thread reproduces it at the same position (entries
    /// carrying `tool_call_id` are re-spliced right after their tool item by
    /// the rebuild).
    pub fn add_info_message(
        &mut self,
        text: String,
        anchor: NoticeAnchor,
        tool_call_id: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let weak = cx.weak_entity();
        let ix = self
            .conversation
            .update(cx, |c, cx| c.push_notice(text.clone(), anchor, weak, cx));
        self.apply_list_insert(ix);
        self.append_ui_note(agent::db::UiNoteKind::Notice, text, tool_call_id, cx);
        // Tail-follow keeps the viewport pinned to the live end, so a
        // `TurnEnd`-anchored notice is revealed by the follow; an `After`
        // anchored one sits above the viewport by design (a record near its
        // tool call, not an alert).
        cx.notify();
    }

    /// Persist a UI annotation (`Error` / `Notice` / `PlanReview`) as a
    /// `custom` entry in the session jsonl at the current leaf. The append
    /// order IS the reload order, so rebuilt conversations place the card
    /// where it appeared live; entries carrying `tool_call_id` are re-spliced
    /// next to their tool item by the rebuild instead. Fire-and-forget via
    /// the engine's command queue, which orders it against prompts.
    fn append_ui_note(
        &self,
        kind: agent::db::UiNoteKind,
        text: String,
        tool_call_id: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let mut data = serde_json::json!({ "text": text });
        // A tool-anchored notice carries the tool call id so the rebuild can
        // splice it next to the tool item, matching the live placement.
        // `data` is raw JSON — no schema change.
        if let Some(id) = tool_call_id {
            data["tool_call_id"] = serde_json::Value::String(id.to_owned());
        }
        let kind_str = match kind {
            agent::db::UiNoteKind::Error => "error",
            agent::db::UiNoteKind::Notice => "notice",
            agent::db::UiNoteKind::PlanReview => "plan_review",
        };
        if !self.send_note(|sid| manox_protocol::ClientNote::AppendUiNote {
            session_id: sid.into(),
            kind: kind_str.into(),
            data: data.clone(),
        }) {
            self.thread.update(cx, |t, _| {
                t.append_ui_note(agent::db::UiNoteRecord { kind, data })
            });
        }
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
        let allow = matches!(decision, PermissionDecision::AllowOnce);
        if let Some(msg_id) = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.pending_auth.get(&id).cloned())
            && let Some(conn) = &self.client_conn
        {
            conn.send_to_server(manox_protocol::FromClient::Reply {
                id: msg_id,
                outcome: Ok(serde_json::json!({ "allow": allow })),
            });
            return;
        }
        self.thread.update(cx, |thread, _| {
            thread.respond_authorization(&id, agent::ToolAuthorizationResponse::Decision(decision));
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
        if self
            .store
            .as_ref()
            .and_then(|s| {
                serde_json::from_value::<agent::thread::HistoryPhase>(serde_json::Value::String(
                    s.read(cx).store.history_phase.clone(),
                ))
                .ok()
            })
            .unwrap_or_else(|| self.thread.read(cx).history_phase())
            .is_loading()
        {
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
        if let Some(msg_id) = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.pending_auth.get(&id).cloned())
            && let Some(conn) = &self.client_conn
        {
            conn.send_to_server(manox_protocol::FromClient::Reply {
                id: msg_id,
                outcome: Ok(serde_json::json!({
                    "answers": answers,
                    "response": response,
                })),
            });
            return;
        }
        self.thread.update(cx, |thread, _| {
            thread.respond_authorization(
                &id,
                agent::ToolAuthorizationResponse::AskUserQuestion { answers, response },
            );
        });
        cx.notify();
    }

    /// Abort the current turn.
    pub(crate) fn cancel_turn(&mut self, cx: &mut Context<Self>) {
        if !self.send_note(|sid| manox_protocol::ClientNote::CancelTurn {
            session_id: sid.into(),
        }) {
            self.thread.update(cx, |thread, _| {
                thread.cancel();
            });
        }
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
        self.store
            .as_ref()
            .map(|s| s.read(cx).store.plan_mode)
            .unwrap_or_else(|| self.thread.read(cx).plan_mode())
    }

    /// Send a `ClientNote` to the AgentServer when the landing-thread
    /// connection is available (γ-3 mutation path). Returns `true` when the
    /// note was sent; the caller falls back to `self.thread.update` when `false`.
    fn send_note(&self, note_fn: impl FnOnce(&str) -> manox_protocol::ClientNote) -> bool {
        if let (Some(conn), Some(sid)) = (&self.client_conn, &self.session_id) {
            conn.send_to_server(manox_protocol::FromClient::Notification { note: note_fn(sid) });
            true
        } else {
            false
        }
    }

    /// Toggle plan mode on the current thread (persisted by the engine).
    pub(crate) fn set_thread_plan_mode(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if !self.send_note(|sid| manox_protocol::ClientNote::SetPlanMode {
            session_id: sid.into(),
            enabled,
        }) {
            self.thread.update(cx, |t, _| t.set_plan_mode(enabled));
        }
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
        let thread_id = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.id.0.clone())
            .unwrap_or_else(|| self.thread.read(cx).id.0.clone());
        self.thread
            .update(cx, |t, _| t.set_plan_review_pending(false));
        agent::thread_store_global().with_mut(|s| s.mark_pending_plan(&thread_id, false));
        if matches!(choice, PlanReviewChoice::Refine) {
            // Keep plan mode ON: demote the card and prompt for feedback.
            // The feedback turn runs under the plan-mode instructions; the
            // model updates the plan file and proposes again (fresh card).
            self.conversation
                .update(cx, |c, cx| c.consume_plan_review(cx));
            self.list_state.remeasure();
            self.add_info_message(
                i18n::t("plan-refine-notice").to_string(),
                NoticeAnchor::TurnEnd,
                None,
                cx,
            );
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
        let mut meta = self.user_turn_meta(cx);
        // The seed is harness-authored: attribute it to the session's own
        // agent so the bubble header names the agent, not the human.
        let author = self
            .store
            .as_ref()
            .and_then(|s| {
                serde_json::from_value::<agent::message::MessageAuthor>(serde_json::Value::String(
                    s.read(cx).store.self_author.clone(),
                ))
                .ok()
            })
            .unwrap_or_else(|| self.thread.read(cx).self_author());
        meta.author = Some(author);
        let ui = Self::message_ui_metadata(&meta);
        if matches!(choice, PlanReviewChoice::ExecuteFresh) {
            // Fresh context: archive this thread and continue on a new one
            // seeded with the execution directive. The plan file persists on
            // disk, so the new thread reads it from the path — no inline
            // plan-text copy in the transcript.
            let old_id = self
                .store
                .as_ref()
                .map(|s| s.read(cx).store.id.0.clone())
                .unwrap_or_else(|| self.thread.read(cx).id.0.clone());
            let cwd = self
                .store
                .as_ref()
                .map(|s| std::path::PathBuf::from(s.read(cx).store.cwd.clone()))
                .unwrap_or_else(|| self.thread.read(cx).cwd().to_path_buf());
            let project = self
                .store
                .as_ref()
                .and_then(|s| {
                    s.read(cx)
                        .store
                        .project
                        .clone()
                        .map(std::path::PathBuf::from)
                })
                .or_else(|| self.thread.read(cx).project());
            let model = self
                .store
                .as_ref()
                .and_then(|s| {
                    s.read(cx)
                        .store
                        .model
                        .clone()
                        .and_then(|v| serde_json::from_value::<pi::types::Model>(v).ok())
                })
                .or_else(|| self.thread.read(cx).model());
            let effort = self
                .store
                .as_ref()
                .and_then(|s| {
                    serde_json::from_value::<agent::language_model::ReasoningEffort>(
                        serde_json::Value::String(s.read(cx).store.reasoning_effort.clone()),
                    )
                    .ok()
                })
                .unwrap_or_else(|| self.thread.read(cx).reasoning_effort());
            let permission = self
                .store
                .as_ref()
                .and_then(|s| {
                    serde_json::from_value::<agent::thread::PermissionMode>(
                        serde_json::Value::String(s.read(cx).store.permission_mode.clone()),
                    )
                    .ok()
                })
                .unwrap_or_else(|| self.thread.read(cx).permission_mode());
            let new = match &project {
                Some(dir) => cx.new(|cx| {
                    ThreadProxy::new(
                        Thread::new_in_project(
                            ThreadId(uuid::Uuid::new_v4().to_string()),
                            dir.clone(),
                        ),
                        cx,
                    )
                }),
                None => cx.new(|cx| {
                    ThreadProxy::new(
                        Thread::new_fresh(ThreadId(uuid::Uuid::new_v4().to_string()), cwd),
                        cx,
                    )
                }),
            };
            new.update(cx, |t, _| {
                if let Some(model) = model {
                    t.set_model(model);
                }
                t.set_reasoning_effort(effort);
                t.set_permission_mode(permission);
                t.seed_plan_execution(review.plan_file.clone(), seed_text, Some(ui));
            });
            self.attach_thread(new, window, cx);
            refresh_thread_list();
            agent::thread_store_global().with_mut(|s| s.archive_thread(&old_id, true));
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
            let choice_str = match choice {
                PlanReviewChoice::ExecuteCompact => "execute_compact",
                PlanReviewChoice::ExecuteKeep => "execute_keep",
                _ => "execute_keep",
            };
            if let Some(msg_id) = self.store.as_ref().and_then(|s| {
                s.read(cx)
                    .store
                    .pending_plan_verdict
                    .get(&review.plan_file)
                    .cloned()
            }) && let Some(conn) = &self.client_conn
            {
                conn.send_to_server(manox_protocol::FromClient::Reply {
                    id: msg_id,
                    outcome: Ok(serde_json::json!({ "choice": choice_str })),
                });
            } else {
                self.thread.update(cx, |thread, _| {
                    thread.approve_plan(compact, compact_instructions, seed_text, Some(ui));
                });
            }
        }
        cx.notify();
    }

    /// Wire api string → Tag variant + label for the pi model menu.
    pub(crate) fn pi_wire_tag_variant(api: &str) -> (TagVariant, &'static str) {
        match api {
            "anthropic" => (TagVariant::Color(ColorName::Blue), "Anthropic"),
            "openai_responses" => (TagVariant::Color(ColorName::Cyan), "Responses"),
            "openai_completions" => (TagVariant::Color(ColorName::Amber), "Completions"),
            _ => (TagVariant::Secondary, "N/A"),
        }
    }

    /// Wire api string → text color for the pi composer model label and the
    /// context rail's per-model usage rows. Blue/cyan/amber at 600 in light,
    /// 300 in dark, mirroring the VS Code webview's `--wire-*` tokens so both
    /// sides render identical hues.
    pub(crate) fn pi_wire_text_color(api: &str, theme: &Theme) -> gpui::Hsla {
        let step = if theme.is_dark() { 300 } else { 600 };
        match api {
            "anthropic" => ColorName::Blue.scale(step),
            "openai_responses" => ColorName::Cyan.scale(step),
            "openai_completions" => ColorName::Amber.scale(step),
            _ => theme.muted_foreground,
        }
    }

    /// The pi-harness model selector. Reads the shared pi provider registry
    /// (the streaming source of truth): closed, a ghost button showing
    /// `provider · model · effort` with the model name tinted by wire api;
    /// open, a PopupMenu of provider submenus.
    fn render_model_selector_pi(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let open = self.model_open;
        let model = self
            .store
            .as_ref()
            .and_then(|s| {
                s.read(cx)
                    .store
                    .model
                    .clone()
                    .and_then(|v| serde_json::from_value::<pi::types::Model>(v).ok())
            })
            .or_else(|| self.thread.read(cx).model());
        let effort = self
            .store
            .as_ref()
            .and_then(|s| {
                serde_json::from_value::<agent::language_model::ReasoningEffort>(
                    serde_json::Value::String(s.read(cx).store.reasoning_effort.clone()),
                )
                .ok()
            })
            .unwrap_or_else(|| self.thread.read(cx).reasoning_effort());

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
                    dot().into_any_element(),
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(effort.wire_value().to_string())
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
                    let current_effort = this
                        .store
                        .as_ref()
                        .and_then(|s| {
                            serde_json::from_value::<agent::language_model::ReasoningEffort>(
                                serde_json::Value::String(
                                    s.read(cx).store.reasoning_effort.clone(),
                                ),
                            )
                            .ok()
                        })
                        .unwrap_or_else(|| this.thread.read(cx).reasoning_effort());
                    let workspace = cx.entity().downgrade();
                    let menu = PopupMenu::build(window, cx, |menu, window, cx| {
                        Self::build_model_popup_menu_pi(menu, workspace, current_effort, window, cx)
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
    /// A config model registered through several wire apis appears once per
    /// wire endpoint (exact duplicates collapse), so the responses and
    /// completions variants stay selectable alongside the anthropic one.
    fn build_model_popup_menu_pi(
        menu: PopupMenu,
        workspace: WeakEntity<Workspace>,
        current_effort: agent::language_model::ReasoningEffort,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
    ) -> PopupMenu {
        // Group by DISPLAY name via lookup (not adjacency): models() is
        // sorted by registration name, so same-display-name providers with
        // different registrations must still merge into one submenu.
        let mut providers: Vec<(String, Vec<pi::types::Model>)> = Vec::new();
        let mut seen: HashSet<(String, String)> = HashSet::new();
        for m in agent::pi_providers::global().models() {
            let prov = agent::pi_providers::display_provider_name(&m);
            // Identity is the registration name (unique per wire endpoint), so
            // wire variants of one provider stay separate; only exact
            // duplicates collapse.
            if !seen.insert((m.provider.clone(), agent::pi_providers::config_id(&m))) {
                continue;
            }
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
                                if !this.send_note(|sid| manox_protocol::ClientNote::SetModel {
                                    session_id: sid.into(),
                                    id: model.id.clone(),
                                }) {
                                    this.thread.update(cx, |t, _| t.set_model(model));
                                }
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
                        let effort_str = match effort {
                            agent::language_model::ReasoningEffort::High => "high",
                            agent::language_model::ReasoningEffort::Max => "max",
                        };
                        if !this.send_note(|sid| manox_protocol::ClientNote::SetReasoningEffort {
                            session_id: sid.into(),
                            effort: effort_str.into(),
                        }) {
                            this.thread.update(cx, |t, _| {
                                t.set_reasoning_effort(effort);
                            });
                        }
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

    /// Leave recall mode as soon as the input value diverges from the
    /// recalled turn. Recall is derived state keyed on that match, and the
    /// wrapper's `composer = recall` context must drop back to plain caret
    /// movement the moment the walk no longer applies — otherwise the next
    /// arrow key is consumed by a recall that can't act. `set_value` emits
    /// no Change event, so applying a recall step never cancels itself;
    /// user edits and completion inserts do fire Change and end the walk.
    fn exit_diverged_recall(&mut self, cx: &mut Context<Self>) {
        if self.recall_index < 0 {
            return;
        }
        let turns = self.recall_turns(cx);
        let value = self.input_state.read(cx).value().to_string();
        let in_recall = usize::try_from(self.recall_index)
            .ok()
            .and_then(|ix| turns.get(ix))
            .is_some_and(|text| value == *text);
        if !in_recall {
            self.recall_index = -1;
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
                    // otherwise `composer = recall` exactly while a history
                    // recall can apply, so the recall bindings shadow the
                    // Input's up/down only then. A matched action listener
                    // consumes the keystroke even when no recall results, so
                    // outside recall the context stays plain `composer` and
                    // the Input's native MoveUp/MoveDown move the caret. The
                    // completion and recall contexts are mutually exclusive,
                    // so the two same-depth binding sets never both match.
                    wrap = wrap.key_context(composer_key_context(
                        self.completion.is_some(),
                        self.recall_index >= 0,
                        self.input_state.read(cx).value().is_empty(),
                    ));
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
                        // the chips' popovers (project picker, permission menu, `+`
                        // menu) can overflow upward. `MIN_WINDOW_W` keeps the row
                        // wide enough that the chips themselves never overflow.
                        h_flex()
                            .items_center()
                            .gap_1()
                            .min_w_0()
                            .child(plus)
                            .child(project_chip)
                            .when_some(goal_chip, |el, chip| el.child(chip))
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
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.goal.clone())
            .and_then(|v| serde_json::from_value::<agent::goal::ThreadGoal>(v).ok())
            .or_else(|| self.thread.read(cx).goal())
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
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.goal.clone())
            .and_then(|v| serde_json::from_value::<agent::goal::ThreadGoal>(v).ok())
            .or_else(|| self.thread.read(cx).goal())
            .and_then(|goal| goal.token_budget)
            .map(|budget| budget.to_string())
            .unwrap_or_else(|| "none".into());
        self.input_state.update(cx, |state, cx| {
            state.set_value(format!("/goal budget {value}"), window, cx);
        });
        cx.notify();
    }

    pub fn begin_goal_rounds_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.goal.clone())
            .and_then(|v| serde_json::from_value::<agent::goal::ThreadGoal>(v).ok())
            .or_else(|| self.thread.read(cx).goal())
            .and_then(|goal| goal.max_rounds)
            .map(|max| max.to_string())
            .unwrap_or_else(|| "none".into());
        self.input_state.update(cx, |state, cx| {
            state.set_value(format!("/goal rounds {value}"), window, cx);
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
        let g = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.goal.clone())
            .and_then(|v| serde_json::from_value::<agent::goal::ThreadGoal>(v).ok())
            .or_else(|| self.thread.read(cx).goal())?;
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
        let reason = g
            .blocked_reason
            .as_ref()
            .map(|reason| reason.message.clone())
            .unwrap_or_else(|| "—".into());
        let tokens = g.tokens_used.to_string();
        let budget = g
            .token_budget
            .map(|value| value.to_string())
            .unwrap_or_else(|| "∞".into());
        let remaining = g
            .remaining_tokens()
            .map(|value| value.to_string())
            .unwrap_or_else(|| "∞".into());
        let rounds = format!(
            "{}/{}",
            g.rounds_started,
            g.max_rounds
                .map(|max| max.to_string())
                .unwrap_or_else(|| "∞".into())
        );
        let goal_status = g.status;
        let objective_label = i18n::t("goal-popover-objective");
        let status_label = i18n::t("goal-popover-status");
        let elapsed_label = i18n::t("goal-popover-elapsed");
        let reason_label = i18n::t("goal-popover-reason");
        let tokens_label = i18n::t("goal-popover-tokens");
        let budget_label = i18n::t("goal-popover-budget");
        let remaining_label = i18n::t("goal-popover-remaining");
        let rounds_label = i18n::t("goal-popover-rounds");
        let clear_label = i18n::t("goal-popover-clear");
        let pause_label = i18n::t("goal-popover-pause");
        let resume_label = i18n::t("goal-popover-resume");
        let edit_label = i18n::t("goal-popover-edit");
        let edit_budget_label = i18n::t("goal-popover-edit-budget");
        let edit_rounds_label = i18n::t("goal-popover-edit-rounds");
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
            .child(goal_popover_row(&rounds_label, &rounds, fg, muted))
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
                                    if !this.send_note(|sid| manox_protocol::ClientNote::Goal {
                                        session_id: sid.into(),
                                        action: "pause".into(),
                                        objective: None,
                                        budget: None,
                                        max_rounds: None,
                                    }) {
                                        this.thread.update(cx, |t, cx| {
                                            if let Err(error) = t.set_goal_status(
                                                agent::goal::GoalStatus::Paused,
                                                Some(agent::goal::GoalBlockReason {
                                                    code: "user-paused".into(),
                                                    message: "paused by user".into(),
                                                }),
                                                agent::db::GoalActor::User,
                                            ) {
                                                cx.emit(ThreadEvent::Error(error));
                                            }
                                        });
                                    }
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
                                        if !this.send_note(|sid| manox_protocol::ClientNote::Goal {
                                            session_id: sid.into(),
                                            action: "resume".into(),
                                            objective: None,
                                            budget: None,
                                            max_rounds: None,
                                        }) {
                                            this.thread.update(cx, |t, cx| {
                                                if let Err(error) = t.set_goal_status(
                                                    agent::goal::GoalStatus::Active,
                                                    None,
                                                    agent::db::GoalActor::User,
                                                ) {
                                                    cx.emit(ThreadEvent::Error(error));
                                                }
                                            });
                                        }
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
                        matches!(
                            goal_status,
                            agent::goal::GoalStatus::Active
                                | agent::goal::GoalStatus::Paused
                                | agent::goal::GoalStatus::Blocked
                        ),
                        |row| {
                            row.child(
                                Button::new("goal-edit-rounds")
                                    .small()
                                    .label(edit_rounds_label)
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            this.goal_popover_open = false;
                                            this.begin_goal_rounds_edit(window, cx);
                                        },
                                    )),
                            )
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
                                if !this.send_note(|sid| manox_protocol::ClientNote::Goal {
                                    session_id: sid.into(),
                                    action: "clear".into(),
                                    objective: None,
                                    budget: None,
                                    max_rounds: None,
                                }) {
                                    this.thread.update(cx, |t, cx| {
                                        if let Err(error) = t.clear_goal(agent::db::GoalActor::User)
                                        {
                                            cx.emit(ThreadEvent::Error(error));
                                        }
                                    });
                                }
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

    /// Plan-mode indicator chip: visible while the session plans (read-only
    /// research + plan-file writes), so the state is never silent. Clicking
    /// it leaves plan mode — the escape hatch when a review card is missed
    /// or the model stalls in research.
    fn render_plan_chip(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.plan_mode)
            .unwrap_or_else(|| self.thread.read(cx).plan_mode())
        {
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
                    this.add_info_message(
                        i18n::t("plan-mode-off-notice").to_string(),
                        NoticeAnchor::TurnEnd,
                        None,
                        cx,
                    );
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

    /// Access chip + permission-mode popover.
    ///
    /// The chip is a mode-aware pill rendered next to the composer send button.
    /// Each `PermissionMode` gets its own icon + accent color (amber eye for
    /// Read Only, green folder for Workspace Write, red triangle for Full
    /// Access) so the current permission posture is legible at a glance — a
    /// 1-line summary of what the model is allowed to do.
    ///
    /// Clicking the chip opens the popover: a question row with a "Learn
    /// more" link and three selectable rows (icon + title + subtitle, check
    /// on the right). The popover is `w(360)` to fit the longest bilingual
    /// subtitle without wrapping.
    fn render_access_placeholder(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let mode = self
            .store
            .as_ref()
            .and_then(|s| {
                serde_json::from_value::<agent::thread::PermissionMode>(serde_json::Value::String(
                    s.read(cx).store.permission_mode.clone(),
                ))
                .ok()
            })
            .unwrap_or_else(|| self.thread.read(cx).permission_mode());
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
        let content = build_permission_content(workspace.clone(), mode, cx);
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
            let ws_chrome = ws.clone();
            let ws_internal = ws.clone();
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
                move |_window, cx| {
                    let _ = ws_chrome.update(cx, |this, cx| {
                        this.close_plus_menu();
                        this.activate_browser_tool_suite(
                            agent::pi_engine::BrowserSuite::ChromeUse,
                            cx,
                        );
                    });
                },
                move |_window, cx| {
                    let _ = ws_internal.update(cx, |this, cx| {
                        this.close_plus_menu();
                        this.activate_browser_tool_suite(
                            agent::pi_engine::BrowserSuite::WebExplore,
                            cx,
                        );
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

    /// Activate a browser tool suite on the bound thread. The chip is
    /// derived state: it rides the `BrowserSuitesChanged` echo of the
    /// facade mirror, so a landing thread's pre-engine toggle survives until
    /// the engine materializes. The engine merges the suite names atomically
    /// against the session's authoritative active-tool set.
    fn activate_browser_tool_suite(
        &mut self,
        suite: agent::pi_engine::BrowserSuite,
        cx: &mut Context<Self>,
    ) {
        self.thread
            .update(cx, |t, _| t.set_browser_suite(suite, true));
    }

    /// Deactivate a browser tool suite on the bound thread; the chip follows
    /// the mirror echo (see `activate_browser_tool_suite`).
    fn deactivate_browser_tool_suite(
        &mut self,
        suite: agent::pi_engine::BrowserSuite,
        cx: &mut Context<Self>,
    ) {
        self.thread
            .update(cx, |t, _| t.set_browser_suite(suite, false));
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
                if this
                    .store
                    .as_ref()
                    .map(|s| s.read(cx).store.running)
                    .unwrap_or_else(|| this.thread.read(cx).is_running())
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

    /// Attachment + browser-suite chips shown above the composer, each
    /// removable. File attachments clear on submit; browser suites persist.
    fn render_attachments(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.pending_attachments.is_empty() && self.active_browser_suites.is_empty() {
            return None;
        }
        let mut col = v_flex().w_full().gap_1();
        if !self.pending_attachments.is_empty() {
            let on_remove = cx.listener(|this, ix: &usize, _window, cx| {
                if *ix < this.pending_attachments.len() {
                    this.pending_attachments.remove(*ix);
                    cx.notify();
                }
            });
            col = col.child(centered(render_attachment_chips(
                &self.pending_attachments,
                theme,
                move |ix, window, cx| on_remove(&ix, window, cx),
            )));
        }
        if !self.active_browser_suites.is_empty() {
            let on_remove_suite = cx.listener(|this, ix: &usize, _window, cx| {
                if let Some(suite) = this.active_browser_suites.get(*ix).copied() {
                    this.deactivate_browser_tool_suite(suite, cx);
                }
            });
            col = col.child(centered(render_browser_chips(
                &self.active_browser_suites,
                theme,
                move |ix, window, cx| on_remove_suite(&ix, window, cx),
            )));
        }
        Some(col.into_any_element())
    }

    /// The pi-harness project chip: bound-project indicator + dropdown with
    /// recent projects (store `known_projects` plus session-cwd backfill),
    /// blank-project creation and folder selection. Selection is only
    /// allowed on empty threads (same guard as the manox chip). Data source
    /// is the pi thread store only.
    fn render_project_chip_pi(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let project = self
            .store
            .as_ref()
            .and_then(|s| {
                s.read(cx)
                    .store
                    .project
                    .clone()
                    .map(std::path::PathBuf::from)
            })
            .or_else(|| self.thread.read(cx).project());
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
                let can_set = this
                    .store
                    .as_ref()
                    .map(|s| s.read(cx).store.messages.is_empty())
                    .unwrap_or_else(|| this.thread.read(cx).messages().is_empty());
                if !can_set {
                    return;
                }
                this.project_chip_open = true;

                let ws = workspace.clone();
                let theme = cx.theme().clone();
                let ws_blank = ws.clone();
                let ws_folder = ws.clone();

                let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
                    let mut menu = menu.max_w(gpui::px(320.)).scrollable(true);
                    menu = menu.label(i18n::t("sidebar-section-projects"));

                    // Recent projects: registered folders first (newest
                    // first), then session cwds not yet registered.
                    let store = agent::thread_store_global();
                    let mut recent_projects: Vec<String> = Vec::new();
                    let mut seen = std::collections::HashSet::new();
                    store.read(|store| {
                        for path in store.known_projects().iter().rev() {
                            if seen.insert(path.clone()) {
                                recent_projects.push(path.clone());
                            }
                            if recent_projects.len() >= 20 {
                                break;
                            }
                        }
                        if recent_projects.len() < 20 {
                            for sum in store.summaries() {
                                if sum.project.is_empty() || !seen.insert(sum.project.clone()) {
                                    continue;
                                }
                                recent_projects.push(sum.project.clone());
                                if recent_projects.len() >= 20 {
                                    break;
                                }
                            }
                        }
                    });

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
                                        if !this.send_note(|sid| {
                                            manox_protocol::ClientNote::SetCwd {
                                                session_id: sid.into(),
                                                cwd: p.to_str().unwrap_or_default().into(),
                                            }
                                        }) {
                                            this.thread.update(cx, |t, _| t.set_project(p.clone()));
                                        }
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
    fn register_project_in_store(path: &std::path::Path, _cx: &mut Context<Self>) {
        let path = path.to_string_lossy().to_string();
        agent::thread_store_global().with_mut(|s| s.register_project(path));
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
        if !self.send_note(|sid| manox_protocol::ClientNote::SetCwd {
            session_id: sid.into(),
            cwd: new_path.to_str().unwrap_or_default().into(),
        }) {
            self.thread
                .update(cx, |t, _| t.set_project(new_path.clone()));
        }
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
                    if !this.send_note(|sid| manox_protocol::ClientNote::SetCwd {
                        session_id: sid.into(),
                        cwd: path.to_str().unwrap_or_default().into(),
                    }) {
                        this.thread.update(cx, |t, _| t.set_project(path.clone()));
                    }
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
    /// right pane, question-card overlays. Shared by both harness builds.
    fn render_manox(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !matches!(self.view_mode, ViewMode::Workspace) {
            self.drop_turn_navigator(cx);
        }
        // The native webview keeps its last bounds until explicitly hidden,
        // so every frame must pin which one may draw (the active browser tab
        // of a visible right pane) — an inactive tab would otherwise paint
        // over the pane's content.
        self.sync_browser_visibility(cx);
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
                .store
                .as_ref()
                .and_then(|s| {
                    s.read(cx)
                        .store
                        .project
                        .clone()
                        .map(std::path::PathBuf::from)
                })
                .or_else(|| self.thread.read(cx).project())
                .as_ref()
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
        let running = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .unwrap_or_else(|| self.thread.read(cx).is_running());

        self.ensure_blank_project_input(window, cx);

        if self.blocking_overlay_active() && self.turn_navigator.is_some() {
            self.close_turn_navigator(window, cx);
        }

        let editor_open = self.editor_open;
        let right_pane_open = self.right_pane_open();
        let editor_preview = self.editor_preview;
        let editor_width = self.editor_width;
        // Title text is the active thread's display title (user rename > LLM
        // title > mechanical summary). Falls back to "manox" so an unselected
        // first screen stays branded before any title is generated.
        let title_text: SharedString = {
            let s = self
                .store
                .as_ref()
                .map(|s| s.read(cx).store.display_title.clone())
                .unwrap_or_else(|| self.thread.read(cx).display_title());
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
        let loading = self
            .store
            .as_ref()
            .and_then(|s| {
                serde_json::from_value::<agent::thread::HistoryPhase>(serde_json::Value::String(
                    s.read(cx).store.history_phase.clone(),
                ))
                .ok()
            })
            .unwrap_or_else(|| self.thread.read(cx).history_phase())
            .is_loading();
        let composer_placement = composer_placement(editor_open && right_pane_open, first_screen);
        let main_body_w = window.bounds().size.width
            - self.sidebar_width
            - px(SIDEBAR_DIVIDER_WIDTH)
            - if right_pane_open {
                editor_width + px(EDITOR_DIVIDER_WIDTH)
            } else {
                px(0.)
            };
        let show_rail = !first_screen
            && (!editor_open || !right_pane_open)
            && self
                .store
                .as_ref()
                .map(|s| s.read(cx).store.has_interacted)
                .unwrap_or_else(|| self.thread.read(cx).has_interacted())
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
        // Notice items on the first screen (e.g. mode-switch acknowledgement).
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
        // Right pane is a peer tab container for the editor, launcher,
        // browser, sub-agent observers, and embedded sessions. The
        // top-level TabBar is built from `right_tabs`; the content below
        // dispatches on the active tab.
        let active_tab = self.right_tabs.get(self.active_right_tab).cloned();
        let hovered_tab = self.hovered_right_tab;
        let right_tab_children: Vec<Tab> = self
            .right_tabs
            .iter()
            .enumerate()
            .map(|(ix, tab)| {
                // Full label rides the tooltip; the fixed-width tab shows the
                // capped form. Session tabs additionally carry their kind's
                // brand glyph as a prefix.
                let (full, icon_path): (SharedString, Option<&'static str>) = match tab {
                    RightTab::Editor => (i18n::t("member-editor-tab"), None),
                    RightTab::Launcher => (i18n::t("right-tab-launcher"), None),
                    RightTab::Browser(id) => {
                        // The page's <title>, polled by the host; the URL is
                        // the fallback before the first title lands.
                        let label = self
                            .browser_views
                            .get(id)
                            .map(|v| {
                                let view = v.read(cx);
                                let title = view.title();
                                if title.is_empty() {
                                    view.url().to_string()
                                } else {
                                    title.to_string()
                                }
                            })
                            .unwrap_or_default();
                        (i18n::t_str("browser-tab", &[("title", &label)]), None)
                    }
                    // The subagent's address (e.g. `Sailor_0`); the panel's
                    // banner carries the topic.
                    RightTab::Subagent(id) => (id.as_str().into(), None),
                    RightTab::Session(id) => {
                        let session = self.external_sessions.iter().find(|s| s.id == *id);
                        let label = session.map(|s| s.display_title()).unwrap_or_default();
                        (label, session.map(|s| s.kind.icon_asset()))
                    }
                };
                let mut base = Tab::new()
                    .label(cap_tab_label(&full))
                    .w(px(RIGHT_TAB_WIDTH))
                    .tooltip({
                        let full = full.clone();
                        move |window, cx| Tooltip::new(full.clone()).build(window, cx)
                    })
                    .on_hover(cx.listener(move |this, hovering: &bool, _window, cx| {
                        if *hovering {
                            this.hovered_right_tab = Some(ix);
                        } else if this.hovered_right_tab == Some(ix) {
                            this.hovered_right_tab = None;
                        }
                        cx.notify();
                    }));
                if let Some(path) = icon_path {
                    base = base.prefix(
                        Icon::default()
                            .path(path)
                            .xsmall()
                            .text_color(theme.muted_foreground),
                    );
                }
                // The close × reveals on hover for every tab kind. Routing
                // stays in `close_right_tab`: Editor keeps its draft-transfer
                // semantics, Session kills + tears the session down.
                if hovered_tab == Some(ix) {
                    base = base.suffix(
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
                    );
                }
                base
            })
            .collect();
        let editor_pane = v_flex()
            .w(editor_width)
            .h_full()
            .flex_shrink_0()
            .bg(theme.background)
            .child(
                h_flex().w_full().px_2().pt_1().items_center().child(
                    TabBar::new("right-tabs")
                        .underline()
                        .small()
                        .selected_index(self.active_right_tab)
                        .on_click(cx.listener(|this, ix: &usize, _window, cx| {
                            this.set_active_right_tab(*ix, cx);
                        }))
                        .children(right_tab_children)
                        .suffix(
                            Button::new("right-tab-new")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Plus)
                                .tooltip(i18n::t("right-tab-new"))
                                .on_click(cx.listener(|this, _, _window, cx| {
                                    this.open_launcher_tab(cx);
                                })),
                        ),
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
                        Some(RightTab::Subagent(id)) => self
                            .subagent_panels
                            .get(&id)
                            .map(|p| p.clone().into_any_element())
                            .unwrap_or_else(|| gpui::div().into_any_element()),
                        Some(RightTab::Launcher) => {
                            self.render_launcher_content(self.active_right_tab, cx)
                        }
                        Some(RightTab::Session(id)) => self
                            .external_sessions
                            .iter()
                            .find(|s| s.id == id)
                            .map(|s| s.terminal_view.clone().into_any_element())
                            .unwrap_or_else(|| gpui::div().into_any_element()),
                        None => gpui::div().into_any_element(),
                    }),
            );

        // The shared shell provides the sidebar + draggable divider and the
        // mode-switching actions; this mode chains the conversation-only
        // actions and the turn-navigator overlay onto it. The shell's main
        // slot is the main view: a two-column container holding the message
        // column (conversation + rail) and, when any right-pane tab is open,
        // the right side view (editor / launcher / browser / session tabs).
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
                            let diag_enabled = crate::overlap_diag::enabled();
                            let processor = move |ix: usize, _window: &mut Window, cx: &mut App| {
                                let item = conversation.read(cx).items().get(ix).cloned();
                                match item {
                                    // `flex_shrink_0` guards against any
                                    // available height leaking down the
                                    // flex chain and compressing a row.
                                    Some(item) => {
                                        if diag_enabled {
                                            crate::overlap_diag::record_mapping(
                                                ix,
                                                item.read(cx).diagnostic_id(),
                                            );
                                        }
                                        v_flex()
                                            .w_full()
                                            .pt_1()
                                            .pb_4()
                                            .flex_shrink_0()
                                            .min_w_0()
                                            .debug_selector(move || {
                                                format!("workspace-message-row-{ix}")
                                            })
                                            .when(diag_enabled, |this| {
                                                this.on_prepaint(move |bounds, _window, _cx| {
                                                    crate::overlap_diag::record_row(ix, bounds);
                                                })
                                            })
                                            .child(item)
                                            .into_any_element()
                                    }
                                    // Index out of range mid-splice (count
                                    // changed between a layout pass and the
                                    // render closure): render an empty row.
                                    None => gpui::div().into_any_element(),
                                }
                            };
                            let list_state = self.list_state.clone();
                            let width_state = self.list_state.clone();
                            let message_list_width = self.message_list_width.clone();
                            let diag_state = self.list_state.clone();
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
                                        if crate::overlap_diag::enabled() {
                                            crate::overlap_diag::check_completed_frame(
                                                bounds,
                                                diag_state.item_count(),
                                            );
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
                        // Question card overlay (if any)
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
                                .child(
                                    h_flex().items_center().pr_2().child(
                                        Button::new("right-pane-toggle")
                                            .ghost()
                                            .xsmall()
                                            .icon(if right_pane_open {
                                                Icon::new(IconName::PanelRight)
                                            } else {
                                                Icon::default().path("icons/panel-right-dashed.svg")
                                            })
                                            .tooltip(i18n::t("right-pane-toggle"))
                                            .on_click(cx.listener(|this, _, _window, cx| {
                                                this.toggle_right_pane(cx);
                                            })),
                                    ),
                                ),
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
        // right side view (editor / launcher / browser / session tabs) as its
        // sub-columns. Nesting the right pane inside the main view keeps the
        // shell uniformly
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
            // Composer history recall: fires only via the `composer == recall
            // > Input` bindings. The wrapper carries that context exactly
            // while a recall can apply (see `composer_key_context`), so
            // these listeners never see plain caret movement — a listener
            // that fires consumes the keystroke whether or not a recall
            // step results.
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
/// malformed (the generic question overlay then takes over as a fallback).
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

fn thread_cwd(
    thread: &Entity<ThreadEntity>,
    store: &Option<gpui::Entity<ClientStoreHandle>>,
    cx: &App,
) -> Option<SharedString> {
    let cwd = store
        .as_ref()
        .map(|s| std::path::PathBuf::from(s.read(cx).store.cwd.clone()))
        .unwrap_or_else(|| thread.read(cx).cwd().to_path_buf());
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

/// Map a `PermissionMode` to the chip's (label, accent color, icon) triple.
///
/// Colors are theme tokens, not raw hsla values, so the chip follows the
/// active theme (light/dark) without bespoke palettes per mode. The
/// WorkspaceWrite accent uses `info` (green) as a "this is the safe
/// default" signal — staying gray would be visually identical to a disabled
/// state.
fn mode_chip_visual(mode: PermissionMode, theme: &Theme) -> (SharedString, gpui::Hsla, IconName) {
    match mode {
        PermissionMode::ReadOnly => (
            i18n::t("workspace-chip-mode-readonly"),
            theme.warning,
            IconName::Eye,
        ),
        PermissionMode::WorkspaceWrite => (
            i18n::t("workspace-chip-mode-workspacewrite"),
            theme.info,
            IconName::FolderOpen,
        ),
        PermissionMode::DangerFullAccess => (
            i18n::t("workspace-chip-mode-dangerfullaccess"),
            theme.danger,
            IconName::TriangleAlert,
        ),
    }
}

/// Build the popover content for the access chip: a header row (question +
/// "Learn more" link) and three selectable mode rows (icon + title +
/// subtitle, check on the right for the active one). The whole thing is a
/// plain `v_flex` so it sizes to its content with no `flex_1` distribution
/// across items. The chip's dropdown wraps this in a `popover_style` div
/// for the opaque card chrome — that path doesn't go through `PopupMenu`
/// at all, sidestepping the per-`ElementItem` `flex_1`/`min_h(26)` wrapper
/// that was producing both the height-leak bug and the clip-to-26 bug.
///
/// Every clickable row routes through `Workspace::apply_permission_mode` so
/// the mode switch + notice + menu close stay in one place. `theme` is
/// consumed up front: every value used inside the `'static` row closures is
/// pre-extracted into owned `SharedString`/`Hsla`/`IconName`, so the
/// closures don't capture a short-lived theme reference.
fn build_permission_content(
    workspace: WeakEntity<Workspace>,
    current: PermissionMode,
    cx: &mut gpui::App,
) -> gpui::Div {
    let fg: gpui::Hsla = cx.theme().foreground;
    let muted: gpui::Hsla = cx.theme().muted_foreground;
    let info: gpui::Hsla = cx.theme().info;
    let warning: gpui::Hsla = cx.theme().warning;
    let danger: gpui::Hsla = cx.theme().danger;

    let make_row = |mode: PermissionMode,
                    title: SharedString,
                    subtitle: SharedString,
                    icon: IconName,
                    accent: gpui::Hsla,
                    selected: bool| {
        let ws = workspace.clone();
        h_flex()
            .id(("permission-mode-row", mode as usize))
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
                let _ = ws.update(cx, |this, cx| this.apply_permission_mode(mode, cx));
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
            PermissionMode::ReadOnly,
            i18n::t("workspace-mode-readonly-title"),
            i18n::t("workspace-mode-readonly-desc"),
            IconName::Eye,
            warning,
            current == PermissionMode::ReadOnly,
        ))
        .child(make_row(
            PermissionMode::WorkspaceWrite,
            i18n::t("workspace-mode-workspacewrite-title"),
            i18n::t("workspace-mode-workspacewrite-desc"),
            IconName::FolderOpen,
            info,
            current == PermissionMode::WorkspaceWrite,
        ))
        .child(make_row(
            PermissionMode::DangerFullAccess,
            i18n::t("workspace-mode-dangerfullaccess-title"),
            i18n::t("workspace-mode-dangerfullaccess-desc"),
            IconName::TriangleAlert,
            danger,
            current == PermissionMode::DangerFullAccess,
        ))
}

impl Workspace {
    /// Cycle the permission mode on the current thread (`/mode` no-args
    /// form): ReadOnly → WorkspaceWrite → DangerFullAccess → ReadOnly. The mode
    /// change notice rides `apply_permission_mode` so the conversation shows
    /// the switch.
    pub(crate) fn cycle_mode(&mut self, cx: &mut Context<Self>) {
        let next = match self
            .store
            .as_ref()
            .and_then(|s| {
                serde_json::from_value::<agent::thread::PermissionMode>(serde_json::Value::String(
                    s.read(cx).store.permission_mode.clone(),
                ))
                .ok()
            })
            .unwrap_or_else(|| self.thread.read(cx).permission_mode())
        {
            PermissionMode::ReadOnly => PermissionMode::WorkspaceWrite,
            PermissionMode::WorkspaceWrite => PermissionMode::DangerFullAccess,
            PermissionMode::DangerFullAccess => PermissionMode::ReadOnly,
        };
        self.apply_permission_mode(next, cx);
    }

    /// Apply `mode` and immediately send `prompt` as a user turn — the
    /// `/mode <name> [prompt]` form. Slash dispatch only fires while idle
    /// (the submit gate); `/mode` typed mid-turn parks in the follow-up
    /// queue as raw text like any other message.
    pub(crate) fn start_mode_turn(
        &mut self,
        mode: PermissionMode,
        prompt: String,
        cx: &mut Context<Self>,
    ) {
        self.apply_permission_mode(mode, cx);
        self.send_user_turn(prompt, Vec::new(), cx);
    }

    /// Switch the thread's `PermissionMode`, post a localized notice, and
    /// close the popover. Centralized so slash command, chip click, and the
    /// settings panel wiring all funnel through one path.
    pub(crate) fn apply_permission_mode(&mut self, mode: PermissionMode, cx: &mut Context<Self>) {
        let mode_key = match mode {
            PermissionMode::ReadOnly => "readonly",
            PermissionMode::WorkspaceWrite => "workspacewrite",
            PermissionMode::DangerFullAccess => "dangerfullaccess",
        };
        if !self.send_note(|sid| manox_protocol::ClientNote::SetApprovalMode {
            session_id: sid.into(),
            mode: mode_key.into(),
        }) {
            self.thread.update(cx, |t, _| t.set_permission_mode(mode));
        }
        self.add_info_message(
            i18n::t_str("workspace-mode-notice", &[("mode", mode_key)]).to_string(),
            NoticeAnchor::TurnEnd,
            None,
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
        ComposerPlacement, RecallDirection, RecallStep, Workspace, composer_key_context,
        composer_placement, editor_can_submit,
    };
    use gpui::InteractiveElement as _;
    use gpui::prelude::*;

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

    #[test]
    fn composer_key_context_admits_recall_only_while_it_can_apply() {
        // The popover context shadows everything.
        assert_eq!(composer_key_context(true, false, true), "completion = open");
        assert_eq!(composer_key_context(true, true, false), "completion = open");
        // Mid-walk recall and the empty input (where Up starts a recall)
        // keep the recall bindings matched...
        assert_eq!(
            composer_key_context(false, true, false),
            "composer = recall"
        );
        assert_eq!(
            composer_key_context(false, false, true),
            "composer = recall"
        );
        // ...and any other state leaves the caret to the Input's own keys.
        assert_eq!(composer_key_context(false, false, false), "composer");
    }

    /// Minimal composer harness for the recall keybinding tests: a wrapper
    /// carrying a configurable key context around a live input. With
    /// `consume_recall` set it also registers recall listeners like the
    /// workspace does; a recall action that reaches them is consumed even
    /// when it does nothing.
    struct RecallTestComposer {
        input: gpui::Entity<gpui_component::input::InputState>,
        context: &'static str,
        consume_recall: bool,
    }

    impl gpui::Render for RecallTestComposer {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            _cx: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            let mut wrap = gpui::div().key_context(self.context);
            if self.consume_recall {
                wrap = wrap
                    .on_action(|_: &crate::ComposerRecallUp, _window, _cx| {})
                    .on_action(|_: &crate::ComposerRecallDown, _window, _cx| {});
            }
            wrap.child(gpui_component::input::Input::new(&self.input))
        }
    }

    #[gpui::test]
    fn recall_bindings_defer_to_native_caret_with_typed_text(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        cx.update(gpui_component::init);
        cx.update(|cx| cx.bind_keys(crate::composer_recall_key_bindings()));
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot_for_window = slot.clone();
        let (_root, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| RecallTestComposer {
                input: cx
                    .new(|cx| gpui_component::input::InputState::new(window, cx).multi_line(true)),
                context: "composer = recall",
                consume_recall: false,
            });
            let input = view.read(cx).input.clone();
            *slot_for_window.borrow_mut() = Some(input);
            gpui_component::Root::new(view, window, cx)
        });
        cx.simulate_resize(gpui::size(gpui::px(640.), gpui::px(480.)));
        let input = slot.borrow().as_ref().expect("input initialized").clone();
        cx.update(|window, cx| input.update(cx, |state, cx| state.focus(window, cx)));

        // A multi-line draft under the static `composer = recall` context:
        // the recall binding wins every Up, but this harness registers no
        // recall listener, so the dispatched action stays unhandled and
        // GPUI falls back to the next matching binding — the Input's
        // native MoveUp still runs and the caret moves up a line.
        cx.simulate_input("hello\nworld");
        assert_eq!(
            input.read_with(cx, |state, _| state.selected_range().end),
            11
        );
        cx.simulate_keystrokes("up");
        let (row, end) = input.read_with(cx, |state, _| {
            let end = state.selected_range().end;
            (
                gpui_component::input::RopeExt::offset_to_position(state.text(), end).line,
                end,
            )
        });
        assert_eq!(row, 0, "native MoveUp moved the caret to the first line");
        assert!(end < 11);
        // Shift+Up is a selection, not a recall key, so it still extends the
        // selection without interference from the recall bindings. Move down
        // a line first so the selection has somewhere to extend from.
        cx.simulate_keystrokes("down");
        cx.simulate_keystrokes("shift-up");
        let (start, end) = input.read_with(cx, |state, _| {
            let range = state.selected_range();
            (range.start, range.end)
        });
        assert_ne!(start, end);
    }
    #[test]
    fn cap_tab_label_caps_with_ellipsis() {
        let long = "a very long tab label that must be capped";
        let capped = super::cap_tab_label(long);
        assert_eq!(capped.chars().count(), super::RIGHT_TAB_LABEL_CAP + 1);
        assert!(capped.ends_with('\u{2026}'), "{capped}");
        // Short labels pass through untouched; unicode caps on char bounds.
        assert_eq!(super::cap_tab_label("short"), "short");
        let unicode = "\u{4e2d}".repeat(super::RIGHT_TAB_LABEL_CAP + 4);
        assert_eq!(
            super::cap_tab_label(&unicode).chars().count(),
            super::RIGHT_TAB_LABEL_CAP + 1
        );
    }
    /// Right-pane state machine coverage: persisted-snapshot remap, the
    /// stash/restore round trip, orphan-tab drops, and db re-materialization
    /// across a simulated restart. Runs on an in-memory store so the real
    /// `~/.manox/threads.db` is never touched.
    #[gpui::test]
    async fn right_pane_state_machine(cx: &mut gpui::TestAppContext) {
        use super::{PersistedRightTab, RightTab};
        use gpui::AppContext as _;

        cx.update(gpui_component::init);
        let db_path =
            std::env::temp_dir().join(format!("manox-right-pane-test-{}.db", uuid_like_id()));
        let db = std::sync::Arc::new(
            agent::db::ThreadsDatabase::open(&db_path).expect("open temp threads db"),
        );
        cx.update(|_cx| {
            agent::runtime::init();
            agent::pi_providers::init();
            agent::thread_store::init_for_test(db.clone());
        });

        let captured: std::rc::Rc<std::cell::RefCell<Option<gpui::Entity<Workspace>>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = captured.clone();
        let window = cx.open_window(
            gpui::size(gpui::px(960.), gpui::px(640.)),
            move |window, cx| {
                let workspace = cx.new(|cx| Workspace::new(window, cx));
                *slot.borrow_mut() = Some(workspace.clone());
                gpui_component::Root::new(workspace, window, cx)
            },
        );
        cx.run_until_parked();
        let mut visual = gpui::VisualTestContext::from_window(window.into(), cx);
        let ws = captured.borrow().clone().expect("workspace captured");

        // ── persisted_right_pane: subagent filter + active remap ──────────
        visual.update(|_window, cx| {
            ws.update(cx, |ws, cx| {
                ws.right_tabs = vec![
                    RightTab::Editor,
                    RightTab::Subagent("s1".into()),
                    RightTab::Launcher,
                ];
                ws.active_right_tab = 2;
                ws.right_pane_visible = true;
                let p = ws.persisted_right_pane(cx);
                assert_eq!(p.tabs.len(), 2, "subagent tab must drop out");
                assert!(matches!(p.tabs[0], PersistedRightTab::Editor));
                assert!(matches!(p.tabs[1], PersistedRightTab::Launcher));
                assert_eq!(p.active, 1, "active remaps into the filtered list");
                assert!(p.visible);

                // An active subagent tab falls back to the head.
                ws.active_right_tab = 1;
                let p = ws.persisted_right_pane(cx);
                assert_eq!(p.active, 0);
            });
        });

        // ── stash → restore round trip (in-session) ────────────────────────
        visual.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.right_tabs = vec![RightTab::Editor, RightTab::Launcher];
                ws.active_right_tab = 1;
                ws.right_pane_visible = true;
                ws.stash_right_pane("threadA".into(), cx);
                assert!(ws.right_tabs.is_empty());
                assert!(!ws.right_pane_visible);
                ws.restore_right_pane("threadA", window, cx);
                assert_eq!(ws.right_tabs.len(), 2);
                assert!(matches!(ws.right_tabs[0], RightTab::Editor));
                assert!(matches!(ws.right_tabs[1], RightTab::Launcher));
                assert_eq!(ws.active_right_tab, 1);
                assert!(ws.right_pane_visible);
            });
        });
        // The stash wrote the db row keyed by thread.
        assert!(
            db.load_right_pane("threadA")
                .expect("db readable")
                .is_some()
        );

        // ── orphaned tabs drop on restore ──────────────────────────────────
        // A browser view closed and a session exited while another thread was
        // foreground: the stashed tabs referencing them must not come back.
        visual.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.right_tabs = vec![
                    RightTab::Browser(987_654),
                    RightTab::Session("external:claude:dead".into()),
                    RightTab::Editor,
                ];
                ws.active_right_tab = 2;
                ws.right_pane_visible = true;
                ws.stash_right_pane("threadB".into(), cx);
                ws.restore_right_pane("threadB", window, cx);
                assert_eq!(ws.right_tabs.len(), 1, "orphans dropped");
                assert!(matches!(ws.right_tabs[0], RightTab::Editor));
                assert_eq!(ws.active_right_tab, 0, "active remaps onto the survivor");
            });
        });

        // ── db re-materialization across a simulated restart ───────────────
        visual.update(|window, cx| {
            ws.update(cx, |ws, cx| {
                ws.right_tabs = vec![RightTab::Editor, RightTab::Launcher];
                ws.active_right_tab = 0;
                ws.right_pane_visible = true;
                ws.stash_right_pane("threadC".into(), cx);
                // Restart: the in-session stash is gone, only the db row
                // survives.
                ws.right_pane_by_thread.clear();
                ws.restore_right_pane("threadC", window, cx);
                assert_eq!(ws.right_tabs.len(), 2);
                assert!(matches!(ws.right_tabs[0], RightTab::Editor));
                assert!(matches!(ws.right_tabs[1], RightTab::Launcher));
                assert!(ws.right_pane_visible);
            });
        });

        // ── toggle on an empty pane lands on a fresh Launcher ──────────────
        visual.update(|_window, cx| {
            ws.update(cx, |ws, cx| {
                ws.right_tabs.clear();
                ws.right_pane_visible = false;
                ws.editor_open = true; // stale on purpose
                ws.toggle_right_pane(cx);
                assert!(ws.right_pane_visible);
                assert_eq!(ws.right_tabs.len(), 1);
                assert!(matches!(ws.right_tabs[0], RightTab::Launcher));
                assert!(!ws.editor_open, "a Launcher is never the editor");
            });
        });

        // Release the process-global store override so the gpui leak
        // detector doesn't trip on it at teardown.
        agent::thread_store::drop_for_test();
        let _ = std::fs::remove_file(&db_path);
    }

    /// Unique-ish id for temp files without pulling in a uuid dependency.
    fn uuid_like_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("{nanos}-{:?}", std::thread::current().id())
    }

    #[gpui::test]
    fn composer_context_without_recall_moves_caret_natively(cx: &mut gpui::TestAppContext) {
        use gpui::AppContext as _;
        cx.update(gpui_component::init);
        cx.update(|cx| cx.bind_keys(crate::composer_recall_key_bindings()));
        let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot_for_window = slot.clone();
        let (_root, cx) = cx.add_window_view(move |window, cx| {
            let view = cx.new(|cx| RecallTestComposer {
                input: cx
                    .new(|cx| gpui_component::input::InputState::new(window, cx).multi_line(true)),
                // The live composer outside recall: plain context, with the
                // workspace's recall listeners present.
                context: "composer",
                consume_recall: true,
            });
            let input = view.read(cx).input.clone();
            *slot_for_window.borrow_mut() = Some(input);
            gpui_component::Root::new(view, window, cx)
        });
        cx.simulate_resize(gpui::size(gpui::px(640.), gpui::px(480.)));
        let input = slot.borrow().as_ref().expect("input initialized").clone();
        cx.update(|window, cx| input.update(cx, |state, cx| state.focus(window, cx)));

        // Without the `composer = recall` context the recall bindings never
        // match, so Up/Down are the Input's native MoveUp/MoveDown even
        // though recall listeners are registered and would consume the
        // keystrokes if they fired.
        cx.simulate_input("hello\nworld");
        assert_eq!(
            input.read_with(cx, |state, _| state.selected_range().end),
            11
        );
        cx.simulate_keystrokes("up");
        let (row, end) = input.read_with(cx, |state, _| {
            let end = state.selected_range().end;
            (
                gpui_component::input::RopeExt::offset_to_position(state.text(), end).line,
                end,
            )
        });
        assert_eq!(row, 0, "native MoveUp moved the caret to the first line");
        assert!(end < 11);
        cx.simulate_keystrokes("down");
        let row = input.read_with(cx, |state, _| {
            gpui_component::input::RopeExt::offset_to_position(
                state.text(),
                state.selected_range().end,
            )
            .line
        });
        assert_eq!(row, 1, "native MoveDown moved the caret back down");
    }
}
