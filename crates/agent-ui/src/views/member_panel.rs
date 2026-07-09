//! Read-only observation panel for a single team worker member.
//!
//! A `MemberPanel` subscribes to its member `Thread`'s `ThreadEvent`s and feeds
//! them into a private [`ConversationState`] — reusing the full
//! [`crate::views::message`] rendering pipeline (agent text, reasoning folds,
//! tool-call cards, peer-message bubbles). The panel has no composer: the
//! leader is the sole input face; members are observed, not driven from here.
//!
//! It also renders a compact slice of the shared [`Team`] task list — this
//! member's owned tasks plus the unassigned pool — so the panel doubles as a
//! per-member work view. The member's lifetime is the team's; if the team
//! disbands (dropping the member `Thread`), the weak handle stops upgrading and
//! the panel renders a "gone" state until its tab is closed.

use std::collections::HashMap;

use agent::language_model::TokenUsage;
use agent::team::{Task, TaskListEvent, Team};
use agent::{Thread, ThreadEvent, i18n};
use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Entity, FontWeight, Pixels, Render, ScrollHandle, SharedString,
    Subscription, WeakEntity, Window, px,
};
use gpui_component::{ActiveTheme as _, ElementExt as _, Theme, h_flex, v_flex};

use crate::Workspace;
use crate::conversation::ConversationState;

/// A read-only panel observing one team worker member's conversation + tasks.
pub struct MemberPanel {
    member: WeakEntity<Thread>,
    member_name: String,
    role: String,
    team: WeakEntity<Team>,
    weak_workspace: WeakEntity<Workspace>,
    conversation: Entity<ConversationState>,
    sub: Option<Subscription>,
    task_sub: Option<Subscription>,
    scroll_handle: ScrollHandle,
    stick_to_bottom: bool,
}

impl MemberPanel {
    /// Construct a panel for `member`, backfilling its conversation from the
    /// member's current messages and subscribing to subsequent events. The
    /// `team` weak handle reaches the shared task list for the board.
    pub fn new(
        member: Entity<Thread>,
        member_name: String,
        role: String,
        team: WeakEntity<Team>,
        weak_workspace: WeakEntity<Workspace>,
        cx: &mut App,
    ) -> Entity<Self> {
        let messages = member.read(cx).messages().to_vec();
        let empty_usage: HashMap<String, TokenUsage> = HashMap::new();
        let weak_ws = weak_workspace.clone();
        let conversation = cx.new(|cx| {
            ConversationState::rebuild_from_messages(&messages, &empty_usage, &role, weak_ws, cx)
        });

        let panel = cx.new(|_| Self {
            member: member.downgrade(),
            member_name,
            role,
            team,
            weak_workspace,
            conversation,
            sub: None,
            task_sub: None,
            scroll_handle: ScrollHandle::new(),
            stick_to_bottom: true,
        });

        // Subscriptions need the panel entity's own context (the handler's
        // first arg is `&mut Self`). Install them after construction.
        panel.update(cx, |this, cx| {
            let member_sub = cx.subscribe(&member, move |this, _m, ev: &ThreadEvent, cx| {
                let role = this.role.clone();
                let weak = this.weak_workspace.clone();
                this.conversation
                    .update(cx, |c, cx| c.apply(ev, &role, None, weak, cx));
                // New content arrives: resume tail-follow unless the user has
                // scrolled up to inspect history.
                this.stick_to_bottom = true;
                cx.notify();
            });
            this.sub = Some(member_sub);

            let team_weak = this.team.clone();
            if let Some(team_ent) = team_weak.upgrade() {
                let tasks = team_ent.read(cx).tasks().clone();
                let task_sub = cx.subscribe(&tasks, |_this, _t, _ev: &TaskListEvent, cx| {
                    cx.notify();
                });
                this.task_sub = Some(task_sub);
            }
        });

        panel
    }

    pub fn member_name(&self) -> &str {
        &self.member_name
    }

    /// Whether the member `Thread` is still alive (team not disbanded).
    fn alive(&self) -> bool {
        self.member.upgrade().is_some()
    }

