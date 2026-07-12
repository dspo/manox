//! Right-hand context rail: a stable sidecar showing the active thread's
//! environment/cockpit information (run status, changes, branch, per-model
//! token usage, context budget, plan milestones, sources).
//!
//! The rail is a first-class view owned by [`crate::Workspace`]. It holds the
//! cockpit state (run phase, milestones, per-cell counter animation state)
//! that used to live directly on `Workspace`, plus strong handles to the
//! active [`agent::Thread`] and [`crate::ConversationState`] it renders
//! against. Writes to cockpit state flow through `Workspace` →
//! `self.context_rail.update(cx, |r, cx| …)`.
//!
//! Layout: a fixed-width (responsive, collapsible) column with a scrollable
//! inner panel. Unlike the old floating env card this is a normal flex child
//! of the top-level three-column layout, so it never overlaps the conversation
//! and the composer never spans underneath it.

use std::collections::HashMap;
use std::time::Duration;

use agent::compact::MIN_COMPACTION_CONTEXT_WINDOW;
use agent::{Thread, ThreadEvent, i18n};
use gpui::{
    Animation, AnimationExt as _, AnyElement, Context, Entity, Render, SharedString, Window,
    ease_out_quint, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::cockpit::{CockpitPhase, Milestone, MilestoneStatus, context_budget_pct};

// ── Geometry ─────────────────────────────────────────────────────────────

/// Desktop rail width. Compact enough for the per-model tree block: model id
/// on the top line, `├── Throughput` / `└── Cache` tree rows underneath, each
/// with `↑↓` animated counters.
pub(crate) const RAIL_DESKTOP_WIDTH: f32 = 300.;
/// Narrowed rail width once the window drops below [`RAIL_NARROW_BREAK`].
/// Keeps the rail visible while leaving the conversation more room.
const RAIL_NARROW_WIDTH: f32 = 280.;
/// Below this main-column width the rail folds into a drawer and the
/// conversation column takes the full body. Matches the old env-card gate so
/// a narrow window never crowds the conversation.
const RAIL_NARROW_BREAK: f32 = 900.;

/// Longest model id rendered in full, calibrated to "MiniMax/MiniMax-M3[1m]"
/// (22 chars). Longer ids are cut to this width then trimmed by 3 and given a
/// "..." suffix, so the result stays one line at `ENV_MODEL_ID_MAX` chars
/// (e.g. "MiniMax/MiniMax-M3[...").
const ENV_MODEL_ID_MAX: usize = 22;

// ── ContextRail view ──────────────────────────────────────────────────────

/// Right-side context sidecar. Owns the cockpit state (run phase, milestones,
/// per-cell counter animation state) and renders the environment/cockpit
/// panel that used to float as an absolute card over the conversation.
pub(crate) struct ContextRail {
    pub(crate) thread: Entity<Thread>,
    /// Coarse run phase shown in the status row. Derived from `ThreadEvent`s
    /// routed here by `Workspace`; the per-second thinking ticker's
    /// `cx.notify()` also refreshes the elapsed display.
    pub(crate) cockpit_phase: CockpitPhase,
    /// Title of the most recently running tool, surfaced in the status row
    /// when the phase is `RunningTool`. Cleared on terminal stop.
    pub(crate) cockpit_running_tool_title: Option<String>,
    /// Plan steps parsed from the approved `exit_plan_mode` plan. All
    /// `Pending` outside a turn; the first is promoted to `InProgress` while
    /// the thread runs, demoted back to `Pending` on terminal stop.
    pub(crate) cockpit_milestones: Vec<Milestone>,
    /// Whether the milestone section is collapsed (`ToggleCockpitTasks` /
    /// ctrl+t toggles). Hidden still renders the run-status row.
    pub(crate) cockpit_hide_tasks: bool,
    /// Cached `settings.auto_compact.{enabled,threshold}`, refreshed on
    /// construction and when the user exits the Settings overlay. Avoids a
    /// per-frame file read in the context-budget render.
    pub(crate) cockpit_auto_compact_enabled: bool,
    pub(crate) cockpit_auto_compact_threshold: f64,
    /// Per-cell scoreboard state, keyed by `"{model_name}|{field}"` where
    /// `field ∈ {in, out, cache_create, cache_read}`. Stored as
    /// `(gen, last_value)`: `gen` is bumped on every value delta so the
    /// animation's element id changes and gpui fires a fresh 0→1 tween;
    /// `last_value` is the value the previous render committed and becomes
    /// the start of the count-up interpolation. Rebuilt every render so
    /// cells whose model disappeared (e.g. thread reset) are pruned, keeping
    /// the map bounded by the number of live models.
    pub(crate) env_counter_state: HashMap<String, (u64, u64)>,
}

impl ContextRail {
    pub(crate) fn new(
        thread: Entity<Thread>,
        auto_compact_enabled: bool,
        auto_compact_threshold: f64,
    ) -> Self {
        Self {
            thread,
            cockpit_phase: CockpitPhase::Idle,
            cockpit_running_tool_title: None,
            cockpit_milestones: Vec::new(),
            cockpit_hide_tasks: false,
            cockpit_auto_compact_enabled: auto_compact_enabled,
            cockpit_auto_compact_threshold: auto_compact_threshold,
            env_counter_state: HashMap::new(),
        }
    }

    /// Width the rail column takes on the top-level h_flex at the given
    /// main-column body width. `None` means the window is too narrow: the rail
    /// folds into a drawer and the conversation column takes the full body.
    pub(crate) fn rail_width_for(main_body_w: gpui::Pixels) -> Option<f32> {
        if main_body_w < px(RAIL_NARROW_BREAK) {
            return None;
        }
        if main_body_w < px(RAIL_NARROW_BREAK + 160.) {
            Some(RAIL_NARROW_WIDTH)
        } else {
            Some(RAIL_DESKTOP_WIDTH)
        }
    }

    /// Reset per-thread cockpit state on thread switch: the outgoing thread's
    /// milestones, running-tool title, and per-model counter state do not
    /// apply to the incoming one. Mirrors the old `Workspace::set_active_thread`
    /// reset.
    pub(crate) fn reset_for_thread_switch(&mut self, running: bool, cx: &mut Context<Self>) {
        self.env_counter_state.clear();
        self.cockpit_phase = if running {
            CockpitPhase::Streaming
        } else {
            CockpitPhase::Idle
        };
        self.cockpit_running_tool_title = None;
        self.cockpit_milestones = Vec::new();
        cx.notify();
    }

    /// Update `cockpit_phase` and `cockpit_running_tool_title` for the
    /// streaming/tool variants that flow through the generic catch-all arm.
    /// `Error`, `Stop`, `TurnStarted`, and `ToolCallAuthorization` are handled
    /// in their dedicated arms on `Workspace`; this only covers the residual
    /// transitions routed here from the workspace event handler.
    pub(crate) fn update_cockpit_phase(&mut self, ev: &ThreadEvent, cx: &mut Context<Self>) {
        match ev {
            ThreadEvent::AgentText(_) => {
                self.cockpit_phase = CockpitPhase::Streaming;
            }
            ThreadEvent::AgentThinking(_) => {
                self.cockpit_phase = CockpitPhase::Thinking;
            }
            ThreadEvent::ToolCall { status, title, .. } => match status {
                agent::thread::ToolCallStatus::Running => {
                    self.cockpit_phase = CockpitPhase::RunningTool;
                    self.cockpit_running_tool_title = Some(title.clone());
                }
                // A non-running terminal/intermediate status means the model
                // is back to streaming the next assistant segment.
                _ => {
                    self.cockpit_phase = CockpitPhase::Streaming;
                }
            },
            ThreadEvent::CompactionStarted { .. } => {
                self.cockpit_phase = CockpitPhase::Summarizing;
            }
            ThreadEvent::Compaction { .. } => {
                // The summary landed; the turn resumes streaming.
                self.cockpit_phase = CockpitPhase::Streaming;
            }
            _ => {}
        }
        cx.notify();
    }

    /// Mark the first `Pending` milestone `InProgress` so the panel signals
    /// which step is currently being worked. Called on `TurnStarted`. Only the
    /// first pending item is promoted — the cockpit never claims to know the
    /// model's exact progress within a step.
    pub(crate) fn promote_first_milestone_in_progress(&mut self, cx: &mut Context<Self>) {
        for m in &mut self.cockpit_milestones {
            if m.status == MilestoneStatus::Pending {
                m.status = MilestoneStatus::InProgress;
                break;
            }
        }
        cx.notify();
    }

    /// Demote any `InProgress` milestone back to `Pending` on a terminal stop.
    pub(crate) fn demote_milestones_to_pending(&mut self, cx: &mut Context<Self>) {
        for m in &mut self.cockpit_milestones {
            if m.status == MilestoneStatus::InProgress {
                m.status = MilestoneStatus::Pending;
            }
        }
        cx.notify();
    }

    /// Seed milestones from an approved plan. Continue-in-plan does not — the
    /// user asked for another planning round, so the prior steps no longer
    /// describe committed work.
    pub(crate) fn set_milestones_from_plan(&mut self, plan_text: &str, cx: &mut Context<Self>) {
        self.cockpit_milestones = crate::cockpit::parse_milestones(plan_text);
        cx.notify();
    }

    // ── Rendering ─────────────────────────────────────────────────────────

    /// The rail body: a scrollable panel with the conversation-info card.
    /// Rendered as a normal flex child (no `absolute()`), so it occupies its
    /// own column and never overlaps the conversation or the composer.
    fn render_panel(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        // Approval mode used to live in the card's "Modes" section (now removed);
        // the per-mode chip in the composer footer reads it directly, so we
        // don't need to query it here.
        let (project, per_model) = {
            let thread = self.thread.read(cx);
            (
                thread.project().cloned(),
                thread.per_model_token_usage().clone(),
            )
        };
        let branch_label = if project.is_some() {
            "main".to_string()
        } else {
            i18n::t("workspace-env-no-project").to_string()
        };
        let muted = theme.muted_foreground;

        // Build per-model token rows, sorted by total usage descending.
        let mut model_rows: Vec<_> = per_model
            .into_iter()
            .filter(|(_, u)| u.total_tokens() > 0)
            .collect();
        model_rows.sort_by_key(|b| std::cmp::Reverse(b.1.total_tokens()));

        // Diff every cell against the last-rendered value, bump the per-cell
        // gen on delta, and rebuild `env_counter_state` so the map is bounded
        // by the number of live models. The four cells per model are
        // (field, value) pairs; we capture (from, to, gen) per cell so the
        // animation closures below can be plain `'static` move-closures.
        // Tree labels are constant across iterations; resolve once before the
        // loop so each iteration just clones the SharedString instead of
        // re-running the i18n lookup.
        let throughput_label = i18n::t("workspace-env-throughput");
        let cache_label = i18n::t("workspace-env-cache");
        let mut new_state: HashMap<String, (u64, u64)> = HashMap::new();
        // Each model block carries: model id text + two tree rows. The
        // capture-based build here keeps the closure machinery out of the
        // outer builder.
        let mut model_blocks: Vec<(String, gpui::Div, gpui::Div)> =
            Vec::with_capacity(model_rows.len());

        for (model_name, usage) in model_rows {
            let model_display = truncate_env_model_id(model_name.clone());
            let cells: [(&str, u64); 4] = [
                ("in", usage.input_tokens),
                ("out", usage.output_tokens),
                ("cache_create", usage.cache_creation_input_tokens),
                ("cache_read", usage.cache_read_input_tokens),
            ];
            let mut from_to_gen: [(u64, u64, u64); 4] = [(0, 0, 0); 4];
            for (i, (field, value)) in cells.iter().enumerate() {
                let cell_key = format!("{model_name}|{field}");
                let (old_value, new_gen) = match self.env_counter_state.get(&cell_key) {
                    // First render for this cell: roll from 0 up to the
                    // current value (gen 1).
                    None => (0u64, 1u64),
                    // Value unchanged: reuse the existing gen so the
                    // animation id is stable and gpui doesn't replay.
                    Some(&(gen_, last)) if last == *value => (last, gen_),
                    // Value changed: roll from the previous value to the
                    // new one, bumping gen so a fresh tween fires.
                    Some(&(gen_, last)) => (last, gen_.wrapping_add(1)),
                };
                new_state.insert(cell_key, (new_gen, *value));
                from_to_gen[i] = (old_value, *value, new_gen);
            }

            // Tree prefix glyph (`├── ` / `└── `) painted slightly muted so
            // it reads as chrome rather than data.
            let branch_prefix = |glyph: &'static str| -> gpui::Div {
                gpui::div()
                    .text_xs()
                    .text_color(muted.opacity(0.55))
                    .child(SharedString::from(glyph))
            };
            let (in_f, in_t, in_g) = from_to_gen[0];
            let (out_f, out_t, out_g) = from_to_gen[1];
            let (cce_f, cce_t, cce_g) = from_to_gen[2];
            let (ccr_f, ccr_t, ccr_g) = from_to_gen[3];

            let throughput_row = h_flex()
                .pl(px(12.))
                .gap_1()
                .items_center()
                .text_xs()
                .text_color(muted)
                .child(branch_prefix("├── "))
                .child(throughput_label.clone())
                .child(counter_animated("↑", in_f, in_t, "in", in_g))
                .child(counter_animated("↓", out_f, out_t, "out", out_g));
            let cache_row = h_flex()
                .pl(px(12.))
                .gap_1()
                .items_center()
                .text_xs()
                .text_color(muted)
                .child(branch_prefix("└── "))
                .child(cache_label.clone())
                .child(counter_animated("↑", cce_f, cce_t, "cache_create", cce_g))
                .child(counter_animated("↓", ccr_f, ccr_t, "cache_read", ccr_g));
            model_blocks.push((model_display, throughput_row, cache_row));
        }
        // Commit the rebuilt per-cell state; auto-prunes cells whose model
        // disappeared (the map only contains live cells from this render).
        self.env_counter_state = new_state;

        v_flex()
            .w_full()
            .h_full()
            .min_h_0()
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .child(
                        gpui::div()
                            .text_sm()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.foreground)
                            .child(i18n::t("context-rail-title")),
                    )
                    .child(
                        Button::new("context-rail-collapse")
                            .ghost()
                            .xsmall()
                            .icon(IconName::PanelRightClose)
                            .tooltip(i18n::t("context-rail-collapse")),
                    ),
            )
            .child(self.render_cockpit_status_row(theme, cx))
            .child(env_row(
                IconName::Bot,
                self.thread.read(cx).display_title().into(),
                None,
                theme,
            ))
            .child(env_row(
                IconName::Frame,
                i18n::t("workspace-env-changes"),
                Some(
                    h_flex()
                        .gap_1()
                        .text_xs()
                        .child(
                            gpui::div()
                                .text_color(theme.success)
                                .child(if project.is_some() { "+0" } else { "--" }),
                        )
                        .child(
                            gpui::div()
                                .text_color(theme.danger)
                                .child(if project.is_some() { "-0" } else { "" }),
                        )
                        .into_any_element(),
                ),
                theme,
            ))
            .child(env_row(IconName::Github, branch_label.into(), None, theme))
            // Usage section: section header + per-model tree blocks.
            // Each block is a v_flex: model id on the top line,
            // throughput + cache tree rows below, separated by gap_0p5.
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(Icon::new(IconName::MemoryStick).xsmall().text_color(muted))
                            .child(
                                gpui::div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(i18n::t("workspace-env-usage")),
                            ),
                    )
                    .children(model_blocks.into_iter().map(
                        |(model_display, throughput_row, cache_row)| {
                            v_flex()
                                .w_full()
                                .gap_0p5()
                                .child(
                                    gpui::div()
                                        .text_xs()
                                        .text_color(theme.foreground)
                                        .child(model_display),
                                )
                                .child(throughput_row)
                                .child(cache_row)
                                .into_any_element()
                        },
                    )),
            )
            .child(self.render_cockpit_context_budget(theme, cx))
            .child(self.render_cockpit_milestones(theme, cx))
            .child(gpui::div().h(px(1.)).w_full().bg(theme.border))
            // Sources section.
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        gpui::div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(i18n::t("workspace-env-sources")),
                    )
                    .child(
                        gpui::div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(i18n::t("workspace-env-no-sources")),
                    ),
            )
            .into_any_element()
    }

    /// Run-status row: phase label + elapsed (per-second refresh via the
    /// thinking ticker) + cumulative token count. Sits directly under the rail
    /// title so the current phase is the first thing the eye lands on.
    fn render_cockpit_status_row(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let phase_key = match self.cockpit_phase {
            CockpitPhase::Idle => "cockpit-status-idle",
            CockpitPhase::Thinking => "cockpit-status-thinking",
            CockpitPhase::Streaming => "cockpit-status-streaming",
            CockpitPhase::RunningTool => "cockpit-status-running-tool",
            CockpitPhase::AwaitingApproval => "cockpit-status-awaiting-approval",
            CockpitPhase::Summarizing => "cockpit-status-summarizing",
            CockpitPhase::Stopped => "cockpit-status-stopped",
            CockpitPhase::Failed => "cockpit-status-failed",
        };
        let phase_label = i18n::t(phase_key).to_string();
        let thread = self.thread.read(cx);
        let elapsed = thread
            .turn_started_at()
            .map(|t| crate::cockpit::format_elapsed(t.elapsed()))
            .unwrap_or_else(|| "0s".to_string());
        let tokens = crate::cockpit::format_tokens(thread.cumulative_token_usage().total_tokens());
        let mut label = i18n::t_str(
            "cockpit-run-status",
            &[
                ("phase", phase_label.as_str()),
                ("elapsed", elapsed.as_str()),
                ("tokens", tokens.as_str()),
            ],
        )
        .to_string();
        // When a tool is running, append its title so the user can see *what*
        // is executing, not just that something is.
        if let Some(title) = &self.cockpit_running_tool_title
            && !title.is_empty()
        {
            label.push_str(" · ");
            label.push_str(title);
        }
        let icon = match self.cockpit_phase {
            CockpitPhase::Thinking | CockpitPhase::Streaming | CockpitPhase::Summarizing => {
                IconName::LoaderCircle
            }
            CockpitPhase::RunningTool => IconName::Play,
            CockpitPhase::AwaitingApproval => IconName::Bell,
            CockpitPhase::Stopped => IconName::Pause,
            CockpitPhase::Failed => IconName::CircleX,
            CockpitPhase::Idle => IconName::Dash,
        };
        env_row(icon, label.into(), None, theme)
    }

    /// Context-budget row: percent of the window still free before auto-summary
    /// fires (or before the raw window fills, when auto-summary is off / window
    /// too small). Hidden entirely when no usage has been reported yet (first
    /// turn). Turns warning-colored within 10% of the trigger.
    fn render_cockpit_context_budget(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let thread = self.thread.read(cx);
        let max_input = thread.model().map(|m| m.max_token_count()).unwrap_or(0);
        let budget = context_budget_pct(
            max_input,
            thread.last_request_token_usage(),
            self.cockpit_auto_compact_enabled,
            self.cockpit_auto_compact_threshold,
        );
        let Some(budget) = budget else {
            // No usage yet — render a muted placeholder so the section doesn't
            // pop in/out as the first turn streams.
            return env_row(
                IconName::BatteryFull,
                i18n::t("cockpit-context-of-window")
                    .replace("{$pct}", "100")
                    .into(),
                None,
                theme,
            );
        };
        let pct = (budget.remaining_pct.round() as i64).clamp(0, 100);
        let key = if self.cockpit_auto_compact_enabled && max_input >= MIN_COMPACTION_CONTEXT_WINDOW
        {
            "cockpit-context-until-auto-summary"
        } else {
            "cockpit-context-of-window"
        };
        let label = i18n::t_str(key, &[("pct", &pct.to_string())]);
        let near = budget.remaining_pct <= 10.0;
        let color = if near {
            theme.warning
        } else {
            theme.muted_foreground
        };
        env_row(
            IconName::BatteryFull,
            label,
            Some(
                gpui::div()
                    .text_xs()
                    .text_color(color)
                    .child(i18n::t("cockpit-context-estimate"))
                    .into_any_element(),
            ),
            theme,
        )
    }

    /// Milestone section: the parsed plan steps with status glyphs. Collapsible
    /// by clicking the header or pressing `ToggleCockpitTasks`
    /// (cmd-shift-m / ctrl-shift-m). When collapsed, only the header renders
    /// so the user always has an affordance to expand again. Hidden entirely
    /// when there are no milestones (no plan yet).
    fn render_cockpit_milestones(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        if self.cockpit_milestones.is_empty() {
            return gpui::div().into_any_element();
        }
        let muted = theme.muted_foreground;
        let hidden = self.cockpit_hide_tasks;
        let hint_key = if hidden {
            "cockpit-show-tasks-hint"
        } else {
            "cockpit-hide-tasks-hint"
        };
        // Header is a clickable toggle — `cursor_pointer` signals it, and the
        // chevron indicates expand/collapse state so the row is readable even
        // without the trailing hint.
        let chevron = if hidden {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };
        let header = h_flex()
            .id("cockpit-milestones-header")
            .w_full()
            .items_center()
            .gap_1()
            .cursor_pointer()
            .on_click(cx.listener(|this, _: &gpui::ClickEvent, _window, cx| {
                this.cockpit_hide_tasks = !this.cockpit_hide_tasks;
                cx.notify();
            }))
            .child(Icon::new(IconName::Menu).xsmall().text_color(muted))
            .child(Icon::new(chevron).xsmall().text_color(muted))
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(muted)
                    .child(i18n::t("cockpit-milestones-header")),
            )
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(muted.opacity(0.6))
                    .child(i18n::t(hint_key)),
            );
        if hidden {
            return v_flex().w_full().gap_1().child(header).into_any_element();
        }
        // Render the most recent completed milestone plus a collapsed summary
        // of any earlier ones, so a long list of done steps doesn't drown out
        // the live one. The cockpit only ever marks Pending/InProgress, so the
        // completed/failed branches are forward-compat render paths.
        let mut completed_tail: Option<&Milestone> = None;
        let mut completed_count = 0usize;
        for m in &self.cockpit_milestones {
            if m.status == MilestoneStatus::Completed {
                completed_count += 1;
                completed_tail = Some(m);
            }
        }
        let mut section = v_flex().w_full().gap_1().child(header);
        for m in &self.cockpit_milestones {
            if m.status == MilestoneStatus::Completed && Some(m) != completed_tail {
                continue;
            }
            section = section.child(self.render_milestone_row(m, theme));
        }
        if completed_count > 1 {
            section = section.child(gpui::div().pl(px(12.)).text_xs().text_color(muted).child(
                i18n::t_count("cockpit-completed-summary", (completed_count - 1) as i64),
            ));
        }
        section.into_any_element()
    }

    /// One milestone row: status glyph + title (+blocked-by note). The
    /// in-progress step is foreground-bold; others are muted so the live step
    /// stands out.
    fn render_milestone_row(&self, m: &Milestone, theme: &Theme) -> AnyElement {
        let muted = theme.muted_foreground;
        let (glyph, glyph_color) = match m.status {
            MilestoneStatus::Pending => ("◻", muted),
            MilestoneStatus::InProgress => ("▶", theme.foreground),
            MilestoneStatus::Blocked { .. } => ("⏳", theme.warning),
            MilestoneStatus::Completed => ("✔", muted.opacity(0.7)),
            MilestoneStatus::Failed => ("✕", theme.danger),
        };
        let (title_color, weight) = match m.status {
            MilestoneStatus::InProgress => (theme.foreground, gpui::FontWeight::SEMIBOLD),
            _ => (muted, gpui::FontWeight::NORMAL),
        };
        let mut title = m.title.clone();
        if let MilestoneStatus::Blocked { by } = &m.status
            && !by.is_empty()
        {
            let deps: Vec<String> = by.iter().map(|i| format!("#{i}")).collect();
            title.push(' ');
            title.push_str(&i18n::t_str(
                "cockpit-blocked-by",
                &[("deps", &deps.join(", "))],
            ));
        }
        h_flex()
            .w_full()
            .pl(px(12.))
            .gap_1()
            .items_center()
            .text_xs()
            .child(
                gpui::div()
                    .text_color(glyph_color)
                    .min_w(px(14.))
                    .child(SharedString::from(glyph)),
            )
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(title_color)
                    .font_weight(weight)
                    .child(SharedString::from(title)),
            )
            .into_any_element()
    }
}

