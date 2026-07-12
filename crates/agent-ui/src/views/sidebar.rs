//! Conversation history sidebar.
//!
//! A standalone gpui Entity that subscribes to `ThreadStore` and lists past threads. Clicking a
//! conversation entry emits `OpenThread(id)`; the "+" button on each project folder header emits
//! `NewThreadWithProject(path)`; the "+" button on the "Conversations" section emits `NewThread`.
//! Workspace subscribes to these events.
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
    EventEmitter, Pixels, Render, SharedString, Subscription, WeakEntity, Window, ease_in_out,
    prelude::*, px,
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

/// Events the sidebar emits to the Workspace.
#[derive(Debug, Clone)]
pub enum SidebarEvent {
    OpenThread(String),
    NewThread,
    /// New thread bound to a specific project path.
    NewThreadWithProject(PathBuf),
    OpenPlugins,
    /// User clicked archive/unarchive. The bool is the new archived state.
    ArchiveThread(String, bool),
    /// Launch an external agent CLI session with a user-picked provider + model
    /// (the cascade wizard's terminal action). The kind identifies the agent
    /// (`claude` / `codex` / `copilot`); the two strings are provider name +
    /// model id.
    SpawnExternalSession(crate::external_session::SessionKind, String, String),
    /// Switch the main area to an already-running external session.
    OpenExternalSession(String),
    /// Close an external session (× on the row): kill the agent and drop it
    /// from the sidebar.
    CloseExternalSession(String),
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
    /// list. Rendered in a dedicated "External" section; an `external:` id in
    /// `selected` highlights the active one.
    external_sessions: Vec<crate::external_session::ExternalSessionSummary>,
    /// Whether the new-session `PopupMenu` (Manox Thread / Claude Code / Codex /
    /// GitHub Copilot) is open off the "Conversations" header `+`.
    new_session_open: bool,
    new_session_menu: Option<Entity<PopupMenu>>,
    new_session_menu_sub: Option<Subscription>,
    /// Live width driven by dragging the divider on the right edge. Updated
    /// from the owning `Workspace` on every drag-move tick.
    width: Pixels,
    _sub: Subscription,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Sidebar {
    pub fn new(width: Pixels, cx: &mut Context<Self>) -> Self {
        let store = agent::thread_store_global();
        let sub = cx.subscribe(&store, |_this, _store, ev: &ThreadStoreEvent, cx| {
            // `SummariesUpdated` re-renders the whole list (a thread was
            // created/saved/deleted). `RunningChanged` flips the running
            // indicator on affected rows; `cx.notify()` is enough — the
            // per-row `running` bool is recomputed in `render` from the store's
            // `is_running` and the shimmer animation starts/stops accordingly.
            match ev {
                ThreadStoreEvent::SummariesUpdated | ThreadStoreEvent::RunningChanged => {
                    cx.notify();
                }
            }
        });
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

    /// Open the new-session `PopupMenu` off the "Conversations" header `+`.
    /// One flat row (Manox Thread → `NewThread`) plus one submenu per external
    /// agent kind. Each agent submenu is a provider→model cascade built from
    /// `registry::global().models()` filtered by the agent's id; picking a
    /// model emits `SpawnExternalSession(kind, provider, model)`. An agent with
    /// no supporting model renders a muted "no model" label row.
    fn open_new_session_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                        this.close_new_session_menu();
                        cx.emit(SidebarEvent::NewThread);
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
                let icon = match kind {
                    crate::external_session::SessionKind::ClaudeCode => IconName::Bot,
                    crate::external_session::SessionKind::Codex => IconName::Cpu,
                    crate::external_session::SessionKind::GithubCopilot => IconName::Github,
                };
                let label = kind.label();
                let agent_id = kind.agent_id();
                menu = menu.submenu_with_icon(
                    Some(Icon::new(icon).small().text_color(theme.muted_foreground)),
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
    }

    /// Mark the currently selected thread id (back-filled by Workspace on switch/new, for highlight).
    pub fn set_selected(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        if self.selected == id {
            return;
        }
        // The outgoing selection becomes the previous one so its row can play
        // the fade-out half of the slide; bump the generation so both rows'
        // wash animations retrigger.
        self.prev_selected = self.selected.take();
        self.selected = id;
        self.select_gen = self.select_gen.wrapping_add(1);
        cx.notify();
    }

    /// A collapsible project folder: a clickable header (chevron + folder icon +
    /// basename) over its indented conversation rows when expanded.
    fn render_project_group(
        &self,
        path: &str,
        group: &[agent::ThreadSummary],
        selected: Option<&str>,
        store: &Entity<ThreadStore>,
        slide: &SlideCtx,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Owned clone so the row-mapping closure can borrow `theme` without
        // holding an immutable borrow of `*cx` (which would clash with the
        // `&mut cx` the closure also passes into each row). Cheap: Theme's
        // heavy fields are Arc/Rc refcount bumps.
        let theme = cx.theme().clone();
        let expanded = !self.collapsed.contains(path);
        let name = std::path::Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(path)
            .to_string();
        let key: SharedString = path.to_string().into();

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
                Button::new(format!("new-thread-in-project-{key}"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .tooltip(i18n::t("sidebar-new-chat"))
                    .on_click(cx.listener({
                        let path = path.to_string();
                        move |_this, _ev, _window, cx| {
                            cx.stop_propagation();
                            cx.emit(SidebarEvent::NewThreadWithProject(PathBuf::from(
                                path.clone(),
                            )));
                        }
                    })),
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
            // `w_full` keeps this v_flex stretching to the project group's
            // width; without it the v_flex collapses to its content's max
            // width, and at narrow sidebar widths the inner title div gets
            // 0 px of horizontal space — `overflow_hidden` then clips the
            // title text completely, so the row only renders the tag/tokens
            // strip below.
            v_flex().w_full().gap_0p5().children(group.iter().map(|s| {
                render_thread_item(
                    s,
                    selected == Some(s.id.as_str()),
                    store.read(cx).is_running(&s.id),
                    px(16.),
                    slide,
                    &theme,
                    cx,
                )
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

        // Partition into project-bound groups (keyed by project path, first-seen
        // order preserved) and the projectless remainder. `summaries` is already
        // newest-first, so both keep that ordering.
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

        // Build the flat, render-order list of visible thread ids to derive the
        // slide direction between the previous and the new selection. Collapsed
        // project groups contribute nothing; their rows aren't on screen.
        let mut flat_ids: Vec<String> = Vec::new();
        for (path, group) in &projects {
            if !self.collapsed.contains(path.as_str()) {
                flat_ids.extend(group.iter().map(|s| s.id.clone()));
            }
        }
        flat_ids.extend(loose.iter().map(|s| s.id.clone()));
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

        // Leave room for the macOS traffic-light buttons that float over the transparent titlebar.
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
            // Scrollable body: top menu + projects + conversations. The body
            // owns the macOS traffic-light inset now that the top tab switcher
            // is gone.
            .child(
                v_flex()
                    .id("sidebar-body")
                    .flex_1()
                    .w_full()
                    // `min_h_0` lets the body shrink below its content height so
                    // `overflow_y_scroll` actually engages; without it the flex
                    // item's min-height defaults to content and the list grows
                    // past the viewport instead of scrolling.
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
                            ))
                            .child(menu_item(
                                "plugins",
                                IconName::Frame,
                                i18n::t("sidebar-plugins"),
                                &theme,
                                Some(cx.listener(|_this, _ev, _window, cx| {
                                    cx.emit(SidebarEvent::OpenPlugins);
                                })),
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
                                                this.open_new_session_menu(window, cx);
                                            }
                                            cx.notify();
                                        })),
                                )
                                .children(self.new_session_menu.clone().map(|menu| {
                                    gpui::div()
                                        .id("new-session-dropdown")
                                        .absolute()
                                        .top_full()
                                        .right_0()
                                        .occlude()
                                        .child(menu)
                                        .into_any_element()
                                }))
                                .into_any_element(),
                        ),
                    ))
                    .child(v_flex().w_full().gap_0p5().children(loose.iter().map(|s| {
                        render_thread_item(
                            s,
                            selected.as_deref() == Some(s.id.as_str()),
                            store.read(cx).is_running(&s.id),
                            px(0.),
                            &slide,
                            &theme,
                            cx,
                        )
                    })))
                    .children(
                        (!self.external_sessions.is_empty()).then(|| {
                            section_header(i18n::t("sidebar-section-external"), &theme, None)
                        }),
                    )
                    .child(v_flex().w_full().gap_0p5().children(
                        self.external_sessions.iter().map(|s| {
                            render_external_session_item(
                                s,
                                selected.as_deref() == Some(s.id.as_str()),
                                &theme,
                                cx,
                            )
                        }),
                    )),
            )
    }
}

