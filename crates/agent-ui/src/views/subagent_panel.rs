//! Sub-agent observation panel — renders the child run's header, live
//! activity (tool lifecycle + text deltas), and final answer.

use agent::{ToolCallStatus, thread::SubagentChildEvent};
use gpui::{Context, Entity, Render, SharedString, WeakEntity, Window, prelude::*};
use gpui_component::{ActiveTheme as _, Theme, h_flex, v_flex};

use crate::Workspace;

pub struct SubagentPanel {
    title: SharedString,
    role: String,
    status: ToolCallStatus,
    activity: Vec<SharedString>,
    final_text: Option<SharedString>,
    #[allow(dead_code)]
    weak_workspace: WeakEntity<Workspace>,
}

impl SubagentPanel {
    pub fn new(
        title: String,
        role: String,
        status: ToolCallStatus,
        backfill: &[SubagentChildEvent],
        final_text: Option<String>,
        weak_workspace: WeakEntity<Workspace>,
        cx: &mut gpui::App,
    ) -> Entity<Self> {
        let activity: Vec<SharedString> = backfill.iter().filter_map(activity_line).collect();
        cx.new(|_| Self {
            title: SharedString::from(title),
            role,
            status,
            activity,
            final_text: final_text.map(SharedString::from),
            weak_workspace,
        })
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn push(&mut self, child: &SubagentChildEvent, cx: &mut Context<Self>) {
        if let Some(line) = activity_line(child) {
            self.activity.push(line);
        }
        cx.notify();
    }

    pub fn set_status(&mut self, status: ToolCallStatus, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    /// Show the final answer (set when a finished run's panel is opened late).
    pub fn set_final_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.final_text = Some(SharedString::from(text));
        cx.notify();
    }
}

/// One human-readable activity line from a child event.
fn activity_line(child: &SubagentChildEvent) -> Option<SharedString> {
    match child {
        SubagentChildEvent::ToolStart { name, hint, .. } => Some(
            match hint {
                Some((key, value)) => format!("▸ {name} {key}={value}"),
                None => format!("▸ {name}"),
            }
            .into(),
        ),
        SubagentChildEvent::ToolEnd { is_error, .. } => Some(
            (if *is_error {
                "✗ tool failed"
            } else {
                "✓ tool done"
            })
            .into(),
        ),
        SubagentChildEvent::Text(text) => {
            if text.trim().is_empty() {
                None
            } else {
                Some(SharedString::from(text.clone()))
            }
        }
        SubagentChildEvent::Thinking(_) => None,
    }
}

impl Render for SubagentPanel {
    fn render(&mut self, _w: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme: Theme = cx.theme().clone();
        let muted = theme.muted_foreground;

        let status_text: SharedString = match self.status {
            ToolCallStatus::Running | ToolCallStatus::PendingApproval => "running".into(),
            ToolCallStatus::Success | ToolCallStatus::Continued => "done".into(),
            ToolCallStatus::Cancelled => "cancelled".into(),
            ToolCallStatus::Error | ToolCallStatus::Denied => "failed".into(),
        };

        let mut body = v_flex()
            .id("subagent-panel-scroll")
            .h_full()
            .w_full()
            .p_3()
            .gap_2()
            .overflow_y_scroll()
            .bg(theme.background);

        // Header: role · title · status
        body = body.child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    gpui::div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(format!("[{}] {}", self.role, self.title)),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(match self.status {
                            ToolCallStatus::Error | ToolCallStatus::Denied => theme.danger,
                            ToolCallStatus::Success | ToolCallStatus::Continued => theme.success,
                            _ => muted,
                        })
                        .child(status_text),
                ),
        );

        // Activity lines.
        for line in &self.activity {
            body = body.child(
                gpui::div()
                    .text_xs()
                    .font_family(theme.mono_font_family.clone())
                    .text_color(muted)
                    .child(line.clone()),
            );
        }

        // Final answer block.
        if let Some(final_text) = &self.final_text {
            body = body.child(
                gpui::div()
                    .pt_2()
                    .text_sm()
                    .text_color(theme.foreground)
                    .child(final_text.clone()),
            );
        }

        body
    }
}
