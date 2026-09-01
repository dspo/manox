//! Conversation history sidebar.
//!
//! A standalone gpui Entity that subscribes to the gpui-free `ThreadStore`
//! (via a `StoreHandle` event pump) and lists past threads. Clicking a
//! conversation entry emits `OpenThread(id)`; the "Conversations" section header's "+" opens the
//! flat new-session menu and each project folder header's ellipsis button opens the project
//! action menu (new session / terminal / VS Code / remove project). Workspace subscribes to
//! these events.
//!
//! Threads bound to a project (chosen on the first screen) are grouped under a collapsible folder
//! in the "Projects" section, keyed by project path; the rest fall under "Conversations". The top
//! menu and bottom account footer are static decoration.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use crate::i18n;
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, Pixels, Render, ScrollHandle, SharedString, Subscription, Transformation,
    WeakEntity, Window, deferred, ease_in_out, linear, percentage, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{PopupMenu, PopupMenuItem},
    tag::{Tag, TagVariant},
    tooltip::Tooltip,
    v_flex,
};
use manox_agent::thread::PermissionMode;
use manox_agent::thread_store::StoreHandle;

/// How far the row wash translates (in pixels, clipped to the row) during the
/// selection-slide. The two adjacent rows animate in opposite directions so
/// the wash reads as moving from the old row to the new one.
const SELECT_SLIDE_PX: f32 = 28.;

/// Vertical direction from the previously-selected row to the newly-selected
/// one, used to angle the slide. `None` (e.g. the two rows are in different
/// sections, or one is off-screen) falls back to a plain fade.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SlideDir {
    Up,
    Down,
    None,
}

/// Per-frame snapshot of the selection transition, shared with every row so
/// each can decide whether it is the incoming or outgoing end of the slide.
#[derive(Clone)]
struct SlideCtx {
    selecting_id: Option<String>,
    deselecting_id: Option<String>,
    dir: SlideDir,
    gen_id: u64,
}

/// Which section header the sticky overlay above the scroll body shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PinnedSection {
    Projects,
    Conversations,
}

/// Section-header pinning rule for the sticky overlay: the Projects header
/// stays pinned while the viewport is inside the projects section; once the
/// projects content has scrolled fully past the top (`scroll_top >=
/// projects_height`) the Conversations header takes over. `projects_height`
/// is `None` before the scroll container has been laid out — the overlay only
/// appears once `scroll_top > 0`, which cannot happen before the first
/// layout, so this branch is just a safety fallback.
fn pinned_section(
    projects_present: bool,
    scroll_top: Pixels,
    projects_height: Option<Pixels>,
) -> PinnedSection {
    if !projects_present {
        return PinnedSection::Conversations;
    }
    match projects_height {
        None => PinnedSection::Projects,
        Some(h) if scroll_top < h => PinnedSection::Projects,
        _ => PinnedSection::Conversations,
    }
}

/// Which end of the slide a row is playing this frame.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AnimRole {
    /// The newly-selected row: wash fades in, settling toward its resting spot.
    Selecting,
    /// The previously-selected row: wash fades out, drifting toward the new row.
    Deselecting,
    /// Neither — no wash overlay (hover handles non-selected feedback).
    None,
}

/// A row in the Conversations list — either a manox thread or an external
/// agent CLI session, unified so the two can be merged and ordered by recency
/// instead of living in separate sections. Both row kinds share the
/// selection-slide: their ids join one `flat_ids` ordering and `render_thread_item`
/// applies the same `SlideCtx` wash to either.
enum SidebarRow {
    Thread(manox_agent::ThreadSummary),
    External(crate::external_session::ExternalSessionSummary),
}

impl SidebarRow {
    /// Recency sort key (newest first). Threads use `interacted_at`; external
    /// sessions use their spawn `created_at` — manox cannot observe in-TUI
    /// interaction, so the spawn time is the only signal it has.
    fn sort_key(&self) -> i64 {
        match self {
            Self::Thread(s) => s.interacted_at,
            Self::External(s) => s.created_at,
        }
    }

    fn id(&self) -> &str {
        match self {
            Self::Thread(s) => s.id.as_str(),
            Self::External(s) => s.id.as_str(),
        }
    }
}

/// Row geometry + team nesting metadata for [`SidebarThreadItem::from_thread`]:
/// `indent` offsets members under their leader, `nested` draws the left guide
/// rail.
struct RowNesting {
    indent: gpui::Pixels,
    team_leader: bool,
    team_collapsed: bool,
    /// Nested row (team member or fork child): draws the left guide rail.
    nested: bool,
}

/// One ordered sidebar row plus its team nesting metadata: `indent` offsets
/// members under their leader, `team_leader` marks a row that can collapse
/// its member group, `team_collapsed` the fold state.
struct ThreadRender {
    row: SidebarRow,
    indent: f32,
    team_leader: bool,
    team_collapsed: bool,
}

/// Events the sidebar emits to the Workspace.
#[derive(Debug, Clone)]
pub enum SidebarEvent {
    OpenThread(String),
    NewThread,
    /// New thread bound to a specific project path.
    NewThreadWithProject(PathBuf),
    /// User clicked archive/unarchive. The bool is the new archived state.
    ArchiveThread(String, bool),
    /// User set or cleared a thread row's tag (overflow menu / chip clear
    /// button). `None` removes the tag.
    SetThreadTag(String, Option<String>),
    /// Launch an external agent CLI session with a user-picked provider +
    /// model (the cascade wizard's terminal action). The kind identifies the
    /// agent (`claude` / `codex` / `copilot`); the strings are provider name +
    /// model id; the optional wire key pins the endpoint variant of the model
    /// (`anthropic` / `responses` / `completions`); the optional PathBuf is
    /// the project path to use as the CLI's cwd (when launched from a
    /// project folder's menu).
    SpawnExternalSession(
        crate::external_session::SessionKind,
        String,
        String,
        Option<String>,
        Option<PathBuf>,
    ),
    /// Launch a plain PTY session with no cx provider injection — the user's
    /// shell (`Terminal`). The optional PathBuf is the project path to use
    /// as the session's cwd (when launched from a project folder's menu);
    /// `None` falls back to the workspace cwd.
    SpawnPlainSession(crate::external_session::SessionKind, Option<PathBuf>),
    /// Launch VS Code with injection resolved from the persisted
    /// `vscode_app:` settings (single entry — no provider/model choice at
    /// launch time). The optional PathBuf is the project path the menu was
    /// opened from — VS Code opens that directory (falls back to the
    /// workspace cwd in the handler).
    LaunchVSCode(Option<PathBuf>),
    /// Switch the main area to an already-running external session.
    OpenExternalSession(String),
    /// Archive an external session from the sidebar row's hover action (the
    /// unified "Inbox" button threads also use): kill the agent and drop it
    /// from the sidebar — the same path as closing the tab.
    ArchiveExternalSession(String),
    /// User removed a project folder from the sidebar (project action menu).
    /// The store unregisters the path: the folder disappears and its threads
    /// fall back to the loose Conversations list. Conversation history is
    /// never touched.
    RemoveProject(PathBuf),
}

/// Order threads as a team forest: top-level rows (threads + externals)
/// merged by recency, each leader followed by its indented member subtree.
/// Orphans and cycles stay top-level (`depth` is zeroed for them at the
/// store); a collapsed leader hides its subtree.
fn team_forest(
    team_collapsed: &HashSet<String>,
    threads: &[manox_agent::ThreadSummary],
    externals: &[crate::external_session::ExternalSessionSummary],
) -> Vec<ThreadRender> {
    let mut members: HashMap<&str, Vec<&manox_agent::ThreadSummary>> = HashMap::new();
    let ids: HashSet<&str> = threads.iter().map(|s| s.id.as_str()).collect();
    for s in threads {
        if s.depth > 0
            && let Some(parent) = s.parent_id.as_deref()
        {
            members.entry(parent).or_default().push(s);
        }
    }
    let mut top: Vec<SidebarRow> = threads
        .iter()
        .filter(|s| {
            // A member whose leader lives in another partition (e.g. an
            // archived leader) cannot nest here: emit it top-level like the
            // webview forest, so archiving a leader never hides its members.
            s.depth == 0 || !s.parent_id.as_deref().is_some_and(|p| ids.contains(p))
        })
        .cloned()
        .map(SidebarRow::Thread)
        .chain(externals.iter().cloned().map(SidebarRow::External))
        .collect();
    top.sort_by_key(|r| std::cmp::Reverse(r.sort_key()));

    let mut out = Vec::new();
    for row in top {
        let id = row.id().to_string();
        // Only threads can lead a team; external CLI sessions never do.
        let team_leader = match &row {
            SidebarRow::Thread(s) => members.contains_key(s.id.as_str()),
            SidebarRow::External(_) => false,
        };
        out.push(ThreadRender {
            row,
            indent: 0.0,
            team_leader,
            team_collapsed: team_collapsed.contains(&id),
        });
        if team_leader && !team_collapsed.contains(&id) {
            let mut kids = members.get(id.as_str()).cloned().unwrap_or_default();
            kids.sort_by_key(|k| std::cmp::Reverse(k.interacted_at));
            for kid in kids {
                push_member(team_collapsed, &mut out, kid, 1.0, &members);
            }
        }
    }
    out
}

/// Render-side nesting cap, mirroring the store's `MAX_TEAM_DEPTH`. The
/// store zeroes cycle/orphan depths, so a deep tree here means corrupt wire
/// data; the cap keeps the recursion from stacking forever as a last line of
/// defense.
const MAX_TEAM_RENDER_DEPTH: f32 = 8.0;