/// A clickable top-level menu row. When `on_click` is `None` the row is static decoration.
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

/// A group header (section label). Text size matches the menu items and
/// thread titles below it so the section reads as a peer of its children
/// rather than a smaller-label category. `action` is an optional trailing
/// element (e.g. a "+" button) rendered to the right of the label.
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

/// One row of the new-session `PopupMenu`: icon + label. Brand names
/// (`kind.label()`) are passed through `i18n::t` which falls back to the
/// literal string when no fluent key matches, so "Claude Code" etc. render
/// verbatim without needing a translation entry.
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
/// submenu. Picking a model emits `SpawnExternalSession(kind, provider, model)`
/// to the sidebar; the top-level menu's `DismissEvent` closes the popup stack.
/// An agent with no supporting model renders a muted "no model" label instead
/// of empty submenus, so the dogfooder sees what to configure.
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
                let model_id = m.id().to_string();
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
                        let _ = sidebar.update(cx, |_this, cx| {
                            cx.emit(SidebarEvent::SpawnExternalSession(
                                kind,
                                prov.clone(),
                                model_id.clone(),
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

/// Render one external-session row in the "External" section. Simpler than a
/// thread row — no slide-wash (external sessions live in their own section and
/// don't participate in the conversation selection slide), just an icon + kind
/// label + muted provider/model subtitle + a trailing `×` that emits
/// `CloseExternalSession(id)`. Clicking the row emits `OpenExternalSession(id)`.
fn render_external_session_item(
    summary: &crate::external_session::ExternalSessionSummary,
    selected: bool,
    theme: &Theme,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    let id = summary.id.clone();
    let id_open = id.clone();
    let id_close = id.clone();
    let kind = summary.kind;
    let icon = match kind {
        crate::external_session::SessionKind::ClaudeCode => IconName::Bot,
        crate::external_session::SessionKind::Codex => IconName::Cpu,
        crate::external_session::SessionKind::GithubCopilot => IconName::Github,
    };
    let subtitle = format!("{} · {}", summary.provider_name, summary.model_id);
    let mut row = h_flex()
        .id(format!("external-row-{id}"))
        .w_full()
        .px_2()
        .py_1p5()
        .gap_2()
        .items_center()
        .rounded(theme.radius)
        .cursor_pointer()
        .hover(|s| s.bg(theme.accent.opacity(0.06)))
        .on_click(cx.listener(move |_this, _ev, _window, cx| {
            cx.emit(SidebarEvent::OpenExternalSession(id_open.clone()));
        }));
    if selected {
        row = row.bg(theme.accent.opacity(0.10));
    }
    row = row
        .child(Icon::new(icon).small().text_color(theme.muted_foreground))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    gpui::div()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(kind.label()),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(subtitle),
                ),
        )
        .child(
            Button::new(format!("external-close-{id}"))
                .ghost()
                .xsmall()
                .compact()
                .icon(IconName::Close)
                .tooltip(i18n::t("sidebar-close-external"))
                .on_click(cx.listener(move |_this, _ev, _window, cx| {
                    cx.stop_propagation();
                    cx.emit(SidebarEvent::CloseExternalSession(id_close.clone()));
                })),
        );
    row.into_any_element()
}

/// Render one conversation row. `indent` adds left padding so rows nested under
/// a project folder align below its label. Two-row layout: title on top, tag +
/// total tokens + relative time + archive action on bottom. The
/// thread-id tag is clickable to copy the full thread id (F1); while `running`
/// is true a highlight band sweeps across the tag (F2).
///
/// Element ids and the hover `group` key are derived from the thread's UUID, not
/// a per-list enumerate index — the project groups and the loose list each
/// enumerate from 0, so an index-keyed id collided across groups and routed
/// clicks to the wrong row. The UUID is unique by construction.
fn render_thread_item(
    summary: &agent::ThreadSummary,
    selected: bool,
    running: bool,
    indent: gpui::Pixels,
    slide: &SlideCtx,
    theme: &Theme,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    let id = summary.id.clone();
    let id_open = id.clone();
    let id_archive = id.clone();
    let id_copy = id.clone();
    let archive_to = !summary.archived;
    let display = summary.display_title();
    let title = if display.is_empty() {
        i18n::t("sidebar-empty-summary").to_string()
    } else {
        truncate(display, 24)
    };
    let updated = format_relative(summary.interacted_at);
    let tokens = format_tokens(summary.cumulative_total_tokens);
    // The active row's surface is painted by the sliding wash overlay below,
    // not by a static row background. Keeping the row itself transparent lets
    // the wash cross-fade between the deselecting and selecting rows without a
    // double-fill, and leaves hover/active free to tint only non-selected rows.
    let role = if slide.selecting_id.as_deref() == Some(id.as_str()) {
        AnimRole::Selecting
    } else if slide.deselecting_id.as_deref() == Some(id.as_str()) {
        AnimRole::Deselecting
    } else {
        AnimRole::None
    };
    // dir_sign orients the slide along the selection's travel direction: Down
    // (new row below the old) moves the wash downward, Up moves it upward. None
    // (initial load, no prior row) zeroes the translation so the wash simply
    // fades in place.
    let dir_sign: f32 = match slide.dir {
        SlideDir::Down => 1.0,
        SlideDir::Up => -1.0,
        SlideDir::None => 0.0,
    };
    let wash = approval_mode_color(summary.approval_mode, theme);
    let slide_gen = slide.gen_id;
    // The wash overlay is attached only to rows participating in the transition.
    // Idle rows (AnimRole::None) carry no overlay and pay no animation cost.
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
                        // Selecting: wash fades in (0->1) while settling into
                        // place from the prior row's direction. Deselecting:
                        // wash fades out (1->0) while continuing past this row
                        // off the opposite edge.
                        let (opacity, ty) = if anim_role == AnimRole::Selecting {
                            (t, -dir_sign * SELECT_SLIDE_PX * (1.0 - t))
                        } else {
                            (1.0 - t, dir_sign * SELECT_SLIDE_PX * t)
                        };
                        // top+bottom shifted by equal-and-opposite offsets moves
                        // the rect without resizing it; overflow_hidden on the
                        // row clips the wash as it enters and exits.
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
    // Selected title stays foreground (strong, full contrast) — the wash alone
    // carries the active signal; tinting the title accent-on-accent crushed
    // contrast and made the active row read as disabled.
    let title_color = theme.foreground;
    let group = gpui::SharedString::from(format!("thread-row-{id}"));
    // Short thread ID: first 8 chars of the UUID. Char-based so a non-ASCII
    // id (defensive — ids are hex today) cannot panic on a char boundary.
    let short_id: String = summary.id.chars().take(8).collect();
    // Running threads share the selected (Primary) tag tint so the indicator
    // reads as "active" even before the sweep animation paints.
    let tag_variant = if selected || running {
        TagVariant::Primary
    } else {
        TagVariant::Secondary
    };

    // F1: the tag sits inside a ghost Button so gpui-component's managed
    // tooltip (only exposed on its own components) is available, and the
    // click copies the full thread id. `stop_propagation` keeps the click
    // from bubbling into the row's `OpenThread` handler. The Tag remains the
    // visual; the Button contributes no box of its own in ghost mode.
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

    // F2: a relative+overflow-hidden wrapper around the tag so a sweeping
    // highlight band — clipped to the wrapper — reads as light passing over
    // the tag while the turn is live. `Animation::repeat` loops forever; the
    // band is only attached when `running`, so idle rows pay no animation cost.
    let tag_wrapper = gpui::div().relative().overflow_hidden().child(tag_button);
    let tag_element: AnyElement = if running {
        // `accent` is `Copy` (`Hsla`); lift it out of the `&Theme` borrow so the
        // `'static` animator closure (which rebuilds the band each frame) can
        // own a copy instead of borrowing `theme` past the function body.
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

    h_flex()
        .id(format!("thread-item-{id}"))
        .group(group.clone())
        .w_full()
        .relative()
        .overflow_hidden()
        .pl(px(8.) + indent)
        .pr_2()
        .py_1()
        .rounded(theme.radius)
        .when(!selected, |this| {
            this.hover(move |s| s.bg(wash.opacity(0.08)))
                .active(move |s| s.bg(wash.opacity(0.18)))
        })
        .on_click(cx.listener(move |_this, _ev, _window, cx| {
            cx.emit(SidebarEvent::OpenThread(id_open.clone()));
        }))
        // The wash sits behind the content (painted first, clipped to the row)
        // so the selection surface slides between rows without covering text.
        .when_some(wash_overlay, |this, overlay| this.child(overlay))
        // Two-row layout: title on top, metadata on bottom. `gap_1` separates
        // the two rows clearly so a multi-line title doesn't visually run into
        // the tag/token row below.
        .child(
            v_flex()
                .w_full()
                .gap_1()
                .flex_1()
                .min_w_0()
                // Row 1: title (full width, no inline tag clutter). A small
                // pin star sits inline when the thread is pinned, so the
                // floating-to-top ordering has a visible marker.
                .child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .items_center()
                        .min_w_0()
                        .when(summary.has_unread, |this| {
                            this.child(
                                gpui::div()
                                    .w(px(8.))
                                    .h(px(8.))
                                    .rounded_full()
                                    .bg(theme.danger),
                            )
                        })
                        .when(summary.pinned, |this| {
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
                // Row 2: tag + tokens + relative time, with the archive
                // action taking their place on hover.
                .child(
                    h_flex()
                        .w_full()
                        .gap_1()
                        .items_center()
                        .child(tag_element)
                        // Tokens + relative time, hidden on hover so the action
                        // button can take their place. `min_w_0` + overflow
                        // hidden so a narrow sidebar clips rather than overflows.
                        .child(
                            h_flex()
                                .gap_1()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .group_hover(group.clone(), |s| s.invisible())
                                .child(gpui::div().child(tokens))
                                .child(gpui::div().child(updated)),
                        )
                        // Archive action, revealed on hover.
                        .child(
                            h_flex()
                                .gap_0p5()
                                .invisible()
                                .group_hover(group.clone(), |s| s.visible())
                                .child(
                                    Button::new(format!("archive-thread-{id_archive}"))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Inbox)
                                        .on_click(cx.listener(move |_this, _ev, _window, cx| {
                                            cx.stop_propagation();
                                            cx.emit(SidebarEvent::ArchiveThread(
                                                id_archive.clone(),
                                                archive_to,
                                            ));
                                        })),
                                ),
                        ),
                ),
        )
        .into_any_element()
}

/// Compact token count: 1234 -> "1.2k", 1234567 -> "1.2M".
fn format_tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

fn approval_mode_color(mode: i64, theme: &Theme) -> gpui::Hsla {
    match ApprovalMode::from_i64(mode) {
        ApprovalMode::OnRequest => theme.success,
        ApprovalMode::AutoReview => theme.info,
        ApprovalMode::Yolo => theme.danger,
    }
}

/// Format epoch seconds as a coarse relative time, locale-aware via fluent
/// plural rules (en distinguishes one/other; zh-CN has no plural distinction).
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