impl Render for ContextRail {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex()
            .w_full()
            .h_full()
            .min_h_0()
            .bg(theme.background)
            .border_l_1()
            .border_color(theme.border)
            .child(
                // `min_h_0` lets the panel shrink below its content height so
                // `overflow_y_scroll` actually engages; without it the flex item's
                // min-height defaults to content and the panel grows past the
                // viewport instead of scrolling.
                gpui::div()
                    .id("context-rail-body")
                    .w_full()
                    .h_full()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(self.render_panel(&theme, cx)),
            )
    }
}

// ── Free helpers (moved from workspace.rs) ────────────────────────────────

fn env_row(
    icon: IconName,
    label: SharedString,
    trailing: Option<AnyElement>,
    theme: &Theme,
) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(Icon::new(icon).xsmall().text_color(theme.muted_foreground))
        .child(
            gpui::div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .text_color(theme.foreground)
                .child(label),
        )
        .children(trailing)
        .into_any_element()
}

/// Clamp a model id so the rail never wraps it. Ids up to
/// `ENV_MODEL_ID_MAX` ("MiniMax/MiniMax-M3[1m]") render in full; longer ones
/// are cut to the cap, trimmed by 3 chars, then suffixed with "..." — so the
/// result is exactly `ENV_MODEL_ID_MAX` chars (e.g. "MiniMax/MiniMax-M3[...").
fn truncate_env_model_id(id: String) -> String {
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= ENV_MODEL_ID_MAX {
        return id;
    }
    let head: String = chars.into_iter().take(ENV_MODEL_ID_MAX - 3).collect();
    format!("{head}...")
}

