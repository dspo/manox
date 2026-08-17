//! Conversation history sidebar.
//!
//! A standalone gpui Entity that subscribes to `ThreadStore` and lists past threads. Clicking a
//! conversation entry emits `OpenThread(id)`; the "+" button on each project folder header and
//! the "Conversations" section header opens a new-session popup menu (Manox / Claude Code /
//! Codex / GitHub Copilot). Workspace subscribes to these events.
//!
//! Threads bound to a project (chosen on the first screen) are grouped under a collapsible folder
//! in the "Projects" section, keyed by project path; the rest fall under "Conversations". The top
//! menu and bottom account footer are static decoration.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use agent::i18n;
use agent::thread::ApprovalMode;
use agent::{ThreadStore, ThreadStoreEvent};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, Pixels, Render, ScrollHandle, SharedString, Subscription, Transformation,
    WeakEntity, Window, deferred, ease_in_out, linear, percentage, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{PopupMenu, PopupMenuItem},
    tag::{Tag, TagVariant},
    tooltip::Tooltip,
    v_flex,
};

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
    Thread(agent::ThreadSummary),
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

/// Events the sidebar emits to the Workspace.
#[derive(Debug, Clone)]
pub enum SidebarEvent {
    OpenThread(String),
    NewThread,
    /// New thread bound to a specific project path.
    NewThreadWithProject(PathBuf),
    /// User clicked archive/unarchive. The bool is the new archived state.
    ArchiveThread(String, bool),
    /// Launch an external agent CLI session with a user-picked provider + model
    /// (the cascade wizard's terminal action). The kind identifies the agent
    /// (`claude` / `codex` / `copilot`); the two strings are provider name +
    /// model id; the optional PathBuf is the project path to use as the CLI's
    /// cwd (when launched from a project folder's `+` button).
    SpawnExternalSession(
        crate::external_session::SessionKind,
        String,
        String,
        Option<PathBuf>,
    ),
    /// Launch a plain PTY session with no cx provider injection — the user's
    /// shell (`Terminal`). The optional PathBuf is the project path to use as
    /// the session's cwd (when launched from a project folder's `+` button);
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
}

