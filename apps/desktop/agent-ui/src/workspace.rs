//! Top-level workspace view.
//!
//! Holds a gpui-free `ThreadHandle` plus the AgentServer-backed
//! `ClientStoreHandle` mirror + `Entity<Sidebar>`;
//! `cx.subscribe` handles:
//! - `ThreadEvent`: text/thinking/tool deltas go to `ConversationState`; `ToolCallAuthorization` opens the question card;
//!   the terminal `Stop` (non-ToolUse) triggers the gateway list refetch.
//! - `SidebarEvent`: new conversation / open history / delete.
//!
//! Enter in the input box → append a user message + run_turn + persist (the sidebar shows the new entry immediately).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::i18n;
use crate::views::launcher::LauncherPick;
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
use manox_agent::PermissionDecision;
use manox_agent::collaboration_mode::PlanReviewChoice;
use manox_agent::language_model::StopReason;
use manox_agent::thread::PermissionMode;
use manox_agent::thread_engine::BrowserTabId;
use manox_agent::{Thread, ThreadEvent, ThreadId};
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
use manox_terminal::Terminal;
use terminal_ui::TerminalView;
use terminal_ui::terminal_proxy::TerminalProxy;

mod attach;
mod chips;
mod composer_render;
mod render;

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

/// Snapshot a thread's working directory as a `SharedString` for the
/// `TerminalPanel` prompt line. Reads the `Thread` entity (not the `Workspace`)
/// so it stays safe inside a `Workspace::update` closure, where reading the
/// `Workspace` itself would double-lease. `None` only when the path is empty.
fn thread_cwd(
    thread: &manox_agent::thread::ThreadHandle,
    store: &Option<gpui::Entity<ClientStoreHandle>>,
    cx: &App,
) -> Option<SharedString> {
    let cwd = store
        .as_ref()
        .map(|s| std::path::PathBuf::from(s.read(cx).store.cwd.clone()))
        .unwrap_or_else(|| thread.read(|t| t.cwd().to_path_buf()));
    if cwd.as_os_str().is_empty() {
        None
    } else {
        Some(SharedString::from(cwd.to_string_lossy().to_string()))
    }
}

/// Parse an `AskUserQuestion` tool input into a `PendingAsk`. The per-question
/// `InputState` entities are allocated lazily on first render (they need a
/// `Window`, which the event handler lacks). Returns `None` when the input is
/// malformed (the generic question overlay then takes over as a fallback).
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
mod composer;
mod external;
mod plan_review;
mod right_pane;

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

/// A non-question authorization parked on the user's decision — a
/// `sandbox_permissions` escalation from Edit/Write/Bash, or an
/// `AskUserQuestion` whose payload failed to parse. The ask card only
/// renders question payloads, so without this surface the pending call
/// blocks invisibly until the turn is cancelled.
pub(crate) struct PendingAuth {
    pub id: String,
    pub tool_name: String,
    pub summary: String,
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
/// input's keys for the popover; otherwise the wrapper just says `composer`,
/// which is what the recall bindings (`alt-up` / `alt-down`) hang off. Recall
/// never claims the bare arrows, so an open popover is the only state that has
/// to opt the composer out of it.
fn composer_key_context(completion_open: bool) -> &'static str {
    if completion_open {
        "completion = open"
    } else {
        "composer"
    }
}

struct DeferredUserTurn {
    text: String,
    images: Vec<manox_agent::language_model::MessageContent>,
    meta: UserTurnMeta,
    ui: manox_agent::MessageUiMetadata,
    user_images: Vec<UserImage>,
}

/// The Captain's dispatch prompt for one sub-agent address, with the Unix
/// second it was sent: a sub-agent panel's opening bubble shows the send time,
/// never the time its tab was opened.
#[derive(Debug, Clone)]
struct SubagentPrompt {
    text: String,
    dispatched_at: i64,
}