/// Append a member row and its subtree (recursively) at the given indent.
fn push_member(
    team_collapsed: &HashSet<String>,
    out: &mut Vec<ThreadRender>,
    s: &manox_agent::ThreadSummary,
    depth: f32,
    members: &HashMap<&str, Vec<&manox_agent::ThreadSummary>>,
) {
    let has_kids = members.contains_key(s.id.as_str());
    out.push(ThreadRender {
        row: SidebarRow::Thread(s.clone()),
        indent: depth * 14.0,
        team_leader: has_kids,
        team_collapsed: team_collapsed.contains(&s.id),
    });
    if has_kids && !team_collapsed.contains(&s.id) && depth < MAX_TEAM_RENDER_DEPTH {
        let mut kids = members.get(s.id.as_str()).cloned().unwrap_or_default();
        kids.sort_by_key(|k| std::cmp::Reverse(k.interacted_at));
        for kid in kids {
            push_member(team_collapsed, out, kid, depth + 1.0, members);
        }
    }
}

/// Whether an external session projects into the loose Conversations list
/// instead of a folder group: unbound sessions, and sessions bound to a
/// path that is not a registered project (a removed folder, or a session
/// cwd never bound as a project). Mirrors the thread-side partitioning
/// rule in [`Sidebar::render`].
fn external_session_is_loose(project: Option<&std::path::Path>, known_projects: &[String]) -> bool {
    project.is_none_or(|p| {
        !known_projects
            .iter()
            .any(|kp| kp.as_str() == p.to_string_lossy().as_ref())
    })
}

pub struct Sidebar {
    store: StoreHandle,
    selected: Option<String>,
    /// The thread that was selected immediately before `selected`; its row
    /// plays a fade-out wash while the new row's wash fades in, so selection
    /// reads as the wash sliding from the old row to the new one.
    prev_selected: Option<String>,
    /// Bumped on every selection change. Embedded in each row's animation id
    /// so gpui treats it as a fresh animation and replays 0→1 (its element
    /// state is keyed by id).
    select_gen: u64,
    /// Project paths whose folder group is collapsed; absent means expanded.
    collapsed: HashSet<String>,
    /// Team leader ids whose member group is collapsed; absent means
    /// expanded. Mirrors `collapsed` but per leader row instead of folder.
    team_collapsed: HashSet<String>,
    /// list. Merged into the Conversations list by recency (an `external:` id
    /// in `selected` highlights the active one).
    external_sessions: Vec<crate::external_session::ExternalSessionSummary>,
    /// Whether the new-session `PopupMenu` (Manox / Claude Code / Codex /
    /// GitHub Copilot) is open.
    new_session_open: bool,
    new_session_menu: Option<Entity<PopupMenu>>,
    new_session_menu_sub: Option<Subscription>,
    /// The project path the menu was opened from. `None` when opened from
    /// the Conversations header; `Some` when opened from a project folder's
    /// ellipsis button. The menu closures read this to decide whether to
    /// emit `NewThread` vs `NewThreadWithProject`, to pass the project path
    /// as the CWD for external CLI sessions, and to identify the folder the
    /// Remove-project row unregisters.
    new_session_project: Option<PathBuf>,
    /// The single inline tag edit in flight (one row at a time); `None`
    /// when no row is editing its tag.
    tag_edit: Option<TagEdit>,
    /// The thread row whose three-dot overflow menu is open (one at a
    /// time); mirrors the new-session menu's open flag.
    row_menu_open: Option<String>,
    row_menu: Option<Entity<PopupMenu>>,
    row_menu_sub: Option<Subscription>,
    /// Live width driven by dragging the divider on the right edge. Updated
    /// from the owning `Workspace` on every drag-move tick.
    width: Pixels,
    /// Scroll offset + child-bounds reader for the sidebar scroll body; the
    /// sticky section-header overlay reads it to decide which header to show.
    scroll_handle: ScrollHandle,
    _pump: gpui::Task<()>,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Sidebar {
    pub fn new(width: Pixels, cx: &mut Context<Self>) -> Self {
        let store = manox_agent::thread_store_global();
        // The store is gpui-free: pump its event channel into `cx.notify()`
        // through a spawned task instead of `cx.subscribe` (transitional;
        // γ replaces this with a client store + entity subscription).
        let rx = store.subscribe();
        let _pump = cx.spawn(async move |this, cx: &mut gpui::AsyncApp| {
            while let Ok(_ev) = rx.recv().await {
                let _ = this.update(cx, |_, cx| {
                    // The event value is not needed — any store change
                    // repaints the list.
                    cx.notify();
                });
            }
        });
        Self {
            store,
            selected: None,
            collapsed: HashSet::new(),
            team_collapsed: HashSet::new(),
            prev_selected: None,
            select_gen: 0,
            external_sessions: Vec::new(),
            new_session_open: false,
            new_session_menu: None,
            new_session_menu_sub: None,
            new_session_project: None,
            tag_edit: None,
            row_menu_open: None,
            row_menu: None,
            row_menu_sub: None,
            width,
            scroll_handle: ScrollHandle::new(),
            _pump,
        }
    }

    pub fn store(&self) -> StoreHandle {
        self.store.clone()
    }

    /// Replace the external-session projection. Called by the Workspace
    /// whenever the canonical set changes (spawn / close). The sidebar never
    /// owns the live sessions — it only renders this snapshot.
    pub fn set_external_sessions(
        &mut self,
        sessions: Vec<crate::external_session::ExternalSessionSummary>,
        cx: &mut Context<Self>,
    ) {
        self.external_sessions = sessions;
        cx.notify();
    }

    /// Update the rendered width. Called by the owning `Workspace` on every
    /// divider drag-move tick; the new value takes effect on the next render.
    pub fn set_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        if self.width == width {
            return;
        }
        self.width = width;
        cx.notify();
    }

    /// The Conversations section header with its `+` new-session button.
    /// `id_prefix` disambiguates element ids when the header exists twice in
    /// one tree (in-flow copy + sticky overlay), and `dropdown` gates the
    /// deferred new-session menu so only the visible copy anchors it.
    fn conversations_section_header(
        &self,
        theme: &Theme,
        id_prefix: &str,
        dropdown: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        section_header(
            i18n::t("sidebar-section-conversations"),
            theme,
            Some(
                gpui::div()
                    .relative()
                    .child(
                        Button::new(format!("{id_prefix}-conv-plus"))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip(i18n::t("sidebar-new-chat"))
                            .on_click(cx.listener(|this, _ev, window, cx| {
                                if this.new_session_open {
                                    this.close_new_session_menu();
                                } else {
                                    this.open_new_session_menu(None, window, cx);
                                }
                                cx.notify();
                            })),
                    )
                    // Deferred inside this relative wrapper so it paints after
                    // sibling rows (z-order) while staying positioned just
                    // below the button (`top_full()` is 100% of this wrapper's
                    // height).
                    .children(
                        (dropdown && self.new_session_open && self.new_session_project.is_none())
                            .then(|| {
                                self.render_new_session_dropdown(
                                    format!("{id_prefix}-new-session-dropdown").into(),
                                )
                            })
                            .flatten(),
                    )
                    .into_any_element(),
            ),
        )
    }

    /// Open the session-menu popup. `project` is `None` when opened from the
    /// Conversations header — the flat new-session menu — and `Some(path)`
    /// when opened from a project folder's ellipsis button — the project
    /// action menu (new-session submenu / Terminal / VS Code / Remove
    /// Project). The path determines the CWD for external CLI sessions and
    /// whether the new Manox session binds to the project.
    fn open_new_session_menu(
        &mut self,
        project: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.new_session_project = project.clone();
        let theme = cx.theme().clone();
        let sidebar = cx.entity().downgrade();
        let menu = PopupMenu::build(window, cx, move |menu, window, cx| {
            if project.is_some() {
                return build_project_menu(menu, &sidebar, &theme, window, cx);
            }
            let mut menu = menu
                .max_w(gpui::px(280.))
                .label(i18n::t("sidebar-new-session-label"));
            let sidebar_new = sidebar.clone();
            menu = menu.item(
                PopupMenuItem::new(i18n::t("sidebar-new-session-manox"))
                    .icon(
                        Icon::default()
                            .path("icons/manox.svg")
                            .small()
                            .text_color(theme.muted_foreground),
                    )
                    .on_click(move |_, _window, cx| {
                        let _ = sidebar_new.update(cx, |this, cx| {
                            let project = this.new_session_project.take();
                            this.close_new_session_menu();
                            if let Some(p) = project {
                                cx.emit(SidebarEvent::NewThreadWithProject(p));
                            } else {
                                cx.emit(SidebarEvent::NewThread);
                            }
                            cx.notify();
                        });
                    }),
            );
            // Terminal: a plain shell session — no provider/model cascade,
            // the click spawns immediately in the workspace cwd. Shares the
            // project menu's `sidebar-new-terminal` label — both menus create
            // a fresh terminal.
            {
                let kind = crate::external_session::SessionKind::Terminal;
                let sidebar_plain = sidebar.clone();
                menu = menu.item(
                    PopupMenuItem::new(i18n::t("sidebar-new-terminal"))
                        .icon(
                            Icon::default()
                                .path(kind.icon_asset())
                                .small()
                                .text_color(theme.muted_foreground),
                        )
                        .on_click(move |_, _window, cx| {
                            let _ = sidebar_plain.update(cx, |this, cx| {
                                let project = this.new_session_project.take();
                                this.close_new_session_menu();
                                cx.emit(SidebarEvent::SpawnPlainSession(kind, project));
                                cx.notify();
                            });
                        }),
                );
            }
            for kind in [
                crate::external_session::SessionKind::ClaudeCode,
                crate::external_session::SessionKind::Codex,
                crate::external_session::SessionKind::GithubCopilot,
            ] {
                let sidebar = sidebar.clone();
                let theme = theme.clone();
                let label = kind.label();
                let agent_id = kind.agent_id();
                menu = menu.submenu_with_icon(
                    Some(
                        Icon::default()
                            .path(kind.icon_asset())
                            .small()
                            .text_color(theme.muted_foreground),
                    ),
                    label,
                    window,
                    cx,
                    move |submenu, window, cx| {
                        build_agent_model_cascade(submenu, kind, agent_id, &sidebar, window, cx)
                    },
                );
            }
            // VS Code: single entry — injection resolves from the persisted
            // `vscode_app:` settings (Settings → 外部工具 → Visual Studio
            // Code.app); no provider/model cascade. Launches the VS Code
            // desktop app opening the project directory the menu was opened
            // from. Disabled outright when VS Code is not installed (parity
            // with the 工具 → VS Code system menu).
            {
                let sidebar = sidebar.clone();
                let icon = Icon::default()
                    .path("icons/vscode.svg")
                    .small()
                    .text_color(theme.muted_foreground);
                menu = menu.item(
                    PopupMenuItem::new("VS Code")
                        .icon(icon)
                        .disabled(!manox_ext_agents::vscode_app_installed())
                        .on_click(move |_, _window, cx| {
                            let _ = sidebar.update(cx, |this, cx| {
                                let project = this.new_session_project.clone();
                                cx.emit(SidebarEvent::LaunchVSCode(project));
                                cx.notify();
                            });
                        }),
                );
            }
            menu
        });
        let sub = cx.subscribe(&menu, |this, _menu, _: &DismissEvent, cx| {
            this.close_new_session_menu();
            cx.notify();
        });
        self.new_session_open = true;
        self.new_session_menu = Some(menu);
        self.new_session_menu_sub = Some(sub);
    }