    fn render_header(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let alive = self.alive();
        let running = self
            .member
            .upgrade()
            .map(|m| m.read(cx).is_running())
            .unwrap_or(false);
        let status_key = if !alive {
            "member-disbanded"
        } else if running {
            "member-running"
        } else {
            "member-idle"
        };
        let status = i18n::t(status_key);
        let dot_color = if !alive {
            theme.muted_foreground
        } else if running {
            theme.accent
        } else {
            theme.muted_foreground
        };
        h_flex()
            .w_full()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .gap_2()
            .items_center()
            .child(gpui::div().w(px(8.)).h(px(8.)).rounded_full().bg(dot_color))
            .child(
                gpui::div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(self.member_name.clone()),
            )
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(status),
            )
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("· {}", self.role)),
            )
    }

    fn render_task_board(&self, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let tasks: Vec<Task> = self
            .team
            .upgrade()
            .map(|t| t.read(cx).tasks().read(cx).tasks().to_vec())
            .unwrap_or_default();
        let mine: Vec<&Task> = tasks
            .iter()
            .filter(|t| t.owner.as_deref() == Some(self.member_name.as_str()))
            .collect();
        let unassigned: Vec<&Task> = tasks.iter().filter(|t| t.owner.is_none()).collect();

        let row = |t: &Task| {
            h_flex()
                .w_full()
                .py_1()
                .gap_2()
                .items_center()
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t.id.clone()),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.accent)
                        .child(format!("[{}]", t.status)),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(t.subject.clone()),
                )
        };

        let section = |label: SharedString, items: &[&Task]| {
            v_flex()
                .w_full()
                .child(
                    gpui::div()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .children(items.iter().map(|t| row(t)))
        };

        let body = if mine.is_empty() && unassigned.is_empty() {
            v_flex().w_full().child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(i18n::t("member-no-tasks")),
            )
        } else {
            v_flex()
                .w_full()
                .gap_1()
                .when(!mine.is_empty(), |this| {
                    this.child(section(i18n::t("member-tasks-mine"), &mine))
                })
                .when(!unassigned.is_empty(), |this| {
                    this.child(section(i18n::t("member-tasks-unassigned"), &unassigned))
                })
        };

        v_flex()
            .w_full()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme.border)
            .gap_1()
            .child(
                gpui::div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.muted_foreground)
                    .child(i18n::t("member-tasks")),
            )
            .child(body)
    }
}

impl Render for MemberPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let alive = self.alive();

        let conv = self.conversation.clone();
        let items: Vec<AnyElement> = conv
            .read(cx)
            .items()
            .iter()
            .cloned()
            .map(|item| {
                v_flex()
                    .pt_1()
                    .pb_4()
                    .flex_shrink_0()
                    .min_w_0()
                    .child(item)
                    .into_any_element()
            })
            .collect();

        let scroll = self.scroll_handle.clone();
        let weak = cx.weak_entity();

        let mut col = v_flex()
            .h_full()
            .w_full()
            .bg(theme.background)
            .text_color(theme.foreground);
        col = col.child(self.render_header(&theme, cx));
        if alive {
            col = col.child(self.render_task_board(&theme, cx));
        }
        col = col.child(
            v_flex()
                .id("member-msg-scroll")
                .w_full()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .overflow_y_scroll()
                .overflow_x_hidden()
                .track_scroll(&scroll)
                .children(items)
                .on_prepaint(move |_bounds, _window, cx| {
                    let Some(this) = weak.upgrade() else {
                        return;
                    };
                    if !this.read(cx).stick_to_bottom {
                        return;
                    }
                    let off = scroll.offset().y;
                    let max = scroll.max_offset().y;
                    if (max + off).abs() < px(1.) {
                        return;
                    }
                    scroll.scroll_to_bottom();
                })
                .on_scroll_wheel(cx.listener(
                    |this, ev: &gpui::ScrollWheelEvent, window, cx| {
                        // An upward wheel breaks tail-follow so the user can
                        // scroll back through history; re-arm by scrolling back
                        // to the bottom.
                        let dy = ev.delta.pixel_delta(window.line_height()).y;
                        if dy > Pixels::ZERO {
                            this.stick_to_bottom = false;
                            cx.notify();
                        }
                    },
                )),
        );
        col
    }
}
