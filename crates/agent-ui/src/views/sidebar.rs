//! Conversation history sidebar.
//!
//! A standalone gpui Entity that subscribes to `ThreadStore` and lists past threads. Clicking a
//! conversation entry emits `OpenThread(id)`; the "new conversation" menu item emits `NewThread`;
//! each entry's "×" emits `DeleteThread(id)`. Workspace subscribes to these events.
//!
//! The top menu, "projects" group, and bottom account footer are static decoration that mirrors
//! Codex.app's sidebar layout; only the "new conversation" item and the real conversation list
//! carry behavior.

use agent::{ThreadStore, ThreadStoreEvent};
use gpui::{
    AnyElement, Context, Entity, EventEmitter, Render, Subscription, Window, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

/// Static placeholder projects shown under the "projects" group. These carry no behavior and exist
/// only to mirror Codex.app's layout.
const PLACEHOLDER_PROJECTS: &[(&str, &[(&str, &str)])] = &[
    ("cvat", &[]),
    (
        "floraldet-training",
        &[
            ("本周 main 更新到默认，然后复现…", "4天"),
            ("city 只 claude 和 codex 等 age…", "4天"),
        ],
    ),
    (
        "cx",
        &[
            ("介绍一下本项目？", "1周"),
            ("what model now？", "1周"),
            ("what model now？", "1周"),
        ],
    ),
    ("huaji-skm", &[("本项目全文 health 语义有可…", "2周")]),
];

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
    _sub: Subscription,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Sidebar {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let store = agent::thread_store_global();
        let sub = cx.subscribe(&store, |_this, _store, ev: &ThreadStoreEvent, cx| {
            if matches!(ev, ThreadStoreEvent::SummariesUpdated) {
                cx.notify();
            }
        });
        Self {
            store,
            selected: None,
            _sub: sub,
        }
    }

    pub fn store(&self) -> Entity<ThreadStore> {
        self.store.clone()
    }

    /// Mark the currently selected thread id (back-filled by Workspace on switch/new, for highlight).
    pub fn set_selected(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        self.selected = id;
        cx.notify();
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let summaries = self.store.read(cx).summaries().to_vec();
        let selected = self.selected.clone();

        // Leave room for the macOS traffic-light buttons that float over the transparent titlebar.
        let top_inset = if cfg!(target_os = "macos") {
            px(28.)
        } else {
            px(8.)
        };

        v_flex()
            .h_full()
            .w(px(260.))
            .bg(theme.secondary)
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
                                "新对话",
                                &theme,
                                Some(cx.listener(|_this, _ev, _window, cx| {
                                    cx.emit(SidebarEvent::NewThread);
                                })),
                            ))
                            .child(static_menu_item(IconName::Search, "搜索", &theme))
                            .child(static_menu_item(IconName::Calendar, "已安排", &theme))
                            .child(static_menu_item(IconName::Frame, "插件", &theme)),
                    )
                    .child(section_header("项目", &theme))
                    .child(render_projects(&theme))
                    .child(section_header("对话", &theme))
                    .child(
                        v_flex()
                            .gap_0p5()
                            .children(summaries.iter().enumerate().map(|(ix, s)| {
                                render_thread_item(
                                    ix,
                                    s,
                                    selected.as_deref() == Some(s.id.as_str()),
                                    &theme,
                                    cx,
                                )
                            })),
                    ),
            )
            // Fixed bottom footer: settings + account (static).
            .child(render_footer(&theme))
    }
}

