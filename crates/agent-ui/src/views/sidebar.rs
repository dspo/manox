//! Conversation history sidebar.
//!
//! A standalone gpui Entity that subscribes to `ThreadStore` and lists past threads. Clicking a
//! conversation entry emits `OpenThread(id)`; the "+" button on each project folder header and
//! the "Conversations" section header opens a new-session popup menu (Manox Thread / Claude Code /
//! Codex / GitHub Copilot). Workspace subscribes to these events.
//!
//! Threads bound to a project (chosen on the first screen) are grouped under a collapsible folder
//! in the "Projects" section, keyed by project path; the rest fall under "Conversations". The top
//! menu and bottom account footer are static decoration.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use agent::provider::registry;
use agent::{ThreadStore, ThreadStoreEvent, i18n, thread::ApprovalMode};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, ClipboardItem, Context, DismissEvent, Entity,
    EventEmitter, Pixels, Render, SharedString, Subscription, WeakEntity, Window, deferred,
    ease_in_out, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::{PopupMenu, PopupMenuItem},
    tag::{Tag, TagVariant},
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
/// instead of living in separate sections. External rows do not participate
/// in the selection-slide (they have no `ThreadSummary` and are not part of
/// the slide's flat-id ordering); their renderer ignores `SlideCtx`.
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
    /// Whether the new-session `PopupMenu` (Manox Thread / Claude Code / Codex /
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

    /// Open the new-session `PopupMenu`. `project` is `None` when opened from
    /// the Conversations header, `Some(path)` when opened from a project
    /// folder's `+` button — the path determines the CWD for external CLI
    /// sessions and whether "Manox Thread" binds to the project.
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
            let sidebar_manox = sidebar.clone();
            menu = menu.item(new_session_item(
                IconName::Plus,
                i18n::t("sidebar-new-session-manox"),
                &theme,
                move |_, _window, cx| {
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
                },
            ));
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
                                store.read(cx).is_running(&s.id),
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
        let selected = self.selected.clone();
        let store = self.store.clone();

        let mut projects: Vec<(String, Vec<agent::ThreadSummary>)> = Vec::new();
        let mut loose: Vec<agent::ThreadSummary> = Vec::new();
        for s in &summaries {
            if s.project.is_empty() {
                loose.push(s.clone());
            } else if let Some(entry) = projects.iter_mut().find(|(p, _)| *p == s.project) {
                entry.1.push(s.clone());
            } else {
                projects.push((s.project.clone(), vec![s.clone()]));
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
                    .child(
                        v_flex()
                            .w_full()
                            .gap_0p5()
                            .child(static_menu_item(
                                "search",
                                IconName::Search,
                                i18n::t("sidebar-search"),
                                &theme,
                            ))
                            .child(static_menu_item(
                                "scheduled",
                                IconName::Calendar,
                                i18n::t("sidebar-scheduled"),
                                &theme,
                            )),
                    )
                    .children(
                        (!projects.is_empty()).then(|| {
                            section_header(i18n::t("sidebar-section-projects"), &theme, None)
                        }),
                    )
                    .children(projects.into_iter().map(|(path, group)| {
                        self.render_project_group(
                            &path,
                            &group,
                            selected.as_deref(),
                            &store,
                            &slide,
                            cx,
                        )
                    }))
                    .child(section_header(
                        i18n::t("sidebar-section-conversations"),
                        &theme,
                        Some(
                            gpui::div()
                                .relative()
                                .child(
                                    Button::new("conv-plus")
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
                                // Dropdown is deferred inside this relative wrapper so it paints
                                // after sibling rows (z-order) while staying positioned just below
                                // the button (`top_full()` is 100% of this wrapper's height).
                                .children(
                                    (self.new_session_open && self.new_session_project.is_none())
                                        .then(|| {
                                            self.render_new_session_dropdown(
                                                "new-session-dropdown".into(),
                                            )
                                        })
                                        .flatten(),
                                )
                                .into_any_element(),
                        ),
                    ))
                    // Merge loose threads and external sessions into one
                    // recency-ordered list so an external CLI session sits
                    // among manox threads instead of a separate section.
                    // External rows sort by spawn time (manox cannot observe
                    // in-TUI interaction); threads by last interaction.
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
                                            store.read(cx).is_running(&s.id),
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
            )
    }
}

/// A clickable top-level menu row.
fn menu_item(
    id: &'static str,
    icon: IconName,
    label: SharedString,
    theme: &Theme,
    on_click: Option<impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static>,
) -> AnyElement {
    let row = h_flex()
        .id(id)
        .w_full()
        .px_2()
        .py_1p5()
        .gap_2()
        .items_center()
        .rounded(theme.radius)
        .hover(|s| s.bg(theme.accent.opacity(0.08)))
        .child(Icon::new(icon).small().text_color(theme.muted_foreground))
        .child(
            gpui::div()
                .text_sm()
                .text_color(theme.foreground)
                .child(label),
        );

    match on_click {
        Some(handler) => row.on_click(handler).into_any_element(),
        None => row.into_any_element(),
    }
}