/// A thread parked in the background while still running a turn. U6b⑤: the
/// park holds NO kernel entity — just the id, the leaf (the wire-state home
/// whose mirrors the server's §D.5 deltas keep fed while the session stays
/// attached), and the parked subscription (it coordinates the settle
/// unread, the parked plan-review stash and the follow-up stash; it never
/// touches `conversation`/`self.thread`, so a background thread's events
/// cannot be misattributed to the foreground thread). The turn itself runs
/// server-side and survives parking regardless; nothing detaches or
/// disposes while parked — the reclaim is an in-place re-attach, no reopen.
struct BackgroundThread {
    id: String,
    store: Option<gpui::Entity<ClientStoreHandle>>,
    session_id: Option<String>,
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
/// prompt-macro (`manox_agent::command`) or a skill (`manox_agent::skill`).
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
    pub(crate) thread: manox_agent::thread::ThreadHandle,
    /// The `AgentServer`-backed `ClientStoreHandle` — the v2 `SessionStore`
    /// (journal window + projection face + echo map) fed by the multiplexer's
    /// follow stream. `None` until the workspace creates the AgentServer
    /// connection (landing thread); views read the store mirror. Held on
    /// the workspace for the next wiring step (re-handling the store on
    /// thread switch) — written at landing, read there.
    pub(crate) store: Option<gpui::Entity<ClientStoreHandle>>,
    /// T-D: the shared single-connection multiplexer. One app-level
    /// `AgentClient` (client_id "desktop") carries every session; the
    /// per-session handles are leaves fed by its demux pump.
    pub(crate) multiplexer: gpui::Entity<crate::multiplexer::SessionMultiplexer>,
    /// T-D: the shared app-level client used by the fire-and-forget
    /// `send_note` and `Reply` verdict paths (no per-session connection).
    pub(crate) client: std::sync::Arc<manox_session_core::agent_client::AgentClient>,
    /// γ-3: the AgentServer session_id for the landing thread. Used as the
    /// `session_id` field in `FromClient` commands.
    pub(crate) session_id: Option<String>,
    /// Threads that were running when the user switched away (U6b⑤: the
    /// turn runs server-side and survives the switch on its own — the park
    /// keeps the session ATTACHED so the reclaim re-attaches in place with
    /// no reopen, and the parked subscription keeps the settle unread, the
    /// plan-review stash and the follow-up stash coordinated).
    background_threads: Vec<BackgroundThread>,
    /// Generation counter for git-status refreshes: bumping it means any
    /// prior in-flight refresh self-cancels instead of overwriting newer
    /// state. The refresh runs on the global tokio runtime and delivers its
    /// result back via `async_channel`, the same bridge the worktree tool uses.
    git_status_gen: u64,
    pub(crate) sidebar: Entity<Sidebar>,
    /// Distinct bound-project paths of the active summaries, in list order
    /// (the project chip's "recent, unregistered" section; U2 push cache).
    thread_projects: Vec<String>,
    /// Registered project folders (chip menu + the sidebar grouping push).
    known_projects: Vec<String>,
    /// Repaint observer on the multiplexer's list/registry state (U2): its
    /// notify drives the sidebar rows and the workspace's model surfaces.
    _mux_lists: gpui::Subscription,
    pub(crate) conversation: Entity<ConversationState>,
    pub(crate) input_state: Entity<InputState>,
    /// Per-thread unsent composer text, keyed by thread id. Saved when
    /// switching away and restored on return, so each thread keeps its own
    /// in-progress draft instead of a single shared input bleeding across.
    drafts: HashMap<String, String>,
    /// Composer history-recall position into the newest-first user-turn texts;
    /// -1 means the walk is not running. Only `alt-up` / `alt-down` move along
    /// it, so nothing about the text or the caret has to be watched to leave it.
    recall_index: i64,
    /// The walk's working line: what the composer held when the walk was
    /// entered, or the last recalled turn once the user has changed it. `Down`
    /// past the newest turn restores it and ends the walk.
    recall_draft: Option<String>,
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
    subagent_transcripts: HashMap<String, Vec<manox_agent::SubagentChildEvent>>,
    /// Latest completion text per subagent address (from SubagentProgress
    /// status=Success/Error), used as the panel's final answer when the
    /// Agent tool-result is absent (new Steer bus has no ToolResult).
    subagent_final_text: HashMap<String, String>,
    /// The Captain's dispatch prompt per subagent address with its send time,
    /// captured from the Steer tool call so a panel always shows the opening
    /// user message — correctly attributed and timed — even before the child
    /// streams anything.
    subagent_prompts: HashMap<String, SubagentPrompt>,
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
    pending_auth: Option<PendingAuth>,
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
    active_browser_suites: Vec<manox_agent::engine::BrowserSuite>,
    /// True while a native directory picker is open from the "Choose project" row.
    /// Guards against the user submitting a message before the picker resolves
    /// (which would make `set_project` a silent no-op once `messages` is non-empty).
    project_picker_pending: bool,
    /// Parent directory selected for "Create blank project"; waiting for name input.
    blank_project_parent: Option<PathBuf>,
    /// Input state for the blank project folder name overlay.
    blank_project_name_input: Option<Entity<InputState>>,
    thread_sub: Option<Subscription>,
    /// Observes the foreground leaf store itself (beyond `thread_sub`'s
    /// events): a projection-only frame (mid-session `SetModel` →
    /// `Projections` delta) writes the chip fields and notifies the LEAF, but
    /// the entry event's re-render can land in an earlier tick — without this
    /// observe the workspace never repaints and the chip stays stale (the
    /// #765 "picks a model, nothing happens" repro: the journal had both
    /// changes, the render never showed them).
    store_observe: Option<Subscription>,
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

/// What a recall step does to the composer's content.
#[derive(Debug)]
enum RecallStep {
    /// Leave the input as it is (walk already at the oldest turn).
    None,
    /// Replace the input with this text — a past turn, or the walk's draft.
    Recall(String),
    /// The walk ended with an empty draft: clear the input.
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
        if let Some(home) = manox_agent::paths::home_dir() {
            cwd = home;
        }
        // L11: the process-global server — every window and the embedded
        // web UI share one AgentServer (one ownership/routing table).
        let agent_server = manox_session_core::agent_server::global(cwd.clone());
        // The landing thread id doubles as its AgentServer session id
        // (`CreateSession` uses the session id as the `ThreadId`), so the
        // thread the workspace renders and the thread the server drives are
        // the same conversation.
        let landing_id = uuid::Uuid::new_v4().to_string();
        let thread =
            Thread::landing_with_id(manox_agent::ThreadId(landing_id.clone()), cwd.clone());
        let client = std::sync::Arc::new(manox_session_core::agent_client::AgentClient::connect(
            &agent_server,
            "desktop",
            vec![
                manox_protocol::handshake::HookKind::Approve,
                manox_protocol::handshake::HookKind::PlanVerdict,
                manox_protocol::handshake::HookKind::AskUserQuestion,
            ],
            vec![],
        ));
        let multiplexer =
            cx.new(|cx| crate::multiplexer::SessionMultiplexer::with_client(client.clone(), cx));
        let (store, session_id) = {
            let session_id = landing_id.clone();
            let store = multiplexer.update(cx, |m, cx| {
                let handle =
                    m.open_or_create(&session_id, cwd.to_str().unwrap_or_default(), false, cx);
                // GW5: the landing session is the focused one from tick
                // one — its leaf suppresses unread rises while attached.
                m.set_focused(Some(&session_id), cx);
                handle
            });
            (store, session_id)
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
        // U2 list source + GW5 badge source: rows are the multiplexer's wire
        // list, and badges prefer the leaves' client-owned unread mirrors.
        sidebar.update(cx, |s, _| s.bind_multiplexer(multiplexer.clone()));
        // U6a/U6b②: no store handle at all — the list refresh rides the
        // server's watcher broadcast, and the attach path is the landing
        // mirror (the wire owns the session state).
        // The multiplexer's notify (its list/registry state changed)
        // repaints the sidebar rows and the workspace's model surfaces, and
        // feeds the chip-menu caches off the wire state (U2 cross-domain
        // #1: the store-read decoration snapshot retired — the registry
        // rides the Projects mirror, the per-thread projects ride the rows).
        let _mux_lists = cx.observe(&multiplexer, |this, _, cx| {
            let (projects, known) = {
                let m = this.multiplexer.read(cx);
                let mut projects: Vec<String> = Vec::new();
                for row in m.thread_list() {
                    if let Some(project) = row.project.as_deref()
                        && !project.is_empty()
                        && !projects.iter().any(|p| p == project)
                    {
                        projects.push(project.to_string());
                    }
                }
                (projects, m.known_projects().to_vec())
            };
            this.thread_projects = projects;
            this.known_projects = known;
            this.sidebar.update(cx, |_, cx| cx.notify());
            cx.notify();
        });
        let recipient = thread.read(|t| t.self_author());
        let conversation = cx.new(|_| ConversationState::new(recipient));
        let context_rail =
            { cx.new(|_| crate::views::context_rail::ContextRail::new(Some(store.clone()))) };
        let weak_ws = cx.weak_entity();
        context_rail.update(cx, |r, _| r.set_workspace(weak_ws));

        let mut ws = Self {
            cwd,
            thread,
            store: Some(store),
            multiplexer,
            client,
            session_id: Some(session_id),
            background_threads: Vec::new(),
            git_status_gen: 0,
            sidebar,
            thread_projects: Vec::new(),
            known_projects: Vec::new(),
            _mux_lists,
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
            pending_auth: None,
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
            recall_draft: None,
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
            store_observe: None,
            sidebar_sub: None,
            input_sub: None,
            editor_sub: None,
            conversation_sub: None,
            list_state: ListState::new(0, ListAlignment::Bottom, MSG_LIST_OVERDRAW),
            message_list_width: crate::views::MessageListWidthInvalidator::default(),
            list_count: 0,
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
        let (thread_events, store_changes) = ws.subscribe_thread(cx);
        ws.thread_sub = Some(thread_events);
        ws.store_observe = Some(store_changes);
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
        thread: manox_agent::thread::ThreadHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.attach_thread(thread, false, window, cx);
    }
    /// Emit a `ThreadEvent` on the store bound to `thread_id` — the foreground
    /// store when the id is the active thread, otherwise the parked
    /// background thread's store. Lets tests drive the workspace's subscription
    /// handler without a live AgentServer round-trip.
    #[cfg(feature = "test-support")]
    pub fn diagnostic_emit_event(
        &self,
        thread_id: &str,
        event: ThreadEvent,
        cx: &mut Context<Self>,
    ) {
        let store = if self.thread.read(|t| t.id.0.as_str() == thread_id) {
            self.store.as_ref()
        } else {
            self.background_threads
                .iter()
                .find(|b| b.id == thread_id)
                .and_then(|bg| bg.store.as_ref())
        };
        if let Some(store) = store {
            store.update(cx, |_, cx| cx.emit(event));
        }
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
        // Only AskUserQuestion payloads reach this seeder (the live event
        // path is `ToolCallAuthorization`, which carries the real tool
        // name); the fallback exists solely for a malformed ask payload, so
        // the constant names the tool that actually fired.
        self.pending_auth = self.pending_ask.is_none().then(|| PendingAuth {
            id: id.to_string(),
            tool_name: "AskUserQuestion".to_string(),
            summary: String::new(),
        });
        self.ask_step = 0;
        self.ask_transition_gen = self.ask_transition_gen.wrapping_add(1);
        cx.notify();
    }

    /// Seed a non-question authorization (a sandbox escalation, or an ask
    /// whose payload failed to parse) as the pending generic card.
    /// Diagnostic-only: mirrors what the `ToolCallAuthorization` handler
    /// stores for a non-ask payload.
    #[cfg(feature = "test-support")]
    pub fn diagnostic_seed_auth(
        &mut self,
        id: &str,
        tool_name: &str,
        summary: &str,
        cx: &mut Context<Self>,
    ) {
        self.pending_ask = None;
        self.pending_auth = Some(PendingAuth {
            id: id.to_string(),
            tool_name: tool_name.to_string(),
            summary: summary.to_string(),
        });
        self.ask_step = 0;
        cx.notify();
    }

    /// The pending generic-approval card as (id, tool_name, summary).
    #[cfg(feature = "test-support")]
    pub fn diagnostic_pending_auth(&self) -> Option<(String, String, String)> {
        self.pending_auth
            .as_ref()
            .map(|a| (a.id.clone(), a.tool_name.clone(), a.summary.clone()))
    }

    /// Whether any blocking overlay (plan review, ask, generic approval,
    /// blank project) is up.
    #[cfg(feature = "test-support")]
    pub fn diagnostic_blocking_overlay_active(&self) -> bool {
        self.blocking_overlay_active()
    }

    /// Resolve the pending generic-approval card. Diagnostic-only wrapper
    /// around `resolve_auth`; the fake engine accepts the id.
    #[cfg(feature = "test-support")]
    pub fn resolve_auth_for_test(&mut self, decision: PermissionDecision, cx: &mut Context<Self>) {
        self.resolve_auth(decision, cx);
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

    /// Rebuild the conversation view from the thread's v2 display fold. The
    /// trigger is the follow stream's authoritative history boundary
    /// (`WindowChange::Replace` → `ThreadEvent::HistoryRestored`, §D.1); the
    /// T10c-era successor of the deleted `ThreadHistory` note replay.
    pub(crate) fn rebuild_conversation_from_thread(&mut self, cx: &mut Context<Self>) {
        let display: Vec<manox_agent::db::HistoryEntry> = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.display.clone())
            .expect("foreground store present");
        let subagent_rows = manox_agent::subagent_restore::rebuild_from_messages(
            &self
                .store
                .as_ref()
                .map(|s| s.read(cx).store.derived_messages())
                .expect("foreground store present"),
        );
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
                            manox_agent::TokenUsage {
                                input_tokens: v.input,
                                output_tokens: v.output,
                                cache_creation_input_tokens: v.cache_creation,
                                cache_read_input_tokens: v.cache_read,
                            },
                        )
                    })
                    .collect()
            })
            .expect("foreground store present");
        let role = self.model_label(cx);
        let recipient = self.recipient_author();
        let weak = cx.weak_entity();
        let running = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .expect("foreground store present");
        let cwd = thread_cwd(&self.thread, &self.store, cx);
        let new_conv = cx.new(|cx| {
            ConversationState::rebuild_from_display(
                &display,
                &usage,
                &role,
                recipient,
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
        // `Tail` natively pins to the end and keeps following; an upward user
        // scroll disengages it and landing back at the bottom re-arms it.
        self.list_state.set_follow_mode(FollowMode::Tail);
        // Recover the settled sub-agent observation rows alongside the
        // conversation: a restored transcript is the only record of runs that
        // finished (or were killed) before the restart / switch.
        self.apply_subagent_rows(subagent_rows, cx);
        cx.notify();
    }

    /// Wire the workspace to the foreground leaf: the `ThreadEvent` stream
    /// drives the conversation surface, and the entity-level observe repaints
    /// on projection-only frames (the mid-session chip updates — see the
    /// `store_observe` field note).
    fn subscribe_thread(&self, cx: &mut Context<Self>) -> (Subscription, Subscription) {
        let store = self
            .store
            .clone()
            .expect("subscribe_thread requires the foreground store");
        let observe = cx.observe(&store, |_, _, cx| cx.notify());
        let events = cx.subscribe(&store, |this, _store, ev: &ThreadEvent, cx| {
            match ev {
                ThreadEvent::ToolCallAuthorization {
                    id,
                    tool_name,
                    summary,
                    input,
                } => {
                    // AskUserQuestion-shaped payloads surface as the question
                    // card; everything else (a sandbox_permissions escalation
                    // from Edit/Write, a malformed ask) surfaces as the
                    // generic approval card — a parked thread must never wait
                    // invisibly.
                    this.pending_ask = parse_pending_ask(id.clone(), input.clone());
                    this.pending_auth = this.pending_ask.is_none().then(|| PendingAuth {
                        id: id.clone(),
                        tool_name: tool_name.clone(),
                        summary: summary.clone(),
                    });
                    this.ask_step = 0;
                    this.ask_transition_gen = this.ask_transition_gen.wrapping_add(1);
                    this.context_rail.update(cx, |r, cx| {
                        r.cockpit_phase = CockpitPhase::AwaitingApproval;
                        cx.notify();
                    });
                    // U3a: the pending-auth badge is the server pump's
                    // store write + SessionStatus delta (single writer,
                    // §F.2) — a parked thread blocked on this authorization
                    // keeps its badge through the pump, not through a
                    // desktop mirror write that only raced it.
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
                    // The AgentServer pump already persisted the pending
                    // verdict flag on PlanReady (agent_server.rs), so a
                    // restart re-emits the card without a UI-side write —
                    // and U3a: the pump's store write + SessionStatus delta
                    // are the badge's single writer. The sidebar row pauses
                    // its spinner (blue static) while the verdict is due;
                    // `respond_plan_review` releases it.
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
                // v2 (T10c, §D.1): the follow stream's authoritative history
                // boundary (`WindowChange::Replace` from the opening
                // Snapshot / a seamless re-open) re-arms the conversation
                // rebuild — the role the deleted `ThreadHistory` note's
                // `restored` flag used to play. Without this a reopened
                // thread strands on the loading screen; a window change that
                // rewrites the branch is corrected to the authoritative
                // active branch here.
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
                        .map(|s| s.read(cx).store.derived_messages())
                        .expect("foreground store present");
                    if let Some(snapshot) = manox_agent::plan::rebuild_from_messages(&messages)
                        .or_else(|| {
                            this.store
                                .as_ref()
                                .and_then(|s| s.read(cx).store.persisted_plan.as_ref())
                                .and_then(|v| {
                                    serde_json::from_value::<manox_agent::plan::PlanSnapshot>(
                                        v.clone(),
                                    )
                                    .ok()
                                })
                        })
                    {
                        this.context_rail
                            .update(cx, |r, cx| r.set_plan(snapshot, cx));
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
                    // U3a: the running indicator is the server pump's store
                    // write + SessionStatus delta (single writer) — it still
                    // lights before the first streaming delta arrives (the
                    // pump sees TurnStarted off the same facade broadcast).
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
                        .expect("foreground store present");
                    // Cross-domain #5: the wire refetch replaces the kernel
                    // rescan trigger — the server self-holds the scan in its
                    // ListThreads answer.
                    this.multiplexer.update(cx, |m, _| m.fetch_thread_list());
                    // U3a: the settle flags (idle / pending-plan / errored)
                    // are the server pump's store writes — one writer, no
                    // race with this former mirror. The pump clears
                    // pending-plan unconditionally at settle; the review
                    // card's own demote below is UI state, not a store flag.
                    this.turn_active = false;
                    this.background_threads.retain(|b| b.id != thread_id);
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
                    let usage = this.store.as_ref().and_then(|s| {
                        s.read(cx).store.last_token_usage.as_ref().map(|u| {
                            manox_agent::TokenUsage {
                                input_tokens: u.input,
                                output_tokens: u.output,
                                cache_creation_input_tokens: u.cache_creation,
                                cache_read_input_tokens: u.cache_read,
                            }
                        })
                    });
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
                        this.multiplexer.update(cx, |m, _| m.fetch_thread_list());
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
                                                serde_json::from_value::<
                                                    manox_agent::goal::ThreadGoal,
                                                >(v)
                                                .ok()
                                            })
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
                    // U3b: the background-work flag is the server pump's
                    // store write + delta (its BackgroundTaskUpdated arm
                    // computes the same thread_has_running_tasks); the
                    // event still falls through to the conversation's
                    // task-card dispatch below.
                    // U3b: the pending-auth badge drops at VERDICT time on
                    // the server (clear_pending_auth_if_settled at all four
                    // settle points + the §D.5 delta). The tool-traffic
                    // heuristic this replaces only ran in-proc, only for
                    // this client, and raced the actual verdict.
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
                            .expect("foreground store present");
                        // U3b: the Error edge is the server pump's —
                        // mark_idle, the errored flag and the full badge
                        // clear ride its store write + the single delta
                        // carrying the whole set. GW5 kept: no unread rise
                        // for the FOREGROUND error — the user is watching
                        // it; the errored triangle is the signal
                        // (client-owned unread, §F.2).
                        this.turn_active = false;
                        this.background_threads.retain(|b| b.id != thread_id);
                        // Persist the error card so a reloaded thread reproduces
                        // what went wrong at the failed turn's position. The
                        // append rides the actor queue behind the settling run;
                        // a crash before the actor drains it loses the card
                        // (accepted window for an annotation).
                        this.append_ui_note(
                            manox_agent::db::UiNoteKind::Error,
                            e.to_string(),
                            None,
                            cx,
                        );
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
                            manox_agent::ToolCallStatus::Success
                                | manox_agent::ToolCallStatus::Error
                                | manox_agent::ToolCallStatus::Denied
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
                            this.subagent_prompts.insert(
                                addr.to_string(),
                                SubagentPrompt {
                                    text: prompt.to_string(),
                                    dispatched_at: chrono::Utc::now().timestamp(),
                                },
                            );
                        }
                    }
                    let weak = cx.weak_entity();
                    let role = this.model_label(cx);
                    let usage = this.store.as_ref().and_then(|s| {
                        s.read(cx).store.last_token_usage.as_ref().map(|u| {
                            manox_agent::TokenUsage {
                                input_tokens: u.input,
                                output_tokens: u.output,
                                cache_creation_input_tokens: u.cache_creation,
                                cache_read_input_tokens: u.cache_read,
                            }
                        })
                    });
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
        });
        (events, observe)
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
                        .expect("foreground store present");
                    let store = manox_agent::thread_store_global();
                    store.with_mut(|s| s.archive_thread(id, *archived));
                    // Sync the in-memory flag so the title-bar menu label stays
                    // fresh when the sidebar archives the currently active thread.
                    if is_current {
                        let _ = this.send_note(|sid| manox_protocol::ClientNote::ArchiveThread {
                            session_id: sid.into(),
                            archived: *archived,
                        });
                    }
                    // Archiving the active thread navigates away to a fresh
                    // empty thread (Hero view) so the user doesn't stare at a
                    // ghost conversation that just vanished from the sidebar.
                    if *archived && is_current {
                        this.start_new_thread(None, window, cx);
                    }
                }
                SidebarEvent::SetThreadTag(id, tag) => {
                    let store = manox_agent::thread_store_global();
                    store.with_mut(|s| s.set_thread_tag(id, tag.clone()));
                }
                SidebarEvent::RemoveProject(path) => {
                    // Unregister the folder; the sidebar drops the group and
                    // its threads fall back to the loose Conversations list.
                    // Conversation history is never touched.
                    let store = manox_agent::thread_store_global();
                    store.with_mut(|s| s.remove_project(&path.to_string_lossy()));
                }
            },
        )
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
                    // U2: the popover lists the gateway's command snapshot.
                    slash_source(&det.query, self.multiplexer.read(cx).commands())
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
            || self.pending_auth.is_some()
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
                TurnNavigatorEvent::FillComposer { text } => {
                    let text = text.clone();
                    this.close_turn_navigator(window, cx);
                    this.fill_composer_from_turn(text, window, cx);
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
            .expect("foreground store present");
        let worktree_branch = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.with(|st| st.branch.clone()));
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

    /// The agent whose conversation this workspace renders — every user
    /// bubble's header `to`. `lead`-labeled threads show the localized Captain
    /// label; a team member thread shows its own member name.
    fn recipient_author(&self) -> manox_agent::MessageAuthor {
        self.thread.read(|t| t.self_author())
    }

    fn user_turn_meta(&self, cx: &mut Context<Self>) -> UserTurnMeta {
        let permission_mode = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.permission_mode)
            .expect("foreground store present");
        UserTurnMeta::new(
            chrono::Utc::now().timestamp(),
            self.model_label(cx),
            Some(permission_mode),
        )
    }

    fn message_ui_metadata(meta: &UserTurnMeta) -> manox_agent::MessageUiMetadata {
        manox_agent::MessageUiMetadata {
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
            // Selector read face (§J11): the composer model chip derives from
            // the store's projection-materialized model, falling back to the
            // bound thread mirror only while no projection has landed yet.
            // T10c: the v1 `model_name` human label (CurrentModel/ThreadInfo
            // notes) is gone — the `model` projection carries the canonical
            // wire identity only; display names resolve via the provider glue.
            self.store
                .as_ref()
                .and_then(|s| s.read(cx).store.with(|st| st.model_id.clone()))
                .unwrap_or_else(|| {
                    self.thread
                        .read(|t| t.model().cloned())
                        .map(|model| manox_agent::provider_glue::display_name(&model))
                        .unwrap_or_else(|| i18n::t("workspace-no-model").to_string())
                })
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
        self.append_ui_note(manox_agent::db::UiNoteKind::Notice, text, tool_call_id, cx);
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
        kind: manox_agent::db::UiNoteKind,
        text: String,
        tool_call_id: Option<&str>,
        _cx: &mut Context<Self>,
    ) {
        let mut data = serde_json::json!({ "text": text });
        // A tool-anchored notice carries the tool call id so the rebuild can
        // splice it next to the tool item, matching the live placement.
        // `data` is raw JSON — no schema change.
        if let Some(id) = tool_call_id {
            data["tool_call_id"] = serde_json::Value::String(id.to_owned());
        }
        let kind_str = match kind {
            manox_agent::db::UiNoteKind::Error => "error",
            manox_agent::db::UiNoteKind::Notice => "notice",
            manox_agent::db::UiNoteKind::PlanReview => "plan_review",
        };
        let _ = self.send_note(|sid| manox_protocol::ClientNote::AppendUiNote {
            session_id: sid.into(),
            kind: kind_str.into(),
            data: data.clone(),
        });
    }

    /// Abort the current turn.
    pub(crate) fn cancel_turn(&mut self, cx: &mut Context<Self>) {
        let _ = self.send_note(|sid| manox_protocol::ClientNote::CancelTurn {
            session_id: sid.into(),
        });
        cx.notify();
    }

    /// Send a `ClientNote` to the AgentServer when the landing-thread
    /// connection is available (γ-3 mutation path). Returns `true` when the
    /// note was sent; the caller falls back to `self.thread.update` when `false`.
    pub(crate) fn send_note(
        &self,
        note_fn: impl FnOnce(&str) -> manox_protocol::ClientNote,
    ) -> bool {
        if let Some(sid) = &self.session_id {
            self.client.send_note(note_fn(sid));
            true
        } else {
            false
        }
    }

    /// v2 §D.2 submit path: mint an `origin_rpc` correlation id, register the
    /// optimistic echo in the foreground store, and send the
    /// [`ClientCall::Submit`] (receipt-only per L7 — the durable user row
    /// arrives through the follow stream and retires the echo by matching its
    /// `originRpc`). The conversation's optimistic bubble was already pushed by
    /// the caller; retirement just clears the store's echo bookkeeping so the
    /// row is not treated as a remote (unmatched) insertion.
    ///
    /// Returns `true` when the submit rode the connection (session bound).
    pub(crate) fn send_submit_v2(
        &mut self,
        text: String,
        images: Vec<manox_protocol::ImageAttachment>,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(sid) = self.session_id.clone() else {
            tracing::warn!("submit dropped: no session bound to the workspace");
            return false;
        };
        tracing::info!(session_id = %sid, "submit v2 sent");
        let origin_rpc = uuid::Uuid::new_v4().to_string();
        if let Some(store) = self.store.as_ref() {
            store.update(cx, |h, _| {
                h.store.push_echo(&origin_rpc, text.clone());
            });
        }
        self.client.send_call(manox_protocol::ClientCall::Submit {
            session_id: sid,
            text,
            images,
            origin_rpc: Some(origin_rpc),
        });
        true
    }
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
            .map(|s| s.read(cx).store.permission_mode)
            .expect("foreground store present")
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
        // The wire value is the serde (kebab-case) form so the AgentServer's
        // `from_value::<PermissionMode>` round-trips; `mode_key` stays the
        // lowercase form the i18n `workspace-mode-notice` selector keys on.
        let mode_wire = serde_json::to_value(mode)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let _ = self.send_note(|sid| manox_protocol::ClientNote::SetApprovalMode {
            session_id: sid.into(),
            mode: mode_wire,
        });
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

/// Whether the gateway's command snapshot (§D.5 `Commands` / `ListCommands`)
/// registers `name` under `kind` (`"command"` for macros, `"skill"` for
/// skills). U2: the slash-dispatch hit check reads the wire projection of the
/// server's command/skill registries instead of the in-process kernel
/// registries, so a remote server's registry decides the hit. Builtins share
/// the `"command"` kind but never reach this check (they dispatch through
/// their own `SlashCommand::execute`, and the registry adapters skip
/// builtin-named keys at init).
fn wire_commands_has(commands: &serde_json::Value, name: &str, kind: &str) -> bool {
    commands.as_array().is_some_and(|entries| {
        entries.iter().any(|e| {
            e.get("name").and_then(|v| v.as_str()) == Some(name)
                && e.get("kind").and_then(|v| v.as_str()) == Some(kind)
        })
    })
}

/// Read the sidebar decoration columns the wire `ThreadListItem` does not
/// carry yet (U2 dual-track): per-thread project/tag/approval-mode plus the
/// registered-project folder list. Returns `(meta by id, distinct active
/// project paths in list order, known projects)`. The meta map spans both
/// store partitions (active rows win) so a tag lookup addresses archived
/// rows too; the project-path list drives the chip's "recent, unregistered"
/// section. This is the last kernel read feeding the sidebar — the cross-
/// domain ask is to extend §D.5 `ThreadsUpdated` with these columns so it
/// retires.
#[cfg(test)]
mod tests;