pub struct Sidebar {
    store: Entity<ThreadStore>,
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
    /// Live external agent sessions, projected from the Workspace's canonical
    /// list. Merged into the Conversations list by recency (an `external:` id
    /// in `selected` highlights the active one).
    external_sessions: Vec<crate::external_session::ExternalSessionSummary>,
    /// Whether the new-session `PopupMenu` (Manox / Claude Code / Codex /
    /// GitHub Copilot) is open.
    new_session_open: bool,
    new_session_menu: Option<Entity<PopupMenu>>,
    new_session_menu_sub: Option<Subscription>,
    /// The project path the new-session menu was opened from. `None` when
    /// opened from the Conversations header; `Some` when opened from a project
    /// folder's `+` button. The menu closures read this to decide whether to
    /// emit `NewThread` vs `NewThreadWithProject`, and to pass the project path
    /// as the CWD for external CLI sessions.
    new_session_project: Option<PathBuf>,
    /// Live width driven by dragging the divider on the right edge. Updated
    /// from the owning `Workspace` on every drag-move tick.
    width: Pixels,
    /// Scroll offset + child-bounds reader for the sidebar scroll body; the
    /// sticky section-header overlay reads it to decide which header to show.
    scroll_handle: ScrollHandle,
    _sub: Subscription,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Sidebar {
    pub fn new(width: Pixels, cx: &mut Context<Self>) -> Self {
        let store = agent::thread_store_global();
        let sub = cx.subscribe(
            &store,
            |_this, _store, ev: &ThreadStoreEvent, cx| match ev {
                ThreadStoreEvent::SummariesUpdated | ThreadStoreEvent::RunningChanged => {
                    cx.notify();
                }
            },
        );
        Self {
            store,
            selected: None,
            collapsed: HashSet::new(),
            prev_selected: None,
            select_gen: 0,
            external_sessions: Vec::new(),
            new_session_open: false,
            new_session_menu: None,
            new_session_menu_sub: None,
            new_session_project: None,
            width,
            scroll_handle: ScrollHandle::new(),
            _sub: sub,
        }
    }

    pub fn store(&self) -> Entity<ThreadStore> {
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

    /// Open the new-session `PopupMenu`. `project` is `None` when opened from
    /// the Conversations header, `Some(path)` when opened from a project
    /// folder's `+` button — the path determines the CWD for external CLI
    /// sessions and whether the new Manox session binds to the project.
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
            // the click spawns immediately in the menu's project directory
            // (workspace cwd when opened from the Conversations header).
            {
                let kind = crate::external_session::SessionKind::Terminal;
                let sidebar_plain = sidebar.clone();
                menu = menu.item(
                    PopupMenuItem::new(kind.label())
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
                        .disabled(!cx::vscode_app_installed())
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

    /// Build the new-session dropdown anchored below the `+` button that
    /// opened it. Deferred so it paints above sibling rows; `top_full()` is
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

    /// A collapsible project folder: a clickable header (chevron + folder icon +
    /// basename) over its indented conversation rows when expanded. The `+`
    /// button opens the new-session popup menu with the project path so the
    /// workspace can set the CWD for external CLI sessions.
    fn render_project_group(
        &self,
        path: &str,
        group: &[agent::ThreadSummary],
        selected: Option<&str>,
        store: &Entity<ThreadStore>,
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
                        Button::new(format!("new-thread-in-project-{key}"))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip(i18n::t("sidebar-new-chat"))
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
                    // from *this* project folder's `+` button, so the menu
                    // anchors below the clicked button instead of the
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
            // an external CLI session launched from the folder's `+` sits
            // among the folder's manox threads instead of in the loose list.
            let mut rows: Vec<SidebarRow> = group
                .iter()
                .cloned()
                .map(SidebarRow::Thread)
                .chain(externals.iter().cloned().map(SidebarRow::External))
                .collect();
            rows.sort_by_key(|r| std::cmp::Reverse(r.sort_key()));
            v_flex()
                .w_full()
                .gap_0p5()
                .children(rows.into_iter().map(|row| {
                    let is_selected = selected == Some(row.id());
                    match row {
                        SidebarRow::Thread(s) => render_thread_item(
                            &SidebarThreadItem::from_thread(
                                &s,
                                is_selected,
                                ThreadLiveState {
                                    running: store.read(cx).is_running(&s.id),
                                    pending_auth: store.read(cx).pending_auth_contains(&s.id),
                                    pending_plan: store.read(cx).pending_plan_contains(&s.id),
                                    background_work: store.read(cx).background_work_contains(&s.id),
                                },
                                px(16.),
                                &theme,
                            ),
                            slide,
                            &theme,
                            cx,
                        ),
                        SidebarRow::External(s) => render_thread_item(
                            &SidebarThreadItem::from_external(&s, is_selected, px(16.), &theme),
                            slide,
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
        let summaries = self.store.read(cx).summaries().to_vec();
        let known_projects = self.store.read(cx).known_projects().to_vec();
        let selected = self.selected.clone();
        let store = self.store.clone();

        let mut projects: Vec<(String, Vec<agent::ThreadSummary>)> = Vec::new();
        let mut loose: Vec<agent::ThreadSummary> = Vec::new();
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
        // External sessions not bound to a project stay in the loose
        // Conversations list; bound ones are pulled into their folder group
        // inside `render_project_group` (filtered by project path there).
        let loose_externals: Vec<crate::external_session::ExternalSessionSummary> = self
            .external_sessions
            .iter()
            .filter(|s| s.project.is_none())
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
                                let mut rows: Vec<SidebarRow> = loose
                                    .into_iter()
                                    .map(SidebarRow::Thread)
                                    .chain(loose_externals.into_iter().map(SidebarRow::External))
                                    .collect();
                                rows.sort_by_key(|r| std::cmp::Reverse(r.sort_key()));
                                v_flex()
                                    .w_full()
                                    .gap_0p5()
                                    .children(rows.into_iter().map(|row| {
                                        let is_selected = selected.as_deref() == Some(row.id());
                                        match row {
                                            SidebarRow::Thread(s) => render_thread_item(
                                                &SidebarThreadItem::from_thread(
                                                    &s,
                                                    is_selected,
                                                    ThreadLiveState {
                                                        running: store.read(cx).is_running(&s.id),
                                                        pending_auth: store
                                                            .read(cx)
                                                            .pending_auth_contains(&s.id),
                                                        pending_plan: store
                                                            .read(cx)
                                                            .pending_plan_contains(&s.id),
                                                        background_work: store
                                                            .read(cx)
                                                            .background_work_contains(&s.id),
                                                    },
                                                    px(0.),
                                                    &theme,
                                                ),
                                                &slide,
                                                &theme,
                                                cx,
                                            ),
                                            SidebarRow::External(s) => render_thread_item(
                                                &SidebarThreadItem::from_external(
                                                    &s,
                                                    is_selected,
                                                    px(0.),
                                                    &theme,
                                                ),
                                                &slide,
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

/// Build the provider→model cascade shared by every agent submenu in the
/// new-session menu. Models are drawn from the shared pi provider registry,
/// filtered by the agent id (registration metadata `agents`, empty = visible
/// to all); they are grouped by provider display name, each provider a nested
/// submenu. Picking a model invokes `emit` with (provider, model id, project)
/// — the project path (if any) is read from the sidebar's
/// `new_session_project` field so the handler can set the CWD / open
/// directory. The emitted model id is the raw cx config key
/// (`metadata["config_id"]`), which cx matches verbatim.
fn build_model_cascade(
    menu: PopupMenu,
    agent_id: &'static str,
    sidebar: &WeakEntity<Sidebar>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
    emit: impl Fn(&mut Sidebar, &mut Context<Sidebar>, String, String, Option<PathBuf>)
    + Clone
    + 'static,
) -> PopupMenu {
    /// One cascade entry: the raw cx config key plus its display name.
    struct Entry {
        config_id: String,
        display: String,
    }
    let mut providers: Vec<(String, Vec<Entry>)> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for m in agent::pi_providers::global().models() {
        // Missing metadata = non-cx registration (visible); otherwise the
        // effective agent list must contain the cascade's agent (parity
        // with the retired manox `visible_agents` filter).
        let visible = m
            .metadata
            .get("agents")
            .and_then(|v| v.as_array())
            .map(|list| {
                list.iter()
                    .any(|a| a.as_str().is_some_and(|a| a == agent_id))
            })
            .unwrap_or(true);
        if !visible {
            continue;
        }
        let prov = agent::pi_providers::display_provider_name(&m);
        let config_id = m
            .metadata
            .get("config_id")
            .and_then(|v| v.as_str())
            .unwrap_or(m.id.as_str())
            .to_string();
        if !seen.insert((prov.clone(), config_id.clone())) {
            continue; // same model registered on several wire apis
        }
        let entry = Entry {
            config_id,
            display: agent::pi_providers::display_name(&m),
        };
        // Lookup-based grouping (not adjacency): the registry is sorted by
        // registration name, so equal display names must still merge.
        match providers.iter_mut().find(|(name, _)| *name == prov) {
            Some((_, entries)) => entries.push(entry),
            None => providers.push((prov, vec![entry])),
        }
    }

    let mut menu = menu;
    if providers.is_empty() {
        menu = menu.label(i18n::t("external-wizard-no-model"));
        return menu;
    }
    for (prov_name, models) in providers {
        let sidebar = sidebar.clone();
        let prov_for_items = prov_name.clone();
        let emit = emit.clone();
        menu = menu.submenu(prov_name, window, cx, move |submenu, _window, _cx| {
            let mut submenu = submenu;
            for m in &models {
                let model_id = m.config_id.clone();
                let model_name = m.display.clone();
                let prov = prov_for_items.clone();
                let sidebar = sidebar.clone();
                let emit = emit.clone();
                submenu = submenu.item(
                    PopupMenuItem::element(move |_window, _cx| {
                        gpui::div()
                            .text_sm()
                            .child(model_name.clone())
                            .into_any_element()
                    })
                    .on_click(move |_, _, cx: &mut App| {
                        let _ = sidebar.update(cx, |this, cx| {
                            let project = this.new_session_project.clone();
                            emit(this, cx, prov.clone(), model_id.clone(), project);
                            cx.notify();
                        });
                    }),
                );
            }
            submenu
        });
    }
    menu
}

/// Cascade for the external-agent CLI submenus (Claude Code / Codex / GitHub
/// Copilot): picking a model emits `SpawnExternalSession(kind, provider,
/// model, project)` — the workspace spawns the agent CLI in the project's
/// directory and mounts its TUI in the main area.
fn build_agent_model_cascade(
    menu: PopupMenu,
    kind: crate::external_session::SessionKind,
    agent_id: &'static str,
    sidebar: &WeakEntity<Sidebar>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    build_model_cascade(
        menu,
        agent_id,
        sidebar,
        window,
        cx,
        move |_this, cx, provider, model, project| {
            cx.emit(SidebarEvent::SpawnExternalSession(
                kind, provider, model, project,
            ));
        },
    )
}

/// Leading icon for a unified sidebar row. Manox threads carry the brand
/// mark; external agent sessions use their own brand SVG (resolved by
/// `ExtrasAssetSource`, tinted via `text_color`).
#[derive(Clone)]
enum RowIcon {
    Thread,
    External(&'static str),
}

/// What the row's hover "Inbox" archive action emits — a thread toggles its
/// archived flag, an external session tears itself down (kill + drop).
#[derive(Clone)]
enum RowKind {
    Thread { archived: bool },
    External,
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
    /// use the cx session id prefix (traceable to `~/.config/cx/sessions/<id>.sock`).
    short_id: String,
    /// Clipboard payload for the id-tag click. Threads copy the thread id;
    /// external sessions copy the full cx session id (or socket path).
    copy_value: String,
    title: String,
    updated: String,
    pinned: bool,
    has_unread: bool,
    errored: bool,
    running: bool,
    /// A tool authorization is parked waiting for the user's verdict (the
    /// thread's card is only visible when it is the active thread).
    pending_auth: bool,
    /// A plan-review verdict is due; the icon stays blue static, not spinning.
    pending_plan: bool,
    /// Live monitors / background bash: the loop can still self-advance even
    /// with no turn in flight, so the icon keeps spinning.
    background_work: bool,
    /// A sidecar-restored row with no live process; clicking it resumes the
    /// CLI. Rendered dimmed with a resume hover action.
    resumable: bool,
    /// A resume is in flight for this row; rendered with a loading indicator.
    resuming: bool,
    selected: bool,
    indent: gpui::Pixels,
    icon: RowIcon,
    /// Selection-wash color: threads tint by approval mode, external rows use
    /// the theme accent.
    wash: gpui::Hsla,
    kind: RowKind,
}

impl SidebarThreadItem {
    fn from_thread(
        summary: &agent::ThreadSummary,
        selected: bool,
        live: ThreadLiveState,
        indent: gpui::Pixels,
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
            has_unread: summary.has_unread,
            errored: summary.errored,
            running: live.running,
            pending_auth: live.pending_auth,
            pending_plan: live.pending_plan,
            background_work: live.background_work,
            resumable: false,
            resuming: false,
            selected,
            indent,
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
            errored: false,
            running: false,
            pending_auth: false,
            pending_plan: false,
            background_work: false,
            resumable: summary.resumable,
            resuming: summary.resuming,
            selected,
            indent,
            icon: RowIcon::External(summary.kind.icon_asset()),
            // Same wash as AutoPilot threads: `theme.accent` resolves to
            // `neutral-100` (near-white) in the forced Light theme, which made
            // the external row's hover/active/selected wash invisible on the
            // white sidebar. `theme.info` (cyan) is the visible default tint.
            wash: theme.info,
            kind: RowKind::External,
        }
    }
}

/// Render one unified sidebar row — threads and external agent sessions share
/// the layout (leading icon → title → id tag + updated time + hover archive),
/// the selection-wash slide animation, and the hover "Inbox" archive action.
/// Only the emitted event and the leading icon differ by `kind`.
fn render_thread_item(
    item: &SidebarThreadItem,
    slide: &SlideCtx,
    theme: &Theme,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    let id = item.id.clone();
    let id_open = id.clone();
    let id_archive = id.clone();
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
                                .overflow_hidden()
                                .text_sm()
                                .text_color(title_color)
                                .child(title),
                        ),
                )
                .child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .items_center()
                        .child(tag_element)
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
                                .invisible()
                                .group_hover(group.clone(), |s| s.visible())
                                .child(render_hover_action(
                                    id_archive.clone(),
                                    item.kind.clone(),
                                    item.resumable,
                                    cx,
                                )),
                        ),
                ),
        )
        .into_any_element()
}

/// The hover action on a row's right edge. Threads and live external sessions
/// get the Inbox close/archive button; a resumable row gets a Play resume
/// button (the row click already emits the same `OpenExternalSession` event,
/// so the affordance is just a visible hint). Uses `cx.listener` so the click
/// emits on the sidebar's own context (where `EventEmitter<SidebarEvent>`
/// lives) rather than the bare `App` the standalone `on_click` receives.
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
    match ApprovalMode::from_i64(mode) {
        ApprovalMode::AutoPilot => theme.info,
        ApprovalMode::Danger => theme.danger,
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

    fn sample_thread() -> agent::ThreadSummary {
        agent::ThreadSummary {
            id: "thread-abcdef12".into(),
            summary: "Summarize the diff".into(),
            title: None,
            title_override: None,
            model_id: "claude".into(),
            provider_id: None,
            approval_mode: 0,
            project: String::new(),
            depth: 0,
            parent_id: None,
            archived: false,
            pinned: false,
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
            px(0.),
            &theme,
        );
        assert!(thread.selected);
        assert_eq!(thread.id, "thread-abcdef12");
        assert_eq!(thread.wash, approval_mode_color(0, &theme));
        assert!(thread.wash.a > 0.0);
        assert!(matches!(thread.kind, RowKind::Thread { archived: false }));

        let external = SidebarThreadItem::from_external(&sample_external(), true, px(0.), &theme);
        assert!(external.selected);
        // The row's id is the same string the click emits (`OpenExternalSession`)
        // and the workspace feeds to `set_selected`, so the selection lands on
        // this row's `is_selected` check.
        assert_eq!(external.id, "external:claude:deadbeef");
        // Fully unified: the external row's wash is the same visible tint a
        // default (AutoPilot) manox thread carries — never the near-white
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
            px(0.),
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