    fn close_new_session_menu(&mut self) {
        self.new_session_open = false;
        self.new_session_menu = None;
        self.new_session_menu_sub = None;
        self.new_session_project = None;
    }

    /// Open a thread row's three-dot overflow menu (Archive + Tag), anchored
    /// below its trigger button. One row menu at a time — opening one closes
    /// the previous.
    fn open_row_menu(
        &mut self,
        id: String,
        archived: bool,
        has_tag: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_row_menu();
        let theme = cx.theme().clone();
        let sidebar = cx.entity().downgrade();
        let open_id = id.clone();
        let menu = PopupMenu::build(window, cx, move |menu, _window, _cx| {
            let archive_label = if archived {
                i18n::t("sidebar-unarchive")
            } else {
                i18n::t("sidebar-archive")
            };
            let sidebar_archive = sidebar.clone();
            let id_archive = id.clone();
            let menu = menu.item(
                PopupMenuItem::new(archive_label)
                    .icon(
                        Icon::new(IconName::Inbox)
                            .small()
                            .text_color(theme.muted_foreground),
                    )
                    .on_click(move |_, _, cx| {
                        let _ = sidebar_archive.update(cx, |this, cx| {
                            this.close_row_menu();
                            cx.emit(SidebarEvent::ArchiveThread(id_archive.clone(), !archived));
                            cx.notify();
                        });
                    }),
            );
            let tag_label = if has_tag {
                i18n::t("sidebar-thread-tag-rename")
            } else {
                i18n::t("sidebar-thread-tag-add")
            };
            let mode = if has_tag {
                TagEditMode::Renaming
            } else {
                TagEditMode::Adding
            };
            let sidebar_tag = sidebar.clone();
            let id_tag = id.clone();
            menu.item(
                PopupMenuItem::new(tag_label)
                    .icon(
                        Icon::default()
                            .path("icons/tag.svg")
                            .small()
                            .text_color(theme.muted_foreground),
                    )
                    .on_click(move |_, window, cx| {
                        let _ = sidebar_tag.update(cx, |this, cx| {
                            this.close_row_menu();
                            this.begin_tag_edit(id_tag.clone(), mode, window, cx);
                        });
                    }),
            )
        });
        let sub = cx.subscribe(&menu, |this, _menu, _: &DismissEvent, cx| {
            this.close_row_menu();
            cx.notify();
        });
        self.row_menu_open = Some(open_id);
        self.row_menu = Some(menu);
        self.row_menu_sub = Some(sub);
    }

    fn close_row_menu(&mut self) {
        self.row_menu_open = None;
        self.row_menu = None;
        self.row_menu_sub = None;
    }

