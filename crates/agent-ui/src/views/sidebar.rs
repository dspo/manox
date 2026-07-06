//! Conversation history sidebar.
//!
//! A standalone gpui Entity that subscribes to `ThreadStore` and lists past threads. Clicking a
//! conversation entry emits `OpenThread(id)`; the "new conversation" menu item emits `NewThread`;
//! each entry's "×" emits `DeleteThread(id)`. Workspace subscribes to these events.
//!
//! Threads bound to a project (chosen on the first screen) are grouped under a collapsible folder
//! in the "Projects" section, keyed by project path; the rest fall under "Conversations". The top
//! menu and bottom account footer are static decoration mirroring Codex.app's layout.

use std::collections::HashSet;

use agent::{ThreadStore, ThreadStoreEvent, i18n};
use gpui::{
    AnyElement, Context, Entity, EventEmitter, Pixels, Render, SharedString, Subscription, Window,
    prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
    tag::{Tag, TagVariant},
};

/// Events the sidebar emits to the Workspace.
#[derive(Debug, Clone)]
pub enum SidebarEvent {
    OpenThread(String),
    NewThread,
    DeleteThread(String),
}

pub struct Sidebar {
    store: Entity<ThreadStore>,
    selected: Option<String>,
    /// Project paths whose folder group is collapsed; absent means expanded.
    collapsed: HashSet<String>,
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
            if matches!(ev, ThreadStoreEvent::SummariesUpdated) {
                cx.notify();
            }
        });
        Self {
            store,
            selected: None,
            collapsed: HashSet::new(),
            width,
            _sub: sub,
        }
    }

    pub fn store(&self) -> Entity<ThreadStore> {
        self.store.clone()
    }

    /// Update the rendered width. Called by the owning `Workspace` on every
    /// divider drag-move tick; the new value takes effect on the next render.
    pub fn set_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        self.width = width;
        cx.notify();
    }

    /// Mark the currently selected thread id (back-filled by Workspace on switch/new, for highlight).
    pub fn set_selected(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.selected = id;
        cx.notify();
    }

    /// A collapsible project folder: a clickable header (chevron + folder icon +
    /// basename) over its indented conversation rows when expanded.
    fn render_project_group(
        &self,
        path: &str,
        group: &[agent::ThreadSummary],
        selected: Option<&str>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
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
            v_flex()
                .gap_0p5()
                .children(group.iter().enumerate().map(|(ix, s)| {
                    render_thread_item(ix, s, selected == Some(s.id.as_str()), px(16.), theme, cx)
                }))
        });

        v_flex()
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
            // Scrollable body: top menu + projects + conversations.
            .child(
                v_flex()
                    .id("sidebar-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .px_2()
                    .pt(top_inset)
                    .pb_2()
                    .child(
                        v_flex()
                            .gap_0p5()
                            .child(menu_item(
                                "new-thread",
                                IconName::SquareTerminal,
                                i18n::t("sidebar-new-chat"),
                                &theme,
                                Some(cx.listener(|_this, _ev, _window, cx| {
                                    cx.emit(SidebarEvent::NewThread);
                                })),
                            ))
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
                            .child(static_menu_item(
                                "plugins",
                                IconName::Frame,
                                i18n::t("sidebar-plugins"),
                                &theme,
                            )),
                    )
                    .children((!projects.is_empty())
                        .then(|| section_header(i18n::t("sidebar-section-projects"), &theme)))
                    .children(projects.into_iter().map(|(path, group)| {
                        self.render_project_group(&path, &group, selected.as_deref(), &theme, cx)
                    }))
                    .children((!loose.is_empty())
                        .then(|| section_header(i18n::t("sidebar-section-conversations"), &theme)))
                    .child(
                        v_flex()
                            .gap_0p5()
                            .children(loose.iter().enumerate().map(|(ix, s)| {
                                render_thread_item(
                                    ix,
                                    s,
                                    selected.as_deref() == Some(s.id.as_str()),
                                    px(0.),
                                    &theme,
                                    cx,
                                )
                            })),
                    ),
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

/// A grey uppercase-style group header (section labels).
fn section_header(label: SharedString, theme: &Theme) -> AnyElement {
    gpui::div()
        .px_2()
        .pt_3()
        .pb_1()
        .text_xs()
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(theme.muted_foreground)
        .child(label)
        .into_any_element()
}

/// Render one conversation row. `indent` adds left padding so rows nested under
/// a project folder align below its label. Two-row layout: title on top, tag +
/// relative time + delete button on bottom. The thread-id tag uses outline style
/// matching the model-menu wire-api tags.
fn render_thread_item(
    ix: usize,
    summary: &agent::ThreadSummary,
    selected: bool,
    indent: gpui::Pixels,
    theme: &Theme,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    let id = summary.id.clone();
    let id_open = id.clone();
    let id_del = id.clone();
    let title = if summary.summary.is_empty() {
        i18n::t("sidebar-empty-summary").to_string()
    } else {
        truncate(summary.summary.as_str(), 24)
    };
    let updated = format_relative(summary.updated_at);
    let bg = if selected {
        theme.accent.opacity(0.12)
    } else {
        theme.transparent
    };
    let group = gpui::SharedString::from(format!("thread-row-{ix}"));
    let short_id = &summary.id[..summary.id.len().min(8)];
    let tag_variant = if selected {
        TagVariant::Primary
    } else {
        TagVariant::Secondary
    };

    h_flex()
        .id(("thread-item", ix))
        .group(group.clone())
        .w_full()
        .pl(px(8.) + indent)
        .pr_2()
        .py_1()
        .rounded(theme.radius)
        .bg(bg)
        .hover(|s| s.bg(theme.accent.opacity(0.08)))
        .active(|s| s.bg(theme.accent.opacity(0.18)))
        .on_click(cx.listener(move |_this, _ev, _window, cx| {
            cx.emit(SidebarEvent::OpenThread(id_open.clone()));
        }))
        // Two-row layout: title on top, metadata on bottom.
        .child(
            v_flex()
                .gap_0p5()
                .flex_1()
                .min_w_0()
                // Row 1: title (full width, no inline tag clutter).
                .child(
                    gpui::div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(title),
                )
                // Row 2: tag + relative time + delete button.
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(
                            Tag::new()
                                .with_variant(tag_variant)
                                .outline()
                                .small()
                                .child(short_id.to_string()),
                        )
                        // Relative time, hidden while the row is hovered so the delete button can take its place.
                        .child(
                            gpui::div()
                                .flex_shrink_0()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .group_hover(group.clone(), |s| s.invisible())
                                .child(updated),
                        )
                        // Delete button, revealed only on row hover.
                        .child(
                            gpui::div()
                                .invisible()
                                .group_hover(group, |s| s.visible())
                                .child(
                                    Button::new(("del-thread", ix))
                                        .ghost()
                                        .xsmall()
                                        .icon(IconName::Close)
                                        .on_click(cx.listener(move |_this, _ev, _window, cx| {
                                            cx.emit(SidebarEvent::DeleteThread(id_del.clone()));
                                        })),
                                ),
                        ),
                ),
        )
        .into_any_element()
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
