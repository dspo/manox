//! Conversation history sidebar.
//!
//! A standalone gpui Entity that subscribes to `ThreadStore` and lists past threads. Clicking an
//! entry emits `OpenThread(id)`; the "new conversation" button emits `NewThread`; each entry's "×"
//! emits `DeleteThread(id)`. Workspace subscribes to these events.

use agent::{ThreadStore, ThreadStoreEvent};
use gpui::{AnyElement, Context, Entity, EventEmitter, Render, Subscription, Window, prelude::*, px};
use gpui_component::{
    ActiveTheme as _, Sizable as _, button::{Button, ButtonVariants as _}, h_flex, v_flex,
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

        v_flex()
            .h_full()
            .w(px(260.))
            .bg(theme.secondary)
            .border_r_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        Button::new("new-thread")
                            .primary()
                            .small()
                            .w_full()
                            .label("新对话")
                            .on_click(cx.listener(|_this, _ev, _window, cx| {
                                cx.emit(SidebarEvent::NewThread);
                            })),
                    ),
            )
            .child(
                v_flex()
                    .id("threads-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .p_2()
                    .gap_1()
                    .children(summaries.iter().enumerate().map(|(ix, s)| {
                        render_thread_item(ix, s, selected.as_deref() == Some(s.id.as_str()), &theme, cx)
                    })),
            )
    }
}

fn render_thread_item(
    ix: usize,
    summary: &agent::ThreadSummary,
    selected: bool,
    theme: &gpui_component::Theme,
    cx: &mut Context<Sidebar>,
) -> AnyElement {
    let id = summary.id.clone();
    let id_open = id.clone();
    let id_del = id.clone();
    let title = if summary.summary.is_empty() {
        "(新对话)".to_string()
    } else {
        truncate(summary.summary.as_str(), 28)
    };
    let updated = format_timestamp(summary.updated_at);
    let bg = if selected {
        theme.accent.opacity(0.12)
    } else {
        theme.transparent
    };

    h_flex()
        .id(("thread-item", ix))
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
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    gpui::div()
                        .text_sm()
                        .text_color(theme.foreground)
                        .child(title),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(updated),
                ),
        )
        .child(
            Button::new(("del-thread", ix))
                .ghost()
                .small()
                .icon(gpui_component::IconName::Close)
                .on_click(cx.listener(move |_this, _ev, _window, cx| {
                    cx.emit(SidebarEvent::DeleteThread(id_del.clone()));
                })),
        )
        .into_any_element()
}

/// Format epoch seconds as `MM-DD HH:MM` (local timezone).
fn format_timestamp(epoch: i64) -> String {
    let Some(dt) = chrono::DateTime::<chrono::Utc>::from_timestamp(epoch, 0) else {
        return String::new();
    };
    dt.with_timezone(&chrono::Local)
        .format("%m-%d %H:%M")
        .to_string()
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