    /// Anchor the open row menu below its trigger button. Deferred so it
    /// paints above sibling rows and escapes the row's `overflow_hidden`;
    /// `top_full()` + `right_0()` hang it just under the button's wrapper.
    fn render_row_menu_dropdown(&self, id: &str) -> Option<AnyElement> {
        if self.row_menu_open.as_deref() != Some(id) {
            return None;
        }
        let menu = self.row_menu.clone()?;
        Some(
            deferred(
                gpui::div()
                    .id(SharedString::from(format!("thread-menu-dropdown-{id}")))
                    .absolute()
                    .top_full()
                    .right_0()
                    .occlude()
                    .child(menu),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    /// Mount the inline tag input on a row — at most one edit in flight;
    /// starting a new one replaces the previous. Rename mode prefills the
    /// current tag. The input is focused immediately and clamped to
    /// [`MAX_THREAD_TAG_CHARS`] on every edit.
    fn begin_tag_edit(
        &mut self,
        id: String,
        mode: TagEditMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let prefill = (mode == TagEditMode::Renaming)
            .then(|| self.summary_tag(&id, cx))
            .flatten();
        let placeholder = i18n::t("sidebar-thread-tag-placeholder");
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder(placeholder);
            if let Some(value) = prefill {
                state.set_value(value, window, cx);
            }
            state
        });
        let sub = cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| match event {
                // The pinned gpui-component input has no max-length
                // support; clamp every edit down to the tag ceiling.
                InputEvent::Change => {
                    input.update(cx, |state, cx| {
                        let value = state.value();
                        if value.chars().count() > MAX_THREAD_TAG_CHARS {
                            let truncated: String =
                                value.chars().take(MAX_THREAD_TAG_CHARS).collect();
                            state.set_value(truncated, window, cx);
                        }
                    });
                }
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    this.commit_tag_edit(cx);
                }
                _ => {}
            },
        );
        self.tag_edit = Some(TagEdit {
            id,
            input: input.clone(),
            _sub: sub,
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// Commit the in-flight tag edit: a non-empty value emits
    /// `SetThreadTag`, an empty one discards silently. Either way the input
    /// unmounts. Idempotent — a blur may race in after an Enter commit.
    fn commit_tag_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.tag_edit.take() else {
            return;
        };
        let value = edit.input.read(cx).value().trim().to_string();
        if !value.is_empty() {
            cx.emit(SidebarEvent::SetThreadTag(edit.id, Some(value)));
        }
        cx.notify();
    }

    fn cancel_tag_edit(&mut self, cx: &mut Context<Self>) {
        if self.tag_edit.take().is_some() {
            cx.notify();
        }
    }

    /// The persisted tag for a row, reading both sidebar partitions.
    fn summary_tag(&self, id: &str, _cx: &App) -> Option<String> {
        self.store.read(|store| {
            store
                .summaries()
                .iter()
                .chain(store.archived_summaries())
                .find(|s| s.id == id)
                .and_then(|s| s.tag.clone())
        })
    }

    /// Build the session-menu dropdown anchored below the trigger button
    /// that opened it. Deferred so it paints above sibling rows; `top_full()` is
    /// 100% of the wrapping `.relative()` div, so the menu sits just under the
    /// button rather than at the sidebar's bottom edge.
    fn render_new_session_dropdown(&self, id: SharedString) -> Option<AnyElement> {
        self.new_session_menu.clone().map(|menu| {
            deferred(
                gpui::div()
                    .id(id)
                    .absolute()
                    .top_full()
                    .right_0()
                    .occlude()
                    .child(menu),
            )
            .with_priority(1)
            .into_any_element()
        })
    }

    /// Mark the currently selected thread id (back-filled by Workspace on switch/new, for highlight).
    pub fn set_selected(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        if self.selected == id {
            return;
        }
        self.prev_selected = self.selected.take();
        self.selected = id;
        self.select_gen = self.select_gen.wrapping_add(1);
        cx.notify();
    }

    /// Order threads as a team forest: top-level rows (threads + externals)
    /// merged by recency, each leader followed by its indented member
    /// subtree. Orphans and cycles stay top-level (`depth` is zeroed for
    /// them at the store); a collapsed leader hides its subtree.
    fn order_rows(
        &self,
        threads: &[manox_agent::ThreadSummary],
        externals: &[crate::external_session::ExternalSessionSummary],
    ) -> Vec<ThreadRender> {
        team_forest(&self.team_collapsed, threads, externals)
    }

    /// A collapsible project folder: a clickable header (chevron + folder icon +
    /// basename) over its indented conversation rows when expanded. The
    /// trailing ellipsis button opens the project action menu with the
    /// project path so the workspace can set the CWD for external CLI
    /// sessions.
    fn render_project_group(
        &self,
        path: &str,
        group: &[manox_agent::ThreadSummary],
        selected: Option<&str>,
        store: &StoreHandle,
        slide: &SlideCtx,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let expanded = !self.collapsed.contains(path);
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(path)
            .to_string();
        let key: SharedString = path.to_string().into();
        // External sessions bound to this project folder — pulled from the
        // sidebar's projection rather than threaded through as an arg, so the
        // signature stays under clippy's argument limit.
        let externals: Vec<crate::external_session::ExternalSessionSummary> = self
            .external_sessions
            .iter()
            .filter(|s| s.project.as_deref() == Some(std::path::Path::new(path)))
            .cloned()
            .collect();

        let header = h_flex()
            .id(key.clone())
            .w_full()
            .px_2()
            .py_1p5()
            .gap_1()
            .items_center()
            .rounded(theme.radius)
            .hover(|s| s.bg(theme.accent.opacity(0.08)))
            .child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .xsmall()
                .text_color(theme.muted_foreground),
            )
            .child(
                Icon::new(IconName::Folder)
                    .small()
                    .text_color(theme.muted_foreground),
            )
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(name),
            )
            .child(
                gpui::div()
                    .relative()
                    .child(
                        Button::new(format!("project-menu-{key}"))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Ellipsis)
                            .tooltip(i18n::t("sidebar-project-menu"))
                            .on_click(cx.listener({
                                let path = path.to_string();
                                move |this, _ev, window, cx| {
                                    cx.stop_propagation();
                                    if this.new_session_open {
                                        this.close_new_session_menu();
                                    } else {
                                        this.open_new_session_menu(
                                            Some(PathBuf::from(path.clone())),
                                            window,
                                            cx,
                                        );
                                    }
                                    cx.notify();
                                }
                            })),
                    )
                    // Only render the dropdown here when the menu was opened
                    // from *this* project folder's ellipsis button, so the
                    // menu anchors below the clicked button instead of the
                    // Conversations header's `+`.
                    .children(
                        (self.new_session_open
                            && self.new_session_project.as_deref()
                                == Some(std::path::Path::new(path)))
                        .then(|| {
                            self.render_new_session_dropdown(
                                format!("new-session-dropdown-{path}").into(),
                            )
                        })
                        .flatten(),
                    ),
            )
            .on_click(cx.listener({
                let path = path.to_string();
                move |this, _ev, _window, cx| {
                    if !this.collapsed.remove(&path) {
                        this.collapsed.insert(path.clone());
                    }
                    cx.notify();
                }
            }));

        let rows = expanded.then(|| {
            // Threads + this project's external sessions, merged by recency so
            // an external CLI session launched from the folder's menu sits
            // among the folder's manox threads instead of in the loose list;
            // team members nest indented under their leader.
            let team_rows = self.order_rows(group, &externals);
            v_flex()
                .w_full()
                .gap_0p5()
                .children(team_rows.into_iter().map(|tr| {
                    let is_selected = selected == Some(tr.row.id());
                    match tr.row {
                        SidebarRow::Thread(s) => {
                            let live = store.read(|store| ThreadLiveState {
                                running: store.is_running(&s.id),
                                pending_auth: store.pending_auth_contains(&s.id),
                                pending_plan: store.pending_plan_contains(&s.id),
                                background_work: store.background_work_contains(&s.id),
                            });
                            render_thread_item(
                                &SidebarThreadItem::from_thread(
                                    &s,
                                    is_selected,
                                    live,
                                    RowNesting {
                                        indent: px(16. + tr.indent),
                                        team_leader: tr.team_leader,
                                        team_collapsed: tr.team_collapsed,
                                        nested: tr.indent > 0.0,
                                    },
                                    &theme,
                                ),
                                slide,
                                self,
                                &theme,
                                cx,
                            )
                        }
                        SidebarRow::External(s) => render_thread_item(
                            &SidebarThreadItem::from_external(&s, is_selected, px(16.), &theme),
                            slide,
                            self,
                            &theme,
                            cx,
                        ),
                    }
                }))
        });

        v_flex()
            .w_full()
            .gap_0p5()
            .child(header)
            .children(rows)
            .into_any_element()
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let summaries = self.store.read(|s| s.summaries().to_vec());
        let known_projects = self.store.read(|s| s.known_projects().to_vec());
        let selected = self.selected.clone();
        let store = self.store.clone();

        let mut projects: Vec<(String, Vec<manox_agent::ThreadSummary>)> = Vec::new();
        let mut loose: Vec<manox_agent::ThreadSummary> = Vec::new();
        for s in &summaries {
            // Only REGISTERED projects become folder groups; a session cwd
            // that was never bound as a project (e.g. the default home dir)
            // stays in the loose Conversations list.
            if s.project.is_empty() || !known_projects.contains(&s.project) {
                loose.push(s.clone());
            } else if let Some(entry) = projects.iter_mut().find(|(p, _)| *p == s.project) {
                entry.1.push(s.clone());
            } else {
                projects.push((s.project.clone(), vec![s.clone()]));
            }
        }
        // Merge registered projects that have no active threads — they still
        // appear as empty folders so the user can start a new conversation
        // in the project without losing the folder reference.
        for kp in &known_projects {
            if !projects.iter().any(|(p, _)| p == kp) {
                projects.push((kp.clone(), Vec::new()));
            }
        }
        // External sessions not bound to a *registered* project stay in the
        // loose Conversations list — that covers unbound sessions, sessions
        // whose folder was removed, and sessions bound to a cwd never
        // registered as a project. Bound ones are pulled into their folder
        // group inside `render_project_group` (filtered by project path
        // there), so a removed folder never strands its live sessions.
        let loose_externals: Vec<crate::external_session::ExternalSessionSummary> = self
            .external_sessions
            .iter()
            .filter(|s| external_session_is_loose(s.project.as_deref(), &known_projects))
            .cloned()
            .collect();

        let mut flat_ids: Vec<String> = Vec::new();
        for (path, group) in &projects {
            if !self.collapsed.contains(path.as_str()) {
                flat_ids.extend(group.iter().map(|s| s.id.clone()));
                // Include external sessions bound to this project folder so
                // they participate in the selection-slide alongside the
                // folder's threads (merged + recency-sorted at render time).
                flat_ids.extend(
                    self.external_sessions
                        .iter()
                        .filter(|s| s.project.as_deref() == Some(std::path::Path::new(path)))
                        .map(|s| s.id.clone()),
                );
            }
        }
        flat_ids.extend(loose.iter().map(|s| s.id.clone()));
        flat_ids.extend(loose_externals.iter().map(|s| s.id.clone()));
        let dir = match (&self.prev_selected, &self.selected) {
            (Some(prev), Some(cur)) => match (
                flat_ids.iter().position(|id| id == prev),
                flat_ids.iter().position(|id| id == cur),
            ) {
                (Some(p), Some(c)) => match c.cmp(&p) {
                    std::cmp::Ordering::Greater => SlideDir::Down,
                    std::cmp::Ordering::Less => SlideDir::Up,
                    std::cmp::Ordering::Equal => SlideDir::None,
                },
                _ => SlideDir::None,
            },
            _ => SlideDir::None,
        };
        let slide = SlideCtx {
            selecting_id: self.selected.clone(),
            deselecting_id: self.prev_selected.clone(),
            dir,
            gen_id: self.select_gen,
        };

        let top_inset = if cfg!(target_os = "macos") {
            px(28.)
        } else {
            px(8.)
        };

        // Sticky overlay pinned above the scroll body: while the current
        // section's header would scroll out of view, an overlay copy takes its
        // place at the header's resting position and the rows scroll
        // underneath. The in-flow headers stay in the content (parent-child
        // grouping — each header sits directly above its own section), and
        // the overlay only appears after the first scroll, by which point the
        // scroll handle's offset/bounds have been measured.
        let projects_present = !projects.is_empty();
        let scroll_top = -self.scroll_handle.offset().y;
        let projects_height = self.scroll_handle.bounds_for_item(0).map(|b| b.size.height);
        let pinned = pinned_section(projects_present, scroll_top, projects_height);
        let overlay_visible = scroll_top > px(0.);
        // When the overlay carries the Conversations header it also anchors
        // the new-session dropdown; the in-flow copy must then drop its own
        // copy to avoid two menus.
        let overlay_shows_conversations = overlay_visible && pinned == PinnedSection::Conversations;
        let overlay: Option<AnyElement> = overlay_visible.then(|| {
            let header = match pinned {
                PinnedSection::Projects => {
                    section_header(i18n::t("sidebar-section-projects"), &theme, None)
                }
                PinnedSection::Conversations => {
                    self.conversations_section_header(&theme, "sticky", true, cx)
                }
            };
            gpui::div()
                .id("sidebar-sticky-header")
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .px_2()
                .pt(top_inset)
                .bg(theme.background)
                .border_b_1()
                .border_color(theme.border)
                .child(header)
                .into_any_element()
        });
        let projects_el: Vec<AnyElement> = projects
            .into_iter()
            .map(|(path, group)| {
                self.render_project_group(&path, &group, selected.as_deref(), &store, &slide, cx)
            })
            .collect();

        v_flex()
            .h_full()
            .w(self.width)
            .bg(theme.background)
            .border_r_1()
            .border_color(theme.border)
            .relative()
            .child(
                v_flex()
                    .id("sidebar-body")
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .pt(top_inset)
                    .pb_2()
                    .track_scroll(&self.scroll_handle)
                    // child 0: the Projects section — its header sits directly
                    // above the folder groups (parent-child grouping),
                    // rendered even when no registered projects exist (a
                    // zero-height slot) so `bounds_for_item(0)` keeps
                    // measuring the projects↔conversations boundary for the
                    // sticky overlay.
                    .child(
                        v_flex()
                            .w_full()
                            .children(projects_present.then(|| {
                                section_header(i18n::t("sidebar-section-projects"), &theme, None)
                            }))
                            .children(projects_el),
                    )
                    // child 1: the Conversations section — header + loose
                    // (non-project) threads and external sessions, merged by
                    // recency so an external CLI session sits among manox
                    // threads instead of a separate section. External rows
                    // sort by spawn time (manox cannot observe in-TUI
                    // interaction); threads by last interaction.
                    .child(
                        v_flex()
                            .w_full()
                            .child(self.conversations_section_header(
                                &theme,
                                "inflow",
                                !overlay_shows_conversations,
                                cx,
                            ))
                            .child({
                                let team_rows = self.order_rows(&loose, &loose_externals);
                                v_flex()
                                    .w_full()
                                    .gap_0p5()
                                    .children(team_rows.into_iter().map(|tr| {
                                        let is_selected = selected.as_deref() == Some(tr.row.id());
                                        match tr.row {
                                            SidebarRow::Thread(s) => {
                                                let live = store.read(|store| ThreadLiveState {
                                                    running: store.is_running(&s.id),
                                                    pending_auth: store
                                                        .pending_auth_contains(&s.id),
                                                    pending_plan: store
                                                        .pending_plan_contains(&s.id),
                                                    background_work: store
                                                        .background_work_contains(&s.id),
                                                });
                                                render_thread_item(
                                                    &SidebarThreadItem::from_thread(
                                                        &s,
                                                        is_selected,
                                                        live,
                                                        RowNesting {
                                                            indent: px(tr.indent),
                                                            team_leader: tr.team_leader,
                                                            team_collapsed: tr.team_collapsed,
                                                            nested: tr.indent > 0.0,
                                                        },
                                                        &theme,
                                                    ),
                                                    &slide,
                                                    self,
                                                    &theme,
                                                    cx,
                                                )
                                            }
                                            SidebarRow::External(s) => render_thread_item(
                                                &SidebarThreadItem::from_external(
                                                    &s,
                                                    is_selected,
                                                    px(0.),
                                                    &theme,
                                                ),
                                                &slide,
                                                self,
                                                &theme,
                                                cx,
                                            ),
                                        }
                                    }))
                            }),
                    ),
            )
            .children(overlay)
    }
}