/// Compact token count display: `1m,357k`, `168k,653`, `999`.
fn format_tokens(n: u64) -> String {
    const MILLION: u64 = 1_000_000;
    const THOUSAND: u64 = 1_000;
    if n >= MILLION {
        let m = n / MILLION;
        let r = (n % MILLION) / THOUSAND;
        if r == 0 {
            format!("{m}m")
        } else {
            format!("{m}m,{r}k")
        }
    } else if n >= THOUSAND {
        let k = n / THOUSAND;
        let r = n % THOUSAND;
        if r == 0 {
            format!("{k}k")
        } else {
            format!("{k}k,{r}")
        }
    } else {
        n.to_string()
    }
}

/// Scoreboard-style token counter: linearly interpolates the displayed
/// integer from `from` to `to` over 600ms with `ease_out_quint`, then
/// formats it via [`format_tokens`]. The animation id embeds the cell's
/// `field` identity plus `gen`; bumping `gen` on every value delta forces
/// gpui to fire a fresh 0→1 tween, while a stable `gen` reuses the cached
/// end-state and renders the value statically.
///
/// `arrow` is `&'static str` (`"↑"` / `"↓"`) so the closure stays
/// `'static` without copying the arrow into a `SharedString` on every
/// frame. `field` is also `&'static str` for the same reason — only the
/// four canonical field names ever flow through here.
///
/// The visible text_color cascades from the parent `h_flex` row (which
/// sets `text_color(muted)`), so this helper only owns the interpolated
/// value text.
fn counter_animated(
    arrow: &'static str,
    from: u64,
    to: u64,
    field: &'static str,
    gen_: u64,
) -> AnyElement {
    let anim_id = format!("env-counter-{field}-{gen_}");
    let from_f = from as f64;
    let to_f = to as f64;
    gpui::div()
        .with_animation(
            anim_id,
            Animation::new(Duration::from_millis(600)).with_easing(ease_out_quint()),
            move |el, t| {
                let v = (from_f + (to_f - from_f) * t as f64) as u64;
                el.child(SharedString::from(
                    format!("{}{}", arrow, format_tokens(v),),
                ))
            },
        )
        .into_any_element()
}