fn static_menu_item(
    id: &'static str,
    icon: IconName,
    label: SharedString,
    theme: &Theme,
) -> AnyElement {
    menu_item(
        id,
        icon,
        label,
        theme,
        None::<fn(&gpui::ClickEvent, &mut Window, &mut gpui::App)>,
    )
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

/// One row of the new-session `PopupMenu`: icon + label.
fn new_session_item(
    icon: IconName,
    label: impl Into<SharedString>,
    theme: &Theme,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> PopupMenuItem {
    let fg = theme.foreground;
    let muted = theme.muted_foreground;
    let icon = icon.clone();
    let label = label.into();
    PopupMenuItem::element(move |_window, _cx| {
        h_flex()
            .items_center()
            .gap_2()
            .child(Icon::new(icon.clone()).small().text_color(muted))
            .child(gpui::div().text_sm().text_color(fg).child(label.clone()))
            .into_any_element()
    })
    .on_click(on_click)
}

/// Build the provider→model cascade inside an external-agent submenu. Models
/// are drawn from `registry::global().models()` filtered by the agent's id
/// (`visible_agents`); they are grouped by provider, each provider a nested
/// submenu. Picking a model emits `SpawnExternalSession(kind, provider, model,
/// project)` to the sidebar — the project path (if any) is read from the
/// sidebar's `new_session_project` field so the workspace can set the CWD for
/// external CLI sessions.
fn build_agent_model_cascade(
    menu: PopupMenu,
    kind: crate::external_session::SessionKind,
    agent_id: &'static str,
    sidebar: &WeakEntity<Sidebar>,
    window: &mut Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let mut providers: Vec<(String, Vec<agent::language_model::AnyLanguageModel>)> = Vec::new();
    for m in registry::global().models() {
        if !m.visible_agents().iter().any(|a| a == agent_id) {
            continue;
        }
        let prov = m.provider_name().to_string();
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
        menu = menu.label(i18n::t("external-wizard-no-model"));
        return menu;
    }
    for (prov_name, models) in providers {
        let sidebar = sidebar.clone();
        menu = menu.submenu(prov_name, window, cx, move |submenu, _window, _cx| {
            let mut submenu = submenu;
            for m in &models {
                let model_id = m.name().to_string();
                let model_name = m.name().to_string();
                let prov = m.provider_name().to_string();
                let sidebar = sidebar.clone();
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
                            cx.emit(SidebarEvent::SpawnExternalSession(
                                kind,
                                prov.clone(),
                                model_id.clone(),
                                project,
                            ));
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

/// Leading icon for a unified sidebar row. Threads use a generic
/// message-square glyph; external agent sessions use their brand SVG (resolved
/// by `ExtrasAssetSource`, tinted via `text_color`).
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
    running: bool,
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
        running: bool,
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
            running,
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
            truncate(display, 24)
        };
        // Tag the row with the cx session id prefix (tracks the .sock file);
        // fall back to the manox-internal id prefix when the cx id was not
        // recoverable (IPC bind failed).
        let short_id: String = if !summary.cx_session_id.is_empty() {
            summary.cx_session_id.chars().take(8).collect()
        } else {
            summary.id.chars().take(8).collect()
        };
        Self {
            id: summary.id.clone(),
            short_id,
            copy_value: summary.copy_identity(),
            title,
            updated: format_relative(summary.created_at),
            pinned: false,
            has_unread: false,
            running: false,
            selected,
            indent,
            icon: RowIcon::External(summary.kind.icon_asset()),
            wash: theme.accent,
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
    let title_color = theme.foreground;
    let wash = item.wash;
    let icon = item.icon.clone();
    let open_kind = item.kind.clone();
    let group = gpui::SharedString::from(format!("thread-row-{id}"));
    let tag_variant = if item.selected || item.running {
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
    let tag_element: AnyElement = if item.running {
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

    let leading_icon = match icon {
        RowIcon::Thread => Icon::new(IconName::Bot)
            .small()
            .text_color(theme.muted_foreground)
            .into_any_element(),
        RowIcon::External(path) => gpui::svg()
            .path(path)
            .size(px(16.))
            .text_color(theme.muted_foreground)
            .into_any_element(),
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
                        .when(item.has_unread, |this| {
                            this.child(
                                gpui::div()
                                    .w(px(8.))
                                    .h(px(8.))
                                    .rounded_full()
                                    .bg(theme.danger),
                            )
                        })
                        .when(item.pinned, |this| {
                            this.child(Icon::new(IconName::Star).xsmall().text_color(theme.accent))
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
                                .child(render_archive_button(
                                    id_archive.clone(),
                                    item.kind.clone(),
                                    cx,
                                )),
                        ),
                ),
        )
        .into_any_element()
}

/// The hover "Inbox" archive button shared by threads and external sessions.
/// Threads toggle their archived flag; an external session tears itself down
/// (kill + drop) — the unified archive semantics. Uses `cx.listener` so the
/// click emits on the sidebar's own context (where `EventEmitter<SidebarEvent>`
/// lives) rather than the bare `App` the standalone `on_click` receives.
fn render_archive_button(id: String, kind: RowKind, cx: &mut Context<Sidebar>) -> AnyElement {
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
        ApprovalMode::OnRequest => theme.success,
        ApprovalMode::AutoReview => theme.info,
        ApprovalMode::Yolo => theme.danger,
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