fn section_header(label: SharedString, theme: &Theme, action: Option<AnyElement>) -> AnyElement {
    let mut row = h_flex()
        .px_2()
        .pt_3()
        .pb_1()
        .items_center()
        .text_sm()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.muted_foreground)
        .child(gpui::div().flex_1().child(label));

    if let Some(el) = action {
        row = row.child(el);
    }

    row.into_any_element()
}

/// Cascade for the external-agent CLI submenus (Claude Code / Codex / GitHub
/// Copilot) in the new-session menu: builds the shared provider→model cascade
/// and emits `SpawnExternalSession(kind, provider, model, wire, project)` on
/// pick — the workspace spawns the agent CLI in the project's directory and
/// mounts its TUI in the main area. The project path (if any) is read from
/// the sidebar's `new_session_project` field so the handler can set the CWD /
/// open directory.
fn build_agent_model_cascade(
    menu: PopupMenu,
    kind: crate::external_session::SessionKind,
    agent_id: &'static str,
    sidebar: &WeakEntity<Sidebar>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let sidebar = sidebar.clone();
    crate::views::model_cascade::build_model_cascade(
        menu,
        agent_id,
        window,
        cx,
        move |provider, model, wire, _window, cx| {
            let _ = sidebar.update(cx, |this, cx| {
                let project = this.new_session_project.clone();
                cx.emit(SidebarEvent::SpawnExternalSession(
                    kind, provider, model, wire, project,
                ));
                cx.notify();
            });
        },
    )
}

/// A project folder's action menu (its ellipsis button): a new-session
/// submenu (Manox + the external-agent model cascades), a plain terminal,
/// VS Code, and a destructive Remove-project row. The session rows share
/// the flat new-session menu's semantics, scoped to the sidebar's
/// `new_session_project`.
fn build_project_menu(
    menu: PopupMenu,
    sidebar: &WeakEntity<Sidebar>,
    theme: &Theme,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let theme = theme.clone();
    let sidebar = sidebar.clone();
    let mut menu = menu.max_w(gpui::px(280.));
    // New-session submenu: Manox flat row + one provider→model cascade per
    // external agent kind.
    let sidebar_new = sidebar.clone();
    let theme_new = theme.clone();
    menu = menu.submenu_with_icon(
        Some(
            Icon::new(IconName::Plus)
                .small()
                .text_color(theme.muted_foreground),
        ),
        i18n::t("sidebar-new-session-label"),
        window,
        cx,
        move |submenu, window, cx| {
            let mut submenu = submenu;
            let sidebar_manox = sidebar_new.clone();
            submenu = submenu.item(
                PopupMenuItem::new(i18n::t("sidebar-new-session-manox"))
                    .icon(
                        Icon::default()
                            .path("icons/manox.svg")
                            .small()
                            .text_color(theme_new.muted_foreground),
                    )
                    .on_click(move |_, _window, cx| {
                        let _ = sidebar_manox.update(cx, |this, cx| {
                            let project = this.new_session_project.take();
                            this.close_new_session_menu();
                            if let Some(p) = project {
                                cx.emit(SidebarEvent::NewThreadWithProject(p));
                            } else {
                                cx.emit(SidebarEvent::NewThread);
                            }
                            cx.notify();
                        });
                    }),
            );
            for kind in [
                crate::external_session::SessionKind::ClaudeCode,
                crate::external_session::SessionKind::Codex,
                crate::external_session::SessionKind::GithubCopilot,
            ] {
                let sidebar_agent = sidebar_new.clone();
                let label = kind.label();
                let agent_id = kind.agent_id();
                submenu = submenu.submenu_with_icon(
                    Some(
                        Icon::default()
                            .path(kind.icon_asset())
                            .small()
                            .text_color(theme_new.muted_foreground),
                    ),
                    label,
                    window,
                    cx,
                    move |submenu, window, cx| {
                        build_agent_model_cascade(
                            submenu,
                            kind,
                            agent_id,
                            &sidebar_agent,
                            window,
                            cx,
                        )
                    },
                );
            }
            submenu
        },
    );
    // Terminal: a plain shell session — no provider/model cascade, the
    // click spawns immediately in the project directory.
    {
        let kind = crate::external_session::SessionKind::Terminal;
        let sidebar_plain = sidebar.clone();
        menu = menu.item(
            PopupMenuItem::new(i18n::t("sidebar-new-terminal"))
                .icon(
                    Icon::default()
                        .path(kind.icon_asset())
                        .small()
                        .text_color(theme.muted_foreground),
                )
                .on_click(move |_, _window, cx| {
                    let _ = sidebar_plain.update(cx, |this, cx| {
                        let project = this.new_session_project.take();
                        this.close_new_session_menu();
                        cx.emit(SidebarEvent::SpawnPlainSession(kind, project));
                        cx.notify();
                    });
                }),
        );
    }
    // VS Code: single entry — injection resolves from the persisted
    // `vscode_app:` settings; no provider/model cascade. Disabled outright
    // when VS Code is not installed (parity with the flat menu).
    {
        let sidebar_vscode = sidebar.clone();
        let icon = Icon::default()
            .path("icons/vscode.svg")
            .small()
            .text_color(theme.muted_foreground);
        menu = menu.item(
            PopupMenuItem::new("VS Code")
                .icon(icon)
                .disabled(!manox_ext_agents::vscode_app_installed())
                .on_click(move |_, _window, cx| {
                    let _ = sidebar_vscode.update(cx, |this, cx| {
                        let project = this.new_session_project.clone();
                        cx.emit(SidebarEvent::LaunchVSCode(project));
                        cx.notify();
                    });
                }),
        );
    }
    menu = menu.separator();
    // Remove project: unregister the folder; the threads fall back to the
    // loose Conversations list and the history stays on disk.
    let sidebar_rm = sidebar.clone();
    menu = menu.item(
        PopupMenuItem::new(i18n::t("sidebar-remove-project"))
            .icon(
                Icon::default()
                    .path("icons/trash-2.svg")
                    .small()
                    .text_color(theme.danger),
            )
            .on_click(move |_, _window, cx| {
                let _ = sidebar_rm.update(cx, |this, cx| {
                    let project = this.new_session_project.take();
                    this.close_new_session_menu();
                    if let Some(p) = project {
                        cx.emit(SidebarEvent::RemoveProject(p));
                    }
                    cx.notify();
                });
            }),
    );
    menu
}

/// Leading icon for a unified sidebar row. Manox threads carry the brand
/// mark; external agent sessions use their own brand SVG (resolved by
/// `ExtrasAssetSource`, tinted via `text_color`).
#[derive(Clone)]
enum RowIcon {
    Thread,
    External(&'static str),
}

/// What the row's trailing action emits — a thread row's overflow menu
/// toggles its archived flag, an external session's hover button tears it
/// down (kill + drop).
#[derive(Clone)]
enum RowKind {
    Thread { archived: bool },
    External,
}

/// Maximum length of a user thread tag (enforced on every input edit).
const MAX_THREAD_TAG_CHARS: usize = 10;

/// Which flavor of inline tag edit a row is running.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TagEditMode {
    /// The row has no tag yet; committing stores the typed value.
    Adding,
    /// The row's existing tag is being replaced; the input is prefilled.
    Renaming,
}

/// The one inline tag edit in flight (at most one row edits at a time).
/// The add/rename mode is consumed at mount time (prefill decision) and
/// never needed again.
struct TagEdit {
    id: String,
    input: Entity<InputState>,
    /// Input-event subscription (length clamp + commit); dropped with the
    /// edit so a stale input can never fire into the sidebar.
    _sub: Subscription,
}

/// Live per-thread state the store tracks for the row's icon: the spin /
/// blue-static / triangle decision is purely a function of these four flags.
#[derive(Clone, Copy, Default)]
struct ThreadLiveState {
    running: bool,
    pending_auth: bool,
    pending_plan: bool,
    background_work: bool,
}

/// A UI-layer sidebar row projected from either a manox `ThreadSummary` or an
/// `ExternalSessionSummary`, so the two render through one layout with a shared
/// selection-slide animation, id tag, and hover archive action. Only display +
/// identity fields live here — the sidebar never holds PTY handles.
#[derive(Clone)]
struct SidebarThreadItem {
    id: String,
    /// 8-char tag label. Threads use the thread UUID prefix; external sessions
    /// use the cx session id prefix (traceable to `~/.manox/sessions/<id>.sock`).
    short_id: String,
    /// Clipboard payload for the id-tag click. Threads copy the thread id;
    /// external sessions copy the full cx session id (or socket path).
    copy_value: String,
    title: String,
    updated: String,
    pinned: bool,
    /// User-assigned tag rendered as a chip beside the id tag; `None` when
    /// the thread carries no tag. Threads only — external rows never tag.
    tag: Option<String>,
    has_unread: bool,
    errored: bool,
    selected: bool,
    indent: gpui::Pixels,
    /// A team leader whose member rows nest under this row; renders a
    /// collapse chevron before the status icon.
    team_leader: bool,
    /// The leader's member group is folded; the chevron shows the fold.
    team_collapsed: bool,
    /// Nested row (team member or fork child): draws the left guide rail
    /// tying the row to its parent.
    nested: bool,
    icon: RowIcon,
    /// A plan-review verdict is due; the icon stays blue static, not spinning.
    pending_plan: bool,
    /// Live monitors / background bash: the loop can still self-advance even
    /// with no turn in flight, so the icon keeps spinning.
    background_work: bool,
    /// A tool authorization is parked waiting for the user's verdict (the
    /// thread's card is only visible when it is the active thread).
    pending_auth: bool,
    running: bool,
    /// A sidecar-restored row with no live process; clicking it resumes the
    /// CLI. Rendered dimmed with a resume hover action.
    resumable: bool,
    /// A resume is in flight for this row; rendered with a loading indicator.
    resuming: bool,
    /// Selection-wash color: threads tint by approval mode, external rows use
    /// the theme accent.
    wash: gpui::Hsla,
    kind: RowKind,
}