/// A clickable top-level menu row. When `on_click` is `None` the row is static decoration.
fn menu_item(
    id: &'static str,
    icon: IconName,
    label: &'static str,
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

fn static_menu_item(icon: IconName, label: &'static str, theme: &Theme) -> AnyElement {
    menu_item(
        label,
        icon,
        label,
        theme,
        None::<fn(&gpui::ClickEvent, &mut Window, &mut gpui::App)>,
    )
}

/// A grey uppercase-style group header ("项目" / "对话").
fn section_header(label: &'static str, theme: &Theme) -> AnyElement {
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

/// Static projects list with their nested placeholder conversations.
fn render_projects(theme: &Theme) -> AnyElement {
    let mut col = v_flex().gap_0p5();
    for (name, convs) in PLACEHOLDER_PROJECTS {
        col = col.child(
            h_flex()
                .w_full()
                .px_2()
                .py_1p5()
                .gap_2()
                .items_center()
                .rounded(theme.radius)
                .hover(|s| s.bg(theme.accent.opacity(0.08)))
                .child(
                    Icon::new(IconName::Folder)
                        .small()
                        .text_color(theme.muted_foreground),
                )
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(*name),
                ),
        );
        for (title, when) in *convs {
            col = col.child(nested_conversation(title, when, theme));
        }
    }
    col.into_any_element()
}

/// An indented placeholder conversation under a project: title on the left, relative time on the right.
fn nested_conversation(title: &'static str, when: &'static str, theme: &Theme) -> AnyElement {
    h_flex()
        .w_full()
        .pl_8()
        .pr_2()
        .py_1()
        .gap_2()
        .items_center()
        .rounded(theme.radius)
        .hover(|s| s.bg(theme.accent.opacity(0.08)))
        .child(
            gpui::div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(title),
        )
        .child(
            gpui::div()
                .flex_shrink_0()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(when),
        )
        .into_any_element()
}

fn render_thread_item(
    ix: usize,
    summary: &agent::ThreadSummary,
    selected: bool,
    theme: &Theme,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    let id = summary.id.clone();
    let id_open = id.clone();
    let id_del = id.clone();
    let title = if summary.summary.is_empty() {
        "(新对话)".to_string()
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

    h_flex()
        .id(("thread-item", ix))
        .group(group.clone())
        .w_full()
        .px_2()
        .py_1p5()
        .gap_2()
        .items_center()
        .rounded(theme.radius)
        .bg(bg)
        .hover(|s| s.bg(theme.accent.opacity(0.08)))
        .on_click(cx.listener(move |_this, _ev, _window, cx| {
            cx.emit(SidebarEvent::OpenThread(id_open.clone()));
        }))
        .child(
            gpui::div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_sm()
                .text_color(theme.foreground)
                .child(title),
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
                .absolute()
                .right_2()
                .invisible()
                .group_hover(group, |s| s.visible())
                .child(
                    Button::new(("del-thread", ix))
                        .ghost()
                        .small()
                        .icon(IconName::Close)
                        .on_click(cx.listener(move |_this, _ev, _window, cx| {
                            cx.emit(SidebarEvent::DeleteThread(id_del.clone()));
                        })),
                ),
        )
        .into_any_element()
}

/// Static bottom footer: settings row + account row.
fn render_footer(theme: &Theme) -> AnyElement {
    v_flex()
        .w_full()
        .flex_shrink_0()
        .px_2()
        .py_2()
        .gap_0p5()
        .border_t_1()
        .border_color(theme.border)
        .child(static_menu_item(IconName::Settings, "设置", theme))
        .child(
            h_flex()
                .w_full()
                .px_2()
                .py_1p5()
                .gap_2()
                .items_center()
                .rounded(theme.radius)
                .hover(|s| s.bg(theme.accent.opacity(0.08)))
                .child(
                    gpui::div()
                        .size(px(24.))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(theme.accent)
                        .text_color(theme.accent_foreground)
                        .text_xs()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child("账"),
                )
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child("账户"),
                ),
        )
        .into_any_element()
}

/// Format epoch seconds as a coarse relative time (刚刚 / N分钟 / N小时 / N天 / N周).
fn format_relative(epoch: i64) -> String {
    let now = chrono::Local::now().timestamp();
    let diff = (now - epoch).max(0);
    if diff < 60 {
        "刚刚".to_string()
    } else if diff < 3600 {
        format!("{}分钟", diff / 60)
    } else if diff < 86_400 {
        format!("{}小时", diff / 3600)
    } else if diff < 604_800 {
        format!("{}天", diff / 86_400)
    } else {
        format!("{}周", diff / 604_800)
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