impl SidebarThreadItem {
    fn from_thread(
        summary: &manox_agent::ThreadSummary,
        selected: bool,
        live: ThreadLiveState,
        nesting: RowNesting,
        theme: &Theme,
    ) -> Self {
        let display = summary.display_title();
        let title = if display.is_empty() {
            i18n::t("sidebar-empty-summary").to_string()
        } else {
            truncate(display, 24)
        };
        Self {
            short_id: summary.id.chars().take(8).collect(),
            copy_value: summary.id.clone(),
            id: summary.id.clone(),
            title,
            updated: format_relative(summary.interacted_at),
            pinned: summary.pinned,
            tag: summary.tag.clone(),
            has_unread: summary.has_unread,
            errored: summary.errored,
            running: live.running,
            pending_auth: live.pending_auth,
            pending_plan: live.pending_plan,
            background_work: live.background_work,
            resumable: false,
            resuming: false,
            selected,
            indent: nesting.indent,
            team_leader: nesting.team_leader,
            team_collapsed: nesting.team_collapsed,
            nested: nesting.nested,
            icon: RowIcon::Thread,
            wash: approval_mode_color(summary.approval_mode, theme),
            kind: RowKind::Thread {
                archived: summary.archived,
            },
        }
    }

    fn from_external(
        summary: &crate::external_session::ExternalSessionSummary,
        selected: bool,
        indent: gpui::Pixels,
        theme: &Theme,
    ) -> Self {
        let display = summary.display_title();
        let title = if display.is_empty() {
            i18n::t("sidebar-empty-summary").to_string()
        } else {
            truncate(&display, 24)
        };
        // Tag the row with the cx session id prefix (tracks the .sock file);
        // fall back to the manox-internal id prefix when the cx id was not
        // recoverable (IPC bind failed).
        let short_id: String = if !summary.cx_session_id.is_empty() {
            summary.cx_session_id.chars().take(8).collect()
        } else {
            // No cx id — plain PTY sessions never have one, and an agent
            // session lands here when its IPC bind failed. Show the trailing
            // uuid segment of the manox-internal id; the `external:` prefix
            // would render the same "external" tag on every such row.
            summary
                .id
                .rsplit(':')
                .next()
                .unwrap_or(summary.id.as_str())
                .chars()
                .take(8)
                .collect()
        };
        Self {
            id: summary.id.clone(),
            short_id,
            copy_value: summary.copy_identity(),
            title,
            updated: format_relative(summary.created_at),
            pinned: false,
            has_unread: false,
            tag: None,
            errored: false,
            running: false,
            pending_auth: false,
            pending_plan: false,
            background_work: false,
            resumable: summary.resumable,
            resuming: summary.resuming,
            selected,
            indent,
            // External CLI sessions never lead a team; no collapse affordance.
            team_leader: false,
            team_collapsed: false,
            nested: false,
            icon: RowIcon::External(summary.kind.icon_asset()),
            // Same wash as Workspace Write threads: `theme.accent` resolves to
            // `neutral-100` (near-white) in the forced Light theme, which made
            // the external row's hover/active/selected wash invisible on the
            // white sidebar. `theme.info` (cyan) is the visible default tint.
            wash: theme.info,
            kind: RowKind::External,
        }
    }
}

/// Render one unified sidebar row — threads and external agent sessions
/// share the layout (leading icon → title → id tag + tag chip + updated
/// time + trailing action), the selection-wash slide animation, and the
/// hover-reveal affordance. Only the emitted event, the trailing action
/// (overflow menu vs close button), and the leading icon differ by `kind`.
/// `sidebar` carries the per-sidebar overlay state (tag edit, open row
/// menu) — `Context<Sidebar>` derefs to `App`, not the entity.
fn render_thread_item(
    item: &SidebarThreadItem,
    slide: &SlideCtx,
    sidebar: &Sidebar,
    theme: &Theme,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    let id = item.id.clone();
    let id_open = id.clone();
    let id_archive = id.clone();
    let id_team = id.clone();
    let id_copy = item.copy_value.clone();
    let short_id = item.short_id.clone();
    let title = item.title.clone();
    let updated = item.updated.clone();
    let title_color = if item.resumable {
        // A sidecar-restored row reads as parked: dimmed until resumed.
        theme.muted_foreground
    } else if item.errored {
        theme.danger
    } else {
        theme.foreground
    };
    let wash = item.wash;
    let icon = item.icon.clone();
    let open_kind = item.kind.clone();
    let group = gpui::SharedString::from(format!("thread-row-{id}"));
    // The loop can still self-advance (turn in flight, monitors / background
    // bash alive): the row spins and the id tag stays highlighted.
    let autonomous = item.running || item.background_work;
    let tag_variant = if item.selected || autonomous {
        TagVariant::Primary
    } else {
        TagVariant::Secondary
    };
    let role = if slide.selecting_id.as_deref() == Some(id.as_str()) {
        AnimRole::Selecting
    } else if slide.deselecting_id.as_deref() == Some(id.as_str()) {
        AnimRole::Deselecting
    } else {
        AnimRole::None
    };
    let dir_sign: f32 = match slide.dir {
        SlideDir::Down => 1.0,
        SlideDir::Up => -1.0,
        SlideDir::None => 0.0,
    };
    let slide_gen = slide.gen_id;
    let wash_overlay: Option<AnyElement> = if role != AnimRole::None {
        let anim_role = role;
        let anim_id = format!("thread-sel-wash-{id}-{slide_gen}");
        Some(
            gpui::div()
                .absolute()
                .left_0()
                .right_0()
                .rounded(theme.radius)
                .with_animation(
                    anim_id,
                    Animation::new(Duration::from_millis(160)).with_easing(ease_in_out),
                    move |el, t| {
                        let (opacity, ty) = if anim_role == AnimRole::Selecting {
                            (t, -dir_sign * SELECT_SLIDE_PX * (1.0 - t))
                        } else {
                            (1.0 - t, dir_sign * SELECT_SLIDE_PX * t)
                        };
                        el.bg(wash.opacity(0.18 * opacity))
                            .top(px(ty))
                            .bottom(px(-ty))
                    },
                )
                .into_any_element(),
        )
    } else {
        None
    };

    let tag_button = Button::new(format!("thread-id-tag-{id}"))
        .ghost()
        .xsmall()
        .compact()
        .tooltip(i18n::t("sidebar-copy-thread-id"))
        .cursor_pointer()
        .child(
            Tag::new()
                .with_variant(tag_variant)
                .outline()
                .small()
                .child(short_id),
        )
        .on_click(move |_ev, _window, cx: &mut App| {
            cx.stop_propagation();
            cx.write_to_clipboard(ClipboardItem::new_string(id_copy.clone()));
        });

    let tag_wrapper = gpui::div().relative().overflow_hidden().child(tag_button);
    let tag_element: AnyElement = if autonomous {
        let accent = theme.accent;
        tag_wrapper
            .with_animation(
                format!("thread-running-shimmer-{id}"),
                Animation::new(Duration::from_millis(1400))
                    .repeat()
                    .with_easing(ease_in_out),
                move |el, delta| {
                    el.child(
                        gpui::div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .w(px(12.))
                            .bg(accent.opacity(0.55))
                            .left(px(-20. + 120. * delta)),
                    )
                },
            )
            .into_any_element()
    } else {
        tag_wrapper.into_any_element()
    };

    let leading_icon = if item.errored {
        // Abnormal stop: the danger triangle replaces the ship-wheel.
        Icon::new(IconName::TriangleAlert)
            .xsmall()
            .text_color(theme.danger)
            .into_any_element()
    } else if item.resuming {
        // A resume is in flight: the loading indicator replaces the idle icon.
        crate::views::braille_spinner::BrailleSpinner::new()
            .xsmall()
            .color(theme.muted_foreground)
            .into_any_element()
    } else {
        match icon {
            RowIcon::Thread => {
                // ship-wheel state machine: green and spinning while the loop
                // can self-advance, blue static while it waits on the user
                // (approval / plan verdict / unread finished turn), gray for a
                // read pause or a never-run thread.
                let waiting_user = item.pending_auth || item.pending_plan;
                let (color, spin) = if waiting_user {
                    (theme.info, false)
                } else if autonomous {
                    (theme.success, true)
                } else if item.has_unread {
                    (theme.info, false)
                } else {
                    (theme.foreground, false)
                };
                let icon = gpui::svg()
                    .path("icons/ship-wheel.svg")
                    .size(px(16.))
                    .text_color(color);
                if spin {
                    icon.with_animation(
                        format!("thread-running-spin-{id}"),
                        Animation::new(Duration::from_millis(1600))
                            .repeat()
                            .with_easing(linear),
                        |el, delta| {
                            el.with_transformation(Transformation::rotate(percentage(delta)))
                        },
                    )
                    .into_any_element()
                } else {
                    icon.into_any_element()
                }
            }
            RowIcon::External(path) => gpui::svg()
                .path(path)
                .size(px(16.))
                .text_color(theme.muted_foreground)
                .into_any_element(),
        }
    };

    h_flex()
        .id(format!("thread-item-{id}"))
        .group(group.clone())
        .w_full()
        .relative()
        .overflow_hidden()
        // Left guide rail for nested rows ties a member/fork row to its
        // parent, making the leader→member hierarchy visually explicit.
        .when(item.nested, |this| {
            this.child(
                gpui::div()
                    .absolute()
                    .left(px(12.))
                    .top(px(0.))
                    .bottom(px(0.))
                    .w(px(1.))
                    .bg(theme.border),
            )
        })
        .pl(px(8.) + item.indent)
        .pr_2()
        .py_1()
        .gap_2()
        .items_start()
        .rounded(theme.radius)
        .when(!item.selected, |this| {
            this.hover(move |s| s.bg(wash.opacity(0.08)))
                .active(move |s| s.bg(wash.opacity(0.18)))
        })
        // Open click branches on `kind`: a thread opens the conversation, an
        // external session switches the main area to its running TUI.
        .on_click(cx.listener(move |_this, _ev, _window, cx| match open_kind {
            RowKind::Thread { .. } => cx.emit(SidebarEvent::OpenThread(id_open.clone())),
            RowKind::External => cx.emit(SidebarEvent::OpenExternalSession(id_open.clone())),
        }))
        .when_some(wash_overlay, |this, overlay| this.child(overlay))
        .when(item.team_leader, |this| {
            // Team collapse toggle: folds/unfolds the member rows nested
            // under this leader without opening the conversation.
            let id_team = id_team.clone();
            this.child(
                gpui::div()
                    .id(format!("thread-team-chevron-{id}"))
                    .cursor_pointer()
                    .child(
                        Icon::new(if item.team_collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .xsmall()
                        .text_color(theme.muted_foreground),
                    )
                    .on_click(cx.listener(move |this, _ev, _window, cx| {
                        cx.stop_propagation();
                        if !this.team_collapsed.remove(&id_team) {
                            this.team_collapsed.insert(id_team.clone());
                        }
                        cx.notify();
                    })),
            )
        })
        .child(leading_icon)
        .child(
            v_flex()
                .w_full()
                .gap_1()
                .flex_1()
                .min_w_0()
                .child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .items_center()
                        .min_w_0()
                        .when(item.pinned, |this| {
                            this.child(
                                Icon::new(IconName::Star)
                                    .xsmall()
                                    .text_color(theme.accent_foreground),
                            )
                        })
                        .when(item.pending_auth, |this| {
                            this.child(
                                gpui::div()
                                    .id(format!("thread-pending-auth-{id}"))
                                    .child(
                                        Icon::new(IconName::LoaderCircle)
                                            .xsmall()
                                            .text_color(theme.accent),
                                    )
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(i18n::t("sidebar-pending-auth"))
                                            .build(window, cx)
                                    }),
                            )
                        })
                        .child(
                            gpui::div()
                                .flex_1()
                                .min_w_0()
                                // Single-line invariant: the row height must
                                // stay bounded for any title (LLM-generated
                                // thread titles included) at any width.
                                .truncate()
                                .text_sm()
                                .text_color(title_color)
                                .child(title),
                        ),
                )
                .child({
                    // Tag slot: the inline input while an edit is in flight,
                    // else the persisted chip; external rows render neither.
                    let editing_input = sidebar
                        .tag_edit
                        .as_ref()
                        .filter(|e| e.id == id)
                        .map(|e| e.input.clone());
                    let tag_slot: Option<AnyElement> = if let Some(input) = editing_input {
                        Some(render_tag_edit(&id, &input, cx))
                    } else {
                        item.tag
                            .as_ref()
                            .map(|tag| render_tag_chip(&id, tag, &group, cx))
                    };
                    let menu_open = sidebar.row_menu_open.as_deref() == Some(id.as_str());
                    h_flex()
                        .w_full()
                        .gap_1()
                        .items_center()
                        .child(tag_element)
                        .children(tag_slot)
                        .child(
                            h_flex()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .group_hover(group.clone(), |s| s.invisible())
                                .child(gpui::div().child(updated)),
                        )
                        .child(
                            h_flex()
                                .gap_0p5()
                                // The trigger stays visible while its menu is
                                // open so the anchor doesn't vanish
                                // mid-interaction.
                                .when(!menu_open, |this| {
                                    this.invisible().group_hover(group.clone(), |s| s.visible())
                                })
                                .child(match &item.kind {
                                    // Thread rows overflow into the three-dot
                                    // menu (Archive + Tag); external sessions
                                    // keep the single hover close button.
                                    RowKind::Thread { .. } => {
                                        render_thread_menu_trigger(item, sidebar, cx)
                                    }
                                    RowKind::External => render_hover_action(
                                        id_archive.clone(),
                                        item.kind.clone(),
                                        item.resumable,
                                        cx,
                                    ),
                                }),
                        )
                }),
        )
        .into_any_element()
}

/// The inline tag input of the row currently editing its tag. The wrapper
/// swallows clicks (the row's open-thread click must not fire while the
/// user works the input) and catches the input's propagated `Escape` as a
/// cancel; Enter/blur commits flow through the input's own events.
fn render_tag_edit(id: &str, input: &Entity<InputState>, cx: &mut Context<Sidebar>) -> AnyElement {
    let id = id.to_string();
    gpui::div()
        .id(SharedString::from(format!("thread-tag-edit-{id}")))
        .w(px(90.))
        .on_click(|_ev, _window, cx| cx.stop_propagation())
        .on_action(cx.listener(
            move |this, _: &gpui_component::input::Escape, _window, cx| {
                cx.stop_propagation();
                this.cancel_tag_edit(cx);
            },
        ))
        .child(Input::new(input).appearance(false).xsmall())
        .into_any_element()
}

/// The persisted user tag: a chip beside the id tag with an ✕ to clear it;
/// double-clicking it renames inline. Clicks stop propagation so chip
/// interaction never trips the row's open-thread click.
fn render_tag_chip(
    id: &str,
    tag: &str,
    row_group: &SharedString,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    let id = id.to_string();
    let id_rename = id.clone();
    let id_clear = id.clone();
    let tag_text = tag.to_string();
    let row_group = row_group.clone();
    // Same visual language as the thread-id tag; the clear affordance rides
    // the row hover like every other row action (space reserved, so the row
    // never reflows mid-hover).
    h_flex()
        .items_center()
        .id(SharedString::from(format!("thread-tag-chip-{id}")))
        .cursor_pointer()
        .tooltip(move |window, cx| Tooltip::new(i18n::t("sidebar-thread-tag")).build(window, cx))
        .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, window, cx| {
            cx.stop_propagation();
            if ev.click_count() >= 2 {
                this.begin_tag_edit(id_rename.clone(), TagEditMode::Renaming, window, cx);
            }
        }))
        .child(
            Tag::new()
                .with_variant(TagVariant::Secondary)
                .outline()
                .small()
                .child(tag_text),
        )
        .child(
            Button::new(format!("thread-tag-clear-{id}"))
                .ghost()
                .xsmall()
                .icon(IconName::Close)
                .tooltip(i18n::t("sidebar-thread-tag-clear"))
                .invisible()
                .group_hover(row_group, |s| s.visible())
                .on_click(cx.listener(move |_this, _ev, _window, cx| {
                    cx.stop_propagation();
                    cx.emit(SidebarEvent::SetThreadTag(id_clear.clone(), None));
                })),
        )
        .into_any_element()
}

/// The thread row's overflow trigger: a three-dot button opening the
/// Archive + Tag popup. Clicking it again toggles the menu closed; the
/// dropdown anchors below the button (deferred, so it escapes the row's
/// `overflow_hidden` and paints above sibling rows).
fn render_thread_menu_trigger(
    item: &SidebarThreadItem,
    sidebar: &Sidebar,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    let id = item.id.clone();
    let id_toggle = id.clone();
    let archived = matches!(item.kind, RowKind::Thread { archived: true });
    let has_tag = item.tag.is_some();
    let button = Button::new(format!("thread-menu-{id}"))
        .ghost()
        .xsmall()
        .icon(IconName::Ellipsis)
        .tooltip(i18n::t("sidebar-thread-menu"))
        .on_click(cx.listener(move |this, _ev, window, cx| {
            cx.stop_propagation();
            if this.row_menu_open.as_deref() == Some(id_toggle.as_str()) {
                this.close_row_menu();
            } else {
                this.open_row_menu(id_toggle.clone(), archived, has_tag, window, cx);
            }
            cx.notify();
        }));
    let dropdown = sidebar.render_row_menu_dropdown(&id);
    gpui::div()
        .relative()
        .child(button)
        .children(dropdown)
        .into_any_element()
}

/// The hover action on an external session row's right edge: the Inbox
/// close button while live, a Play resume button when resumable (the row
/// click already emits the same `OpenExternalSession` event, so the
/// affordance is just a visible hint). Thread rows use the overflow menu
/// instead. Uses `cx.listener` so the click emits on the sidebar's own
/// context (where `EventEmitter<SidebarEvent>` lives) rather than the bare
/// `App` the standalone `on_click` receives.
fn render_hover_action(
    id: String,
    kind: RowKind,
    resumable: bool,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    if resumable {
        let id_open = id.clone();
        return Button::new(format!("resume-external-{id}"))
            .ghost()
            .xsmall()
            .icon(IconName::Play)
            .tooltip(i18n::t("sidebar-resume-external"))
            .on_click(cx.listener(move |_this, _ev, _window, cx| {
                cx.stop_propagation();
                cx.emit(SidebarEvent::OpenExternalSession(id_open.clone()));
            }))
            .into_any_element();
    }
    let id = id.clone();
    Button::new(format!("archive-thread-{id}"))
        .ghost()
        .xsmall()
        .icon(IconName::Inbox)
        .tooltip(match &kind {
            RowKind::Thread { .. } => i18n::t("sidebar-archive"),
            RowKind::External => i18n::t("sidebar-close-external"),
        })
        .on_click(cx.listener(move |_this, _ev, _window, cx| {
            cx.stop_propagation();
            match kind {
                RowKind::Thread { archived } => {
                    cx.emit(SidebarEvent::ArchiveThread(id.clone(), !archived));
                }
                RowKind::External => {
                    cx.emit(SidebarEvent::ArchiveExternalSession(id.clone()));
                }
            }
        }))
        .into_any_element()
}

fn approval_mode_color(mode: i64, theme: &Theme) -> gpui::Hsla {
    match PermissionMode::from_i64(mode) {
        PermissionMode::ReadOnly => theme.warning,
        PermissionMode::WorkspaceWrite => theme.info,
        PermissionMode::DangerFullAccess => theme.danger,
    }
}

fn format_relative(epoch: i64) -> String {
    let now = chrono::Local::now().timestamp();
    let diff = (now - epoch).max(0);
    if diff < 60 {
        i18n::t("sidebar-time-just-now").to_string()
    } else if diff < 3600 {
        i18n::t_count("sidebar-time-minutes", diff / 60).to_string()
    } else if diff < 86_400 {
        i18n::t_count("sidebar-time-hours", diff / 3600).to_string()
    } else if diff < 604_800 {
        i18n::t_count("sidebar-time-days", diff / 86_400).to_string()
    } else {
        i18n::t_count("sidebar-time-weeks", diff / 604_800).to_string()
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_component::theme::ThemeColor;

    /// A real (non-transparent) theme — `Theme::default()` derives all-zero
    /// colors, so it cannot validate wash visibility.
    fn real_theme() -> Theme {
        Theme::from(&*ThemeColor::light())
    }

    fn sample_thread() -> manox_agent::ThreadSummary {
        manox_agent::ThreadSummary {
            id: "thread-abcdef12".into(),
            summary: "Summarize the diff".into(),
            title: None,
            title_override: None,
            model_id: "claude".into(),
            provider_id: None,
            approval_mode: PermissionMode::default().as_i64(),
            project: String::new(),
            depth: 0,
            parent_id: None,
            archived: false,
            pinned: false,
            tag: None,
            has_unread: false,
            errored: false,
            created_at: 0,
            interacted_at: 0,
            updated_at: 0,
            cumulative_total_tokens: 0,
        }
    }

    fn sample_external() -> crate::external_session::ExternalSessionSummary {
        crate::external_session::ExternalSessionSummary {
            id: "external:claude:deadbeef".into(),
            kind: crate::external_session::SessionKind::ClaudeCode,
            created_at: 0,
            project: None,
            title: None,
            cx_session_id: "deadbeef".into(),
            socket_path: None,
            resumable: false,
            resuming: false,
        }
    }

    /// A member whose leader is not in the current partition (e.g. an
    /// archived leader) must not disappear: the row degrades to top-level,
    /// matching the webview forest.
    #[test]
    fn team_forest_keeps_members_when_leader_is_in_another_partition() {
        let thread = |id: &str, parent: Option<&str>, depth: i32, at: i64| {
            let mut s = sample_thread();
            s.id = id.into();
            s.parent_id = parent.map(str::to_string);
            s.depth = depth;
            s.interacted_at = at;
            s
        };
        // Only the active partition is handed to the forest; the leader sits
        // in the archived partition, so its depth-1 member must surface as a
        // top-level row instead of vanishing.
        let threads = vec![thread("member", Some("leader"), 1, 100)];
        let rows = team_forest(&HashSet::new(), &threads, &[]);
        let ids: Vec<&str> = rows.iter().map(|r| r.row.id()).collect();
        assert_eq!(ids, vec!["member"]);
        assert_eq!(rows[0].indent, 0.0);
        assert!(!rows[0].team_leader);
    }

    /// The team forest orders top-level rows by recency with members nested
    /// indented right after their leader; a collapsed leader hides its
    /// subtree and an orphan stays top-level.
    #[test]
    fn team_forest_nests_members_and_honors_collapse() {
        let thread = |id: &str, parent: Option<&str>, depth: i32, at: i64| {
            let mut s = sample_thread();
            s.id = id.into();
            s.parent_id = parent.map(str::to_string);
            s.depth = depth;
            s.interacted_at = at;
            s
        };
        let threads = vec![
            thread("leader", None, 0, 100),
            thread("member-old", Some("leader"), 1, 50),
            thread("member-new", Some("leader"), 1, 200),
            thread("orphan", Some("gone"), 0, 300),
        ];

        let collapsed = HashSet::new();
        let rows = team_forest(&collapsed, &threads, &[]);
        let ids: Vec<&str> = rows.iter().map(|r| r.row.id()).collect();
        // Orphan sorts newest first; the leader carries its members right
        // after it, members in activity order.
        assert_eq!(ids, vec!["orphan", "leader", "member-new", "member-old"]);
        assert!(rows[1].team_leader);
        assert_eq!(rows[2].indent, 14.0);
        assert_eq!(rows[3].indent, 14.0);
        assert!(!rows[0].team_leader);

        let collapsed = HashSet::from(["leader".to_string()]);
        let rows = team_forest(&collapsed, &threads, &[]);
        let ids: Vec<&str> = rows.iter().map(|r| r.row.id()).collect();
        assert_eq!(ids, vec!["orphan", "leader"]);
        assert!(rows[1].team_collapsed);
    }

    /// The unified row projection: a selected external session carries the
    /// same `selected` flag, id-namespace, and a non-transparent wash as a
    /// selected manox thread, so `set_selected(external_id)` highlights its
    /// row through the same `render_thread_item` wash the threads use.
    #[test]
    fn external_selection_mirrors_thread_selection() {
        let theme = real_theme();

        let thread = SidebarThreadItem::from_thread(
            &sample_thread(),
            true,
            ThreadLiveState::default(),
            RowNesting {
                indent: px(0.),
                team_leader: false,
                team_collapsed: false,
                nested: false,
            },
            &theme,
        );
        assert!(thread.selected);
        assert_eq!(thread.id, "thread-abcdef12");
        assert_eq!(
            thread.wash,
            approval_mode_color(PermissionMode::default().as_i64(), &theme)
        );
        assert!(thread.wash.a > 0.0);
        assert!(matches!(thread.kind, RowKind::Thread { archived: false }));

        let external = SidebarThreadItem::from_external(&sample_external(), true, px(0.), &theme);
        assert!(external.selected);
        // The row's id is the same string the click emits (`OpenExternalSession`)
        // and the workspace feeds to `set_selected`, so the selection lands on
        // this row's `is_selected` check.
        assert_eq!(external.id, "external:claude:deadbeef");
        // Fully unified: the external row's wash is the same visible tint a
        // default (Workspace Write) manox thread carries — never the near-white
        // `theme.accent`, which disappears against the light sidebar.
        assert_eq!(external.wash, thread.wash);
        assert_eq!(external.wash, theme.info);
        assert!(external.wash.a > 0.0);
        assert!(matches!(external.kind, RowKind::External));
    }

    /// Without a cx session id (plain PTY sessions, or an IPC bind failure)
    /// the id tag shows the trailing uuid segment of the manox-internal id —
    /// never the `external` prefix every such row would otherwise share.
    #[test]
    fn external_short_id_falls_back_to_uuid_segment() {
        let theme = real_theme();
        let mut summary = sample_external();
        summary.kind = crate::external_session::SessionKind::Terminal;
        summary.cx_session_id = String::new();
        summary.id = "external:terminal:0123abcd-uuid".into();
        let item = SidebarThreadItem::from_external(&summary, false, px(0.), &theme);
        assert_eq!(item.short_id, "0123abcd");
    }

    /// Deselected rows stay deselected for both kinds (hover-only feedback),
    /// so the `selected` flag alone drives the persistent wash.
    #[test]
    fn external_and_thread_rows_share_deselected_state() {
        let theme = real_theme();
        let thread = SidebarThreadItem::from_thread(
            &sample_thread(),
            false,
            ThreadLiveState::default(),
            RowNesting {
                indent: px(0.),
                team_leader: false,
                team_collapsed: false,
                nested: false,
            },
            &theme,
        );
        let external = SidebarThreadItem::from_external(&sample_external(), false, px(0.), &theme);
        assert!(!thread.selected);
        assert!(!external.selected);
    }

    /// The pinned slot shows the Conversations header when no registered
    /// projects exist — the conversations section is the only section, so its
    /// header is pinned from the top regardless of scroll.
    #[test]
    fn pinned_section_without_projects_is_conversations() {
        assert_eq!(
            pinned_section(false, px(0.), None),
            PinnedSection::Conversations
        );
        assert_eq!(
            pinned_section(false, px(120.), Some(px(100.))),
            PinnedSection::Conversations
        );
    }

    /// Inside the projects section the Projects header stays pinned; once the
    /// projects content has fully scrolled past the top the Conversations
    /// header takes over.
    #[test]
    fn pinned_section_tracks_projects_boundary() {
        // First frame (no measurement yet) stays on Projects.
        assert_eq!(pinned_section(true, px(50.), None), PinnedSection::Projects);
        assert_eq!(
            pinned_section(true, px(0.), Some(px(200.))),
            PinnedSection::Projects
        );
        assert_eq!(
            pinned_section(true, px(199.), Some(px(200.))),
            PinnedSection::Projects
        );
        assert_eq!(
            pinned_section(true, px(200.), Some(px(200.))),
            PinnedSection::Conversations
        );
        assert_eq!(
            pinned_section(true, px(500.), Some(px(200.))),
            PinnedSection::Conversations
        );
    }

    /// The loose/folder partition for external sessions: unbound rows and
    /// rows bound to an unregistered path (removed folder, or a cwd never
    /// bound as a project) stay loose; only a registered project pulls its
    /// sessions into the folder group.
    #[test]
    fn external_session_is_loose_partitions_by_registered_projects() {
        let known = vec!["/p/a".to_string()];
        assert!(external_session_is_loose(None, &known));
        assert!(!external_session_is_loose(
            Some(std::path::Path::new("/p/a")),
            &known
        ));
        assert!(external_session_is_loose(
            Some(std::path::Path::new("/p/b")),
            &known
        ));
        assert!(external_session_is_loose(
            Some(std::path::Path::new("/home/user")),
            &[]
        ));
    }

    /// A sidecar-restored row carries its resumable/resuming states into the
    /// unified row item, so the renderer can dim it and swap in the loading
    /// indicator without touching the live-row path.
    #[test]
    fn resumable_row_propagates_resume_states() {
        let theme = real_theme();
        let mut summary = sample_external();
        summary.resumable = true;
        let item = SidebarThreadItem::from_external(&summary, false, px(0.), &theme);
        assert!(item.resumable);
        assert!(!item.resuming);
        summary.resuming = true;
        let item = SidebarThreadItem::from_external(&summary, false, px(0.), &theme);
        assert!(item.resuming);
    }
}
