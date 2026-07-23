//! Right-hand context rail: a stable sidecar showing the active thread's
//! environment/cockpit information (run status, changes, branch, per-model
//! token usage, context budget, execution plan, sources).
//!
//! The rail is a first-class view owned by [`crate::Workspace`]. It holds the
//! cockpit state (run phase, the model's plan snapshot, per-cell counter
//! animation state)
//! that used to live directly on `Workspace`, plus strong handles to the
//! active [`agent::Thread`] and [`crate::ConversationState`] it renders
//! against. Writes to cockpit state flow through `Workspace` →
//! `self.context_rail.update(cx, |r, cx| …)`.
//!
//! Layout: a fixed-width card that floats over the conversation column's
//! top-right as an absolute overlay — a peer in the z-stack, not a flex
//! column and not a flush rail. The conversation body reserves the card's
//! width as right padding so the message list never hides behind it. A shared
//! title bar spans the whole conversation column over both the message list
//! and this card's slot. The editor pane is a third top-level column outside
//! the conversation, and while it is open the card stays hidden so the
//! conversation reclaims its width.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use agent::{Thread, ThreadEvent, i18n};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, ClickEvent, ClipboardItem, Context, Entity,
    MouseButton, MouseUpEvent, Render, SharedString, WeakEntity, Window, ease_out_quint,
    prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TITLE_BAR_HEIGHT, Theme, WindowExt as _,
    h_flex, notification::Notification, tooltip::Tooltip, v_flex,
};

use crate::Workspace;
use agent::{PlanSnapshot, PlanStepStatus};

use crate::cockpit::{CockpitPhase, cache_read_ratio, cockpit_phase_tag, context_budget_pct};
use crate::git_status::{GitBranchDisplay, GitChangeStats};
use crate::views::braille_spinner::BrailleSpinner;
use crate::views::subagent_panel::{SubagentInfo, status_indicator, subagent_display_title};

// ── Geometry ─────────────────────────────────────────────────────────────

/// Floating card width. Wide enough for the per-model usage block: model id
/// (plus trailing cache-hit badge) on the top line, `├── 穿透` (input /
/// (input / output) and `└── 缓存` (cache read) tree rows underneath,
/// each with `↑↓` animated counters.
pub(crate) const ENV_CARD_WIDTH: f32 = 260.;
/// Right inset the conversation body reserves for the floating card: the
/// card width plus a gutter so the message list clears the card's shadow.
pub(crate) const ENV_CONTENT_INSET: f32 = ENV_CARD_WIDTH + 36.;
/// Below this main-column width the card folds away and the conversation
/// column takes the full body. Matches the old env-card gate so a narrow
/// window never crowds the conversation.
const RAIL_NARROW_BREAK: f32 = 900.;

/// Longest model id rendered in full, calibrated to "MiniMax/MiniMax-M3[1m]"
/// (22 chars). Longer ids are cut to this width then trimmed by 3 and given a
/// "..." suffix, so the result stays one line at `ENV_MODEL_ID_MAX` chars
/// (e.g. "MiniMax/MiniMax-M3[...").
const ENV_MODEL_ID_MAX: usize = 22;

// ── ContextRail view ──────────────────────────────────────────────────────

/// Right-side context sidecar. Owns the cockpit state (run phase, the model's
/// plan snapshot, per-cell counter animation state) and renders the
/// environment/cockpit panel that used to float as an absolute card over the
/// conversation.
pub(crate) struct ContextRail {
    pub(crate) thread: Entity<Thread>,
    /// Coarse run phase shown in the status row. Derived from `ThreadEvent`s
    /// routed here by `Workspace`; the per-second thinking ticker's
    /// `cx.notify()` also refreshes the elapsed display.
    pub(crate) cockpit_phase: CockpitPhase,
    /// Last tag index the status-row slider committed. The slider animates from
    /// this to the freshly computed [`cockpit_phase_tag`] index whenever the
    /// phase drifts to a different tag; `cockpit_tag_gen` bumps to force a fresh
    /// 0→1 tween. Computed in render (not on every phase write) so the
    /// workspace's direct `cockpit_phase = …` assignments need no hook.
    pub(crate) cockpit_tag_prev: u8,
    pub(crate) cockpit_tag_gen: u64,
    /// The model's current execution plan, published via `UpdatePlan` and
    /// recovered from history on reload. `None` until the model publishes one
    /// (or after it clears its list). The rail renders the snapshot's own
    /// step statuses verbatim — nothing here infers progress.
    pub(crate) plan: Option<PlanSnapshot>,
    /// Whether the plan section is collapsed (`ToggleCockpitTasks` /
    /// cmd/ctrl-shift-m toggles). Hidden still renders the run-status row.
    pub(crate) cockpit_hide_tasks: bool,
    /// Whether a plan has been seen for the current thread yet. The first
    /// snapshot auto-collapses when it is long enough; subsequent updates
    /// preserve whatever collapse state the user last chose.
    pub(crate) plan_seen: bool,
    /// Cached `settings.auto_compact.{enabled,threshold}`, refreshed on
    /// construction and when the user exits the Settings overlay. Avoids a
    /// per-frame file read in the context-budget render.
    pub(crate) cockpit_auto_compact_enabled: bool,
    pub(crate) cockpit_auto_compact_threshold: f64,
    weak_workspace: WeakEntity<Workspace>,
    agents: Vec<SubagentInfo>,
    /// Per-cell scoreboard state, keyed by `"{model_name}|{field}"` where
    /// `field ∈ {in, out, cache_create, cache_read}`. Stored as
    /// `(gen, last_value)`: `gen` is bumped on every value delta so the
    /// animation's element id changes and gpui fires a fresh 0→1 tween;
    /// `last_value` is the value the previous render committed and becomes
    /// the start of the count-up interpolation. Rebuilt every render so
    /// cells whose model disappeared (e.g. thread reset) are pruned, keeping
    /// the map bounded by the number of live models.
    pub(crate) env_counter_state: HashMap<String, (u64, u64)>,
    /// Last request's model-facing projection breakdown and optimization
    /// savings, including estimates collected by shadow-mode features.
    pub(crate) optimization: Option<agent::ContextOptimizationMetrics>,
    /// Per-turn prefix cache diagnostic for the current thread. Updated every
    /// turn after token finalization; cleared on thread switch.
    pub(crate) cache_diagnostic: Option<agent::CacheDiagnostic>,
    pub(crate) side_calls: Vec<agent::SideCallMetric>,
    pub(crate) main_call: Option<agent::SideCallMetric>,
    /// Latest git change stats for the thread's cwd. Refreshed (debounced) by
    /// `Workspace` on thread attach, terminal stop, and enter/exit worktree.
    pub(crate) git_change_stats: Option<GitChangeStats>,
    /// Latest resolved branch display for the thread's cwd. `None` until the
    /// first refresh completes; the changes/branch rows render placeholders
    /// until then.
    pub(crate) git_branch_display: Option<GitBranchDisplay>,
}

impl ContextRail {
    pub(crate) fn new(
        thread: Entity<Thread>,
        weak_workspace: WeakEntity<Workspace>,
        auto_compact_enabled: bool,
        auto_compact_threshold: f64,
    ) -> Self {
        Self {
            thread,
            cockpit_phase: CockpitPhase::Idle,
            cockpit_tag_prev: cockpit_phase_tag(CockpitPhase::Idle),
            cockpit_tag_gen: 0,
            plan: None,
            cockpit_hide_tasks: false,
            plan_seen: false,
            cockpit_auto_compact_enabled: auto_compact_enabled,
            cockpit_auto_compact_threshold: auto_compact_threshold,
            weak_workspace,
            agents: Vec::new(),
            env_counter_state: HashMap::new(),
            optimization: None,
            cache_diagnostic: None,
            side_calls: Vec::new(),
            main_call: None,
            git_change_stats: None,
            git_branch_display: None,
        }
    }

    /// Whether the floating context card is shown at the given main-column
    /// body width. `None` means the window is too narrow: the card folds away
    /// and the conversation column takes the full body.
    pub(crate) fn rail_width_for(main_body_w: gpui::Pixels) -> Option<f32> {
        if main_body_w < px(RAIL_NARROW_BREAK) {
            None
        } else {
            Some(ENV_CARD_WIDTH)
        }
    }

    /// Reset per-thread cockpit state on thread switch: the outgoing thread's
    /// plan, running-tool title, and per-model counter state do not
    /// apply to the incoming one. Mirrors the old `Workspace::set_active_thread`
    /// reset. Also clears the cached git stats so the incoming thread shows
    /// placeholders until its own refresh lands.
    pub(crate) fn reset_for_thread_switch(&mut self, running: bool, cx: &mut Context<Self>) {
        self.env_counter_state.clear();
        self.optimization = None;
        self.cache_diagnostic = None;
        self.side_calls.clear();
        self.main_call = None;
        self.agents.clear();
        let new_phase = if running {
            CockpitPhase::Streaming
        } else {
            CockpitPhase::Idle
        };
        self.cockpit_phase = new_phase;
        self.cockpit_tag_prev = cockpit_phase_tag(new_phase);
        // The incoming thread's plan is seeded separately from its history by
        // `set_plan`; clear here so a thread with no plan starts empty rather
        // than inheriting the outgoing thread's list.
        self.plan = None;
        self.plan_seen = false;
        self.git_change_stats = None;
        self.git_branch_display = None;
        cx.notify();
    }

    /// Replace the cached git stats/branch display. Called by `Workspace`
    /// after a debounced background `git_status::gather` resolves.
    pub(crate) fn set_git_status(
        &mut self,
        stats: Option<GitChangeStats>,
        display: Option<GitBranchDisplay>,
        cx: &mut Context<Self>,
    ) {
        self.git_change_stats = stats;
        self.git_branch_display = display;
        cx.notify();
    }

    /// Update `cockpit_phase` for the streaming/tool variants that flow through
    /// the generic catch-all arm. `Error`, `Stop`, `TurnStarted`, and
    /// `ToolCallAuthorization` are handled in their dedicated arms on `Workspace`;
    /// this only covers the residual transitions routed here from the workspace
    /// event handler.
    pub(crate) fn update_cockpit_phase(&mut self, ev: &ThreadEvent, cx: &mut Context<Self>) {
        match ev {
            ThreadEvent::AgentText(_) => {
                self.cockpit_phase = CockpitPhase::Streaming;
            }
            ThreadEvent::AgentThinking(_) => {
                self.cockpit_phase = CockpitPhase::Thinking;
            }
            ThreadEvent::ToolCall { status, .. } => match status {
                agent::thread::ToolCallStatus::Running => {
                    self.cockpit_phase = CockpitPhase::RunningTool;
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

    pub(crate) fn set_agents(&mut self, agents: Vec<SubagentInfo>, cx: &mut Context<Self>) {
        self.agents = agents;
        cx.notify();
    }

    /// Threshold above which a freshly-seen plan auto-collapses so a long list
    /// does not dominate the rail. At or below this, the plan starts expanded.
    const PLAN_AUTOCOLLAPSE_ABOVE: usize = 5;

    /// Adopt a plan snapshot published by the model (or recovered from history).
    /// An empty snapshot clears the plan. The first plan seen for a thread sets
    /// the collapse state by length; later updates preserve the user's choice,
    /// so an update never yanks a plan the user manually expanded back closed.
    pub(crate) fn set_plan(&mut self, snapshot: PlanSnapshot, cx: &mut Context<Self>) {
        if snapshot.is_empty() {
            self.plan = None;
            cx.notify();
            return;
        }
        if !self.plan_seen {
            self.plan_seen = true;
            self.cockpit_hide_tasks = snapshot.steps.len() > Self::PLAN_AUTOCOLLAPSE_ABOVE;
        }
        self.plan = Some(snapshot);
        cx.notify();
    }

    // ── Rendering ─────────────────────────────────────────────────────────

    /// The floating context card's body: the conversation-info chrome (border,
    /// rounded corners, drop shadow, background) plus its content rows. The
    /// `Render` impl positions this as an absolute overlay over the
    /// conversation column's top-right; this fn only paints the card itself.
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

        v_flex()
            .w_full()
            .min_h_0()
            .p_3()
            .gap_2()
            .border_1()
            .border_color(theme.border)
            .rounded(theme.radius)
            .bg(theme.background)
            .shadow(std::vec![
                gpui::BoxShadow::new(px(-3.), px(6.), gpui::hsla(0., 0., 0., 0.22))
                    .blur_radius(px(10.)),
            ])
            .child(
                gpui::div()
                    .text_sm()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme.foreground)
                    .child(i18n::t("context-rail-title")),
            )
            .child(self.render_cockpit_status_row(theme, cx))
            .child(self.render_agents_section(theme, cx))
            .child(self.render_branch_block(&project, theme, cx))
            .child(self.render_usage_section(theme, cx, per_model))
            .child(self.render_optimization_section(theme))
            .child(self.render_main_call_section(theme))
            .child(self.render_side_calls_section(theme))
            .child(self.render_cockpit_context_budget(theme, cx))
            .child(self.render_plan_section(theme, cx))
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

    /// Per-model token usage as a two-row tree: `├── 穿透 ↑{in} 缓存 ↑{ccr}`
    /// (input + cache-read) and `└── 输出 ↓{out}`. Cache creation is no longer
    /// surfaced — cache-read is the only "reused input" metric. A cache-hit
    /// ratio rides beside the model name when the model has any cache-readable
    /// input. The header row carries the conversation's cumulative token total
    /// (moved here from the run-status row) right-aligned. Counter animation
    /// state is diffed and rebuilt here so the per-cell `gen` bumps on every
    /// value delta (see `env_counter_state`).
    fn render_usage_section(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
        per_model: HashMap<String, agent::language_model::TokenUsage>,
    ) -> AnyElement {
        let muted = theme.muted_foreground;
        let total = crate::cockpit::format_tokens(
            self.thread.read(cx).cumulative_token_usage().total_tokens(),
        );
        let mut model_rows: Vec<_> = per_model
            .into_iter()
            .filter(|(_, u)| u.total_tokens() > 0)
            .collect();
        model_rows.sort_by_key(|b| std::cmp::Reverse(b.1.total_tokens()));

        let throughput_label = i18n::t("workspace-env-throughput");
        let cache_label = i18n::t("workspace-env-cache");
        let output_label = i18n::t("workspace-env-output");

        let mut new_state: HashMap<String, (u64, u64)> = HashMap::new();
        // Each model block carries: model id text + the two tree rows
        // (throughput, output). The capture-based build keeps the closure
        // machinery out of the outer builder.
        let mut model_blocks: Vec<(String, Option<SharedString>, gpui::Div, gpui::Div)> =
            Vec::with_capacity(model_rows.len());

        for (model_name, usage) in model_rows {
            let model_display = truncate_env_model_id(model_name.clone());
            // Cache-hit ratio shown beside the model name when the model has
            // any cache-readable input this turn.
            let hit_rate = cache_read_ratio(usage).map(|r| {
                let pct = (r * 100.0).round() as i64;
                i18n::t_str("workspace-env-cache-hit-rate", &[("pct", &pct.to_string())])
            });
            // Three cells: input (穿透 ↑), output (输出 ↓), cache_read (缓存 ↑).
            // Cache creation is dropped — cache-read is the only cache metric
            // that reads as "reused input" to the user.
            let cells: [(&str, u64); 3] = [
                ("in", usage.input_tokens),
                ("out", usage.output_tokens),
                ("cache_read", usage.cache_read_input_tokens),
            ];
            let mut from_to_gen: [(u64, u64, u64); 3] = [(0, 0, 0); 3];
            for (i, (field, value)) in cells.iter().enumerate() {
                let cell_key = format!("{model_name}|{field}");
                let (old_value, new_gen) = match self.env_counter_state.get(&cell_key) {
                    None => (0u64, 1u64),
                    Some(&(gen_, last)) if last == *value => (last, gen_),
                    Some(&(gen_, last)) => (last, gen_.wrapping_add(1)),
                };
                new_state.insert(cell_key, (new_gen, *value));
                from_to_gen[i] = (old_value, *value, new_gen);
            }
            let (in_f, in_t, in_g) = from_to_gen[0];
            let (out_f, out_t, out_g) = from_to_gen[1];
            let (ccr_f, ccr_t, ccr_g) = from_to_gen[2];

            // Tree prefix glyph (`├── ` / `└── `) painted slightly muted so it
            // reads as chrome rather than data.
            let branch_prefix = |glyph: &'static str| -> gpui::Div {
                gpui::div()
                    .text_xs()
                    .text_color(muted.opacity(0.55))
                    .child(SharedString::from(glyph))
            };
            let throughput_row = h_flex()
                .pl(px(12.))
                .gap_1()
                .items_center()
                .text_xs()
                .text_color(muted)
                .child(branch_prefix("├── "))
                .child(throughput_label.clone())
                .child(counter_animated("↑", in_f, in_t, "in", in_g))
                .child(cache_label.clone())
                .child(counter_animated("↑", ccr_f, ccr_t, "cache_read", ccr_g));
            let output_row = h_flex()
                .pl(px(12.))
                .gap_1()
                .items_center()
                .text_xs()
                .text_color(muted)
                .child(branch_prefix("└── "))
                .child(output_label.clone())
                .child(counter_animated("↓", out_f, out_t, "out", out_g));
            model_blocks.push((model_display, hit_rate, throughput_row, output_row));
        }
        self.env_counter_state = new_state;

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
                    )
                    .child(
                        gpui::div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(total)),
                    ),
            )
            .children(model_blocks.into_iter().map(
                |(model_display, hit_rate, throughput_row, output_row)| {
                    v_flex()
                        .w_full()
                        .gap_0p5()
                        .child(
                            h_flex()
                                .items_center()
                                .gap_1()
                                .child(
                                    gpui::div()
                                        .flex_1()
                                        .min_w_0()
                                        .truncate()
                                        .text_xs()
                                        .text_color(theme.foreground)
                                        .child(model_display),
                                )
                                .children(hit_rate.map(|r| {
                                    gpui::div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(r)
                                        .into_any_element()
                                })),
                        )
                        .child(throughput_row)
                        .child(output_row)
                        .into_any_element()
                },
            ))
            .into_any_element()
    }

    fn render_optimization_section(&self, theme: &Theme) -> AnyElement {
        let Some(metrics) = self.optimization.as_ref() else {
            return gpui::div().into_any_element();
        };
        let savings = crate::cockpit::format_tokens(metrics.saved_tokens);
        // Clone out of `self` so the `'static` tooltip closure owns its data.
        let metrics = metrics.clone();
        let theme = theme.clone();
        gpui::div()
            .id("context-optimization-trigger")
            .w_full()
            .items_center()
            .gap_2()
            .child(
                Icon::new(IconName::ChartPie)
                    .xsmall()
                    .text_color(theme.muted_foreground),
            )
            .child(
                gpui::div()
                    .flex_1()
                    .min_w_0()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(i18n::t("context-opt-title")),
            )
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(theme.success)
                    .child(SharedString::from(savings)),
            )
            .tooltip(move |_window, _cx| {
                let metrics = metrics.clone();
                let theme = theme.clone();
                Tooltip::element(move |_w, _c| build_optimization_table(&metrics, &theme))
                    .build(_window, _cx)
            })
            .into_any_element()
    }

    fn render_side_calls_section(&self, theme: &Theme) -> AnyElement {
        if self.side_calls.is_empty() {
            return gpui::div().into_any_element();
        }
        let muted = theme.muted_foreground;
        v_flex()
            .w_full()
            .gap_0p5()
            .child(
                gpui::div()
                    .text_xs()
                    .text_color(muted)
                    .child(i18n::t("context-side-calls-title")),
            )
            .children(self.side_calls.iter().map(|metric| {
                let average_ms = metric.latency_ms / metric.calls.max(1);
                let cache_rate = cache_read_ratio(metric.token_usage)
                    .map(|ratio| format!("{:.0}%", ratio * 100.0))
                    .unwrap_or_else(|| "--".into());
                gpui::div()
                    .pl(px(12.))
                    .text_xs()
                    .text_color(muted)
                    .child(i18n::t_str(
                        "context-side-calls-row",
                        &[
                            ("purpose", &metric.purpose),
                            ("model", &metric.model),
                            ("calls", &metric.calls.to_string()),
                            (
                                "input",
                                &crate::cockpit::format_tokens(metric.token_usage.input_tokens),
                            ),
                            (
                                "output",
                                &crate::cockpit::format_tokens(metric.token_usage.output_tokens),
                            ),
                            (
                                "cache",
                                &crate::cockpit::format_tokens(
                                    metric.token_usage.cache_read_input_tokens,
                                ),
                            ),
                            ("cache_rate", &cache_rate),
                            ("latency", &average_ms.to_string()),
                        ],
                    ))
                    .into_any_element()
            }))
            .into_any_element()
    }

    fn render_main_call_section(&self, theme: &Theme) -> AnyElement {
        let Some(metric) = self.main_call.as_ref() else {
            return gpui::div().into_any_element();
        };
        let average_ms = metric.latency_ms / metric.calls.max(1);
        let cache_rate = cache_read_ratio(metric.token_usage)
            .map(|ratio| format!("{:.0}%", ratio * 100.0))
            .unwrap_or_else(|| "--".into());
        gpui::div()
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(i18n::t_str(
                "context-main-calls-row",
                &[
                    ("model", &metric.model),
                    ("calls", &metric.calls.to_string()),
                    (
                        "input",
                        &crate::cockpit::format_tokens(metric.token_usage.input_tokens),
                    ),
                    (
                        "output",
                        &crate::cockpit::format_tokens(metric.token_usage.output_tokens),
                    ),
                    (
                        "cache",
                        &crate::cockpit::format_tokens(metric.token_usage.cache_read_input_tokens),
                    ),
                    ("cache_rate", &cache_rate),
                    ("latency", &average_ms.to_string()),
                ],
            ))
            .into_any_element()
    }

    /// Change counts `+added` / `-deleted` plus an untracked badge, themed
    /// directly with no label or icon. Rides as the branch row's trailing
    /// element (right-aligned). `--` / no-project is the placeholder before
    /// the first git refresh lands.
    fn render_changes_trailing(&self, project: &Option<PathBuf>, theme: &Theme) -> AnyElement {
        let Some(stats) = self.git_change_stats.as_ref() else {
            let trailing = if project.is_some() {
                SharedString::from("--")
            } else {
                i18n::t("workspace-env-no-project")
            };
            return gpui::div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .child(trailing)
                .into_any_element();
        };
        let added = format!("+{}", stats.added);
        let deleted = format!("-{}", stats.deleted);
        h_flex()
            .gap_1()
            .text_xs()
            .child(gpui::div().text_color(theme.success).child(added))
            .child(gpui::div().text_color(theme.danger).child(deleted))
            .children(if stats.untracked > 0 {
                Some(
                    gpui::div()
                        .text_color(theme.muted_foreground)
                        .child(format!("?{}", stats.untracked)),
                )
            } else {
                None
            })
            .into_any_element()
    }

    /// Branch block: (1) the worktree directory basename, shown only while the
    /// thread is inside a worktree — click copies the name, double-click copies
    /// the absolute path; (2) the branch row — resolved branch or detached sha
    /// (+ "(detached)") as the label with the changes counts as its right-aligned
    /// trailing. Both rows copy on click with a notification for feedback.
    fn render_branch_block(
        &mut self,
        project: &Option<PathBuf>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let display = self.git_branch_display.clone();
        let worktree_path = self.thread.read(cx).worktree().map(|w| w.path.clone());

        // Branch label: branch / detached sha + (detached).
        let branch_label: SharedString = match &display {
            Some(d) if d.is_no_repo() => i18n::t("workspace-env-git-not-a-repo"),
            Some(d) => {
                let mut s = d
                    .branch
                    .clone()
                    .or_else(|| d.detached_sha.clone())
                    .map(SharedString::from)
                    .unwrap_or_else(|| i18n::t("workspace-env-git-unavailable"));
                if d.branch.is_none() && d.detached_sha.is_some() {
                    s = SharedString::from(format!(
                        "{} {}",
                        s,
                        i18n::t("workspace-env-git-detached")
                    ));
                }
                s
            }
            None => {
                if project.is_some() {
                    SharedString::from("--")
                } else {
                    i18n::t("workspace-env-no-project")
                }
            }
        };

        let changes_line = self.render_changes_trailing(project, theme);

        // Branch row: click copies the branch name with notification feedback.
        let branch_for_copy = display.as_ref().and_then(|d| d.branch.clone());
        let branch_feedback = i18n::t("workspace-env-git-copied-branch");
        let branch_row = env_row_clickable(
            "icons/git-branch.svg".into(),
            branch_label,
            Some(changes_line),
            theme,
            move |_ev: &ClickEvent, window, cx| {
                if let Some(ref name) = branch_for_copy {
                    cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
                    window.push_notification(Notification::success(branch_feedback.clone()), cx);
                }
            },
        );

        let mut block = v_flex().w_full().gap_0p5();
        if let Some(path) = worktree_path {
            let basename = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let basename_clone = basename.clone();
            let name_feedback = i18n::t("workspace-env-git-copied-worktree-name");
            let path_feedback = i18n::t("workspace-env-git-copied-worktree-path");
            let path_for_copy = path.display().to_string();
            block = block.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap_2()
                    .cursor_pointer()
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(move |_this, e: &MouseUpEvent, window, cx| {
                            if e.click_count >= 2 {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    path_for_copy.clone(),
                                ));
                                window.push_notification(
                                    Notification::success(path_feedback.clone()),
                                    cx,
                                );
                            } else {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    basename_clone.clone(),
                                ));
                                window.push_notification(
                                    Notification::success(name_feedback.clone()),
                                    cx,
                                );
                            }
                            cx.stop_propagation();
                        }),
                    )
                    .child(
                        Icon::new(Icon::default().path("icons/workflow.svg"))
                            .xsmall()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        gpui::div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(theme.foreground)
                            .child(SharedString::from(basename)),
                    ),
            );
        }
        block = block.child(branch_row);

        block.into_any_element()
    }

    fn render_agents_section(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        fn append_children(
            parent_id: Option<&str>,
            depth: usize,
            agents: &[SubagentInfo],
            weak_workspace: &WeakEntity<Workspace>,
            theme: &Theme,
            rows: &mut Vec<AnyElement>,
        ) {
            for info in agents
                .iter()
                .filter(|info| info.parent_id.as_deref() == parent_id)
            {
                let title = subagent_display_title(info);
                let tooltip_text = title.clone();
                let id = info.id.clone();
                let weak = weak_workspace.clone();
                rows.push(
                    h_flex()
                        .id(SharedString::from(format!("context-agent-{}", info.id)))
                        .w_full()
                        .min_w_0()
                        .pl(px(12. + depth as f32 * 12.))
                        .py_0p5()
                        .gap_1p5()
                        .items_center()
                        .rounded(px(4.))
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.secondary.opacity(0.5)))
                        .tooltip(move |window, cx| {
                            Tooltip::new(tooltip_text.clone()).build(window, cx)
                        })
                        .on_click(move |_, _window, cx: &mut App| {
                            let _ = weak.update(cx, |workspace, cx| {
                                workspace.open_subagent_tab_by_id(&id, cx);
                            });
                        })
                        .child(status_indicator(info.status, theme))
                        .child(
                            gpui::div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_xs()
                                .text_color(theme.foreground)
                                .child(title),
                        )
                        .into_any_element(),
                );
                append_children(
                    Some(&info.id),
                    depth + 1,
                    agents,
                    weak_workspace,
                    theme,
                    rows,
                );
            }
        }

        let main_status = if self.cockpit_phase == CockpitPhase::Failed {
            agent::ToolCallStatus::Error
        } else if self.thread.read(cx).is_running() {
            agent::ToolCallStatus::Running
        } else {
            agent::ToolCallStatus::Success
        };
        let mut rows = vec![
            h_flex()
                .w_full()
                .py_0p5()
                .gap_1p5()
                .items_center()
                .child(status_indicator(main_status, theme))
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .child(i18n::t("context-agents-main")),
                )
                .into_any_element(),
        ];
        append_children(
            None,
            0,
            &self.agents,
            &self.weak_workspace,
            theme,
            &mut rows,
        );

        v_flex()
            .w_full()
            .gap_0p5()
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(IconName::Bot)
                            .xsmall()
                            .text_color(theme.muted_foreground),
                    )
                    .child(
                        gpui::div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(i18n::t("context-agents-title")),
                    ),
            )
            .children(rows)
            .into_any_element()
    }

    /// Run-status row: a sliding three-tag pill (生成中 / 思考中 / 待输入) +
    /// elapsed meta (per-second refresh via the thinking ticker). The
    /// cumulative token total moved to the usage row's trailing slot. Sits
    /// directly under the rail title.
    fn render_cockpit_status_row(&mut self, theme: &Theme, _cx: &mut Context<Self>) -> AnyElement {
        let icon: AnyElement = match self.cockpit_phase {
            CockpitPhase::Thinking | CockpitPhase::Streaming | CockpitPhase::Summarizing => {
                BrailleSpinner::new()
                    .xsmall()
                    .color(theme.muted_foreground)
                    .into_any_element()
            }
            _ => Icon::new(match self.cockpit_phase {
                CockpitPhase::RunningTool => IconName::Play,
                CockpitPhase::AwaitingApproval => IconName::Bell,
                CockpitPhase::Stopped => IconName::Pause,
                CockpitPhase::Failed => IconName::CircleX,
                CockpitPhase::Idle => IconName::Dash,
                _ => unreachable!(),
            })
            .xsmall()
            .text_color(theme.muted_foreground)
            .into_any_element(),
        };
        cockpit_status_block(icon, self.render_cockpit_phase_tag(theme), theme)
    }

    /// Sliding three-tag pill: 生成中 / 思考中 / 待输入. The highlight block
    /// animates from the previously committed tag index to the freshly
    /// computed [`cockpit_phase_tag`] index over 240ms with `ease_out_quint`;
    /// `cockpit_tag_gen` bumps on every tag change so gpui fires a fresh tween,
    /// while a stable tag reuses the cached end-state and renders statically.
    fn render_cockpit_phase_tag(&mut self, theme: &Theme) -> AnyElement {
        let cur = cockpit_phase_tag(self.cockpit_phase);
        let prev = self.cockpit_tag_prev;
        if cur != prev {
            self.cockpit_tag_gen = self.cockpit_tag_gen.wrapping_add(1);
        }
        let tag_gen = self.cockpit_tag_gen;
        self.cockpit_tag_prev = cur;

        const SLOT_W: f32 = 52.;
        const PILL_H: f32 = 22.;
        let pill_w = SLOT_W * 3.0;
        let from_x = prev as f64 * SLOT_W as f64;
        let to_x = cur as f64 * SLOT_W as f64;
        let anim_id = format!("cockpit-tag-{tag_gen}");

        let labels = [
            i18n::t("cockpit-status-streaming"),
            i18n::t("cockpit-status-thinking"),
            i18n::t("cockpit-status-awaiting-input"),
        ];

        let muted = theme.muted_foreground;
        let fg = theme.foreground;
        let accent = theme.accent;
        let border = theme.border;

        let mut slots = h_flex().w(px(pill_w)).h(px(PILL_H)).relative();
        for (i, label) in labels.iter().enumerate() {
            let color = if i as u8 == cur { fg } else { muted };
            slots = slots.child(
                gpui::div()
                    .w(px(SLOT_W))
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_xs()
                    .text_color(color)
                    .child(label.clone()),
            );
        }

        let highlight = gpui::div()
            .absolute()
            .top_0()
            .h_full()
            .w(px(SLOT_W))
            .rounded(px(6.))
            .bg(accent.opacity(0.15))
            .border_1()
            .border_color(border)
            .with_animation(
                anim_id,
                Animation::new(Duration::from_millis(240)).with_easing(ease_out_quint()),
                move |el, t| {
                    let x = from_x + (to_x - from_x) * t as f64;
                    el.left(px(x as f32))
                },
            );

        gpui::div()
            .relative()
            .w(px(pill_w))
            .h(px(PILL_H))
            .child(highlight)
            .child(slots)
            .into_any_element()
    }

    /// Context-budget block — one line: remaining context-window budget
    /// (tokens, against the auto-compact trigger or raw window), omitted when
    /// the thread has no model / zero window. The active fill is the thread's
    /// effective context tokens — the same max(provider usage, local estimate)
    /// the auto-compaction trigger uses, so the display and the trigger agree.
    /// The cumulative token total lives in the usage row.
    fn render_cockpit_context_budget(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let thread = self.thread.read(cx);
        let max_input = thread.model().map(|m| m.max_token_count()).unwrap_or(0);
        let budget = context_budget_pct(
            max_input,
            agent::compact::effective_context_tokens(
                thread.messages(),
                thread.request_token_usage(),
                thread.agent_language(),
            ),
            self.cockpit_auto_compact_enabled,
            self.cockpit_auto_compact_threshold,
        );
        let muted = theme.muted_foreground;
        let warn = theme.warning;

        // The label starts at a fixed x whether or not the leading slot holds
        // the icon (kept for alignment with sibling rows).
        const LEAD_W: f32 = 14.;

        let mut rows = v_flex().w_full().gap_0p5();

        // Context-window budget (tokens). Omitted when the thread has no
        // model / zero window — no honest percentage to show.
        if let Some(budget) = budget {
            let pct = (budget.remaining_pct.round() as i64).clamp(0, 100);
            let used = crate::cockpit::format_tokens(budget.active_tokens);
            let cap = crate::cockpit::format_tokens(budget.cap_tokens);
            let near = budget.remaining_pct <= 10.0;
            let color = if near { warn } else { muted };
            rows = rows.child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        gpui::div()
                            .w(px(LEAD_W))
                            .h(px(LEAD_W))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::BatteryFull).xsmall().text_color(muted)),
                    )
                    .child(
                        gpui::div()
                            .flex_1()
                            .min_w_0()
                            .text_xs()
                            .text_color(color)
                            .child(i18n::t_str(
                                "cockpit-context-remaining-ctx",
                                &[("pct", &pct.to_string()), ("used", &used), ("cap", &cap)],
                            )),
                    ),
            );
        }

        rows.into_any_element()
    }

    /// Plan section: the model's `UpdatePlan` snapshot rendered as an execution
    /// overview. Collapsible by clicking the header or pressing
    /// `ToggleCockpitTasks` (cmd/ctrl-shift-m). The header carries the
    /// `done/total` count and a chevron; the count and chevron are the only
    /// expand/collapse affordance (no hint text). Collapsed shows just the
    /// current task and the remaining count so the rail stays glanceable;
    /// expanded lists every step in a bounded, scrollable region. Hidden
    /// entirely when there is no plan.
    fn render_plan_section(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(plan) = self.plan.clone() else {
            return gpui::div().into_any_element();
        };
        let muted = theme.muted_foreground;
        let hidden = self.cockpit_hide_tasks;
        let (done, total) = plan.progress();
        let chevron = if hidden {
            IconName::ChevronRight
        } else {
            IconName::ChevronDown
        };
        // Header is a clickable toggle; the chevron plus the `done/total` count
        // signal collapse state, so no separate hint text is needed.
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
                    .text_color(muted.opacity(0.7))
                    .child(i18n::t_str(
                        "cockpit-plan-progress",
                        &[("done", &done.to_string()), ("total", &total.to_string())],
                    )),
            );

        if hidden {
            // Collapsed: surface the current task (or a done summary) plus the
            // remaining count, so the header row alone tells the user where the
            // work stands without expanding.
            let mut section = v_flex().w_full().gap_1().child(header);
            if plan.all_completed() {
                section = section.child(
                    gpui::div()
                        .pl(px(12.))
                        .text_xs()
                        .text_color(muted)
                        .child(i18n::t("cockpit-plan-all-done")),
                );
            } else if let Some(current) = plan.current() {
                let remaining = total.saturating_sub(done);
                section = section.child(self.render_plan_row(current, theme));
                if remaining > 1 {
                    section =
                        section.child(gpui::div().pl(px(12.)).text_xs().text_color(muted).child(
                            i18n::t_str(
                                "cockpit-plan-remaining",
                                &[("count", &(remaining - 1).to_string())],
                            ),
                        ));
                }
            }
            return section.into_any_element();
        }

        // Expanded: every step, in a bounded scroll region so a long plan does
        // not push the rest of the rail off-screen. Incomplete steps (Pending,
        // InProgress) sort before completed ones; within each group the
        // original chronological order is preserved.
        let mut list = v_flex().w_full().gap_1();
        let mut steps: Vec<&agent::PlanStep> = plan.steps.iter().collect();
        steps.sort_by_key(|s| s.status == PlanStepStatus::Completed);
        for step in steps {
            list = list.child(self.render_plan_row(step, theme));
        }
        v_flex()
            .w_full()
            .gap_1()
            .child(header)
            .child(
                gpui::div()
                    .id("cockpit-plan-steps")
                    .w_full()
                    .max_h(px(160.))
                    .overflow_y_scroll()
                    .child(list),
            )
            .into_any_element()
    }

    /// One plan step row: status glyph + title. The in-progress step is
    /// foreground-bold; others are muted so the live step stands out. The title
    /// truncates to one line with a tooltip carrying the full text.
    fn render_plan_row(&self, step: &agent::PlanStep, theme: &Theme) -> AnyElement {
        let muted = theme.muted_foreground;
        let (glyph, glyph_color) = match step.status {
            PlanStepStatus::Pending => ("◻", muted),
            PlanStepStatus::InProgress => ("▶", theme.foreground),
            PlanStepStatus::Completed => ("✔", muted.opacity(0.7)),
        };
        let (title_color, weight) = match step.status {
            PlanStepStatus::InProgress => (theme.foreground, gpui::FontWeight::SEMIBOLD),
            _ => (muted, gpui::FontWeight::NORMAL),
        };
        let title = SharedString::from(step.step.clone());
        // The element id is derived from the (unique) step title so a stateful
        // tooltip can attach; uniqueness is guaranteed by `PlanSnapshot`
        // validation, which rejects duplicate titles.
        let row_id = SharedString::from(format!("plan-step-{}", step.step));
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
                    .id(gpui::ElementId::Name(row_id))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(title_color)
                    .font_weight(weight)
                    .child(title.clone())
                    .tooltip(move |window, cx| Tooltip::new(title.clone()).build(window, cx)),
            )
            .into_any_element()
    }
}

impl Render for ContextRail {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // The context panel floats over the conversation column's top-right as
        // an absolute overlay: `top` clears the shared title bar, `right` +
        // the conversation body's right padding keep the message list clear of
        // the card. `occlude()` captures pointer hits so drags meant for the
        // card don't fall through to the conversation. Content height, not
        // full height — a compact floating card, not a flush column.
        v_flex()
            .absolute()
            .top(TITLE_BAR_HEIGHT + px(16.))
            .right(px(16.))
            .w(px(ENV_CARD_WIDTH))
            .occlude()
            .child(self.render_panel(&theme, cx))
    }
}

// ── Free helpers (moved from workspace.rs) ───────────────────────────────

/// Build the hover-table shown when the user hovers the optimization title
/// row. Two-column layout: label (left) / value (right). Category headers
/// in semibold. Zero-value rows are skipped; projection and prefix-cache
/// rows always render.
fn build_optimization_table(
    metrics: &agent::ContextOptimizationMetrics,
    theme: &Theme,
) -> AnyElement {
    let tokens = |v: u64| crate::cockpit::format_tokens(v);
    let muted = theme.muted_foreground;
    v_flex()
        .gap_1()
        .child(opt_heading("Projection", muted))
        .child(opt_row("Sent", &tokens(metrics.projected_tokens), muted))
        .child(opt_row(
            "Baseline",
            &tokens(metrics.estimated_baseline_tokens),
            muted,
        ))
        .child(opt_row("Saved", &tokens(metrics.saved_tokens), muted))
        .child(opt_heading("Breakdown", muted))
        .child(opt_row("System", &tokens(metrics.system_tokens), muted))
        .child(opt_row("Mode", &tokens(metrics.mode_tokens), muted))
        .child(opt_row(
            "Project",
            &tokens(metrics.project_context_tokens),
            muted,
        ))
        .child(opt_row(
            "Schemas",
            &tokens(metrics.tool_schema_tokens),
            muted,
        ))
        .child(opt_row("History", &tokens(metrics.history_tokens), muted))
        .child(opt_row(
            "Results",
            &tokens(metrics.tool_result_tokens),
            muted,
        ))
        .child(opt_heading("Tools", muted))
        .child(opt_row(
            "Schemas",
            &format!(
                "{}/{}",
                metrics.active_tool_schemas, metrics.total_tool_schemas
            ),
            muted,
        ))
        .when(metrics.rewrite_saved_tokens > 0, |el| {
            el.child(opt_row(
                "Rewrite",
                &tokens(metrics.rewrite_saved_tokens),
                muted,
            ))
        })
        .when(metrics.pruning_saved_tokens > 0, |el| {
            el.child(opt_row(
                "Pruning",
                &tokens(metrics.pruning_saved_tokens),
                muted,
            ))
        })
        .when(metrics.discovery_saved_tokens > 0, |el| {
            el.child(opt_row(
                "Discovery",
                &tokens(metrics.discovery_saved_tokens),
                muted,
            ))
        })
        .child(opt_heading("Runtime", muted))
        .child(opt_row(
            "Prefix",
            &format!("{}%", metrics.prefix_stability_pct),
            muted,
        ))
        .when(metrics.compactions_avoided > 0, |el| {
            el.child(opt_row(
                "Avoided compact",
                &metrics.compactions_avoided.to_string(),
                muted,
            ))
        })
        .when(
            metrics.code_nested_calls > 0 || metrics.code_model_round_trips_avoided > 0,
            |el| {
                el.child(opt_row(
                    "Code",
                    &format!(
                        "{} calls / {} trips saved {}→{}",
                        metrics.code_nested_calls,
                        metrics.code_model_round_trips_avoided,
                        tokens(metrics.code_raw_tokens),
                        tokens(metrics.code_projected_tokens),
                    ),
                    muted,
                ))
            },
        )
        .when(metrics.tool_search_queries > 0, |el| {
            el.child(opt_row(
                "ToolSearch",
                &format!(
                    "{} queries / {} hits",
                    metrics.tool_search_queries, metrics.tool_search_hits,
                ),
                muted,
            ))
        })
        .into_any_element()
}

/// Category header row for the optimization hover table.
fn opt_heading(text: &str, muted: gpui::Hsla) -> AnyElement {
    gpui::div()
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(muted.opacity(0.55))
        .child(SharedString::from(text))
        .into_any_element()
}

/// One label/value row for the optimization hover table.
fn opt_row(label: &str, value: &str, muted: gpui::Hsla) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(
            gpui::div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(muted)
                .child(SharedString::from(label)),
        )
        .child(
            gpui::div()
                .text_xs()
                .text_color(muted)
                .child(SharedString::from(value)),
        )
        .into_any_element()
}

/// Multi-line run-status block. The phase slot is now an arbitrary element
/// (the sliding three-tag pill) rather than a plain label, so the caller owns
/// Status block: a leading xs icon plus the phase element (the sliding
/// three-tag pill), vertically centered on a single baseline like the other
/// rail rows.
fn cockpit_status_block(icon: AnyElement, phase: AnyElement, _theme: &Theme) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(icon)
        .child(v_flex().flex_1().min_w_0().child(phase))
        .into_any_element()
}

/// A clickable row with an icon, label, and optional trailing element. The
/// whole row is a pointer cursor with an `on_click` handler. Used by the
/// branch row to copy the branch name.
///
/// `icon_path` is a `icons/…` asset path resolved through `ExtrasAssetSource`,
/// not an `IconName` — the rail's branch / worktree glyphs (lucide
/// `git-branch`, `workflow`) live in manox's local asset bundle, not the
/// `gpui-component-assets` set that `IconName` is generated from.
fn env_row_clickable(
    icon_path: SharedString,
    label: SharedString,
    trailing: Option<AnyElement>,
    theme: &Theme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    h_flex()
        .id("env-row-clickable")
        .w_full()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .on_click(on_click)
        .child(
            Icon::new(Icon::default().path(icon_path))
                .xsmall()
                .text_color(theme.muted_foreground),
        )
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

/// Compact token count display: `1m357k`, `168k653`, `999`.
fn format_tokens(n: u64) -> String {
    const MILLION: u64 = 1_000_000;
    const THOUSAND: u64 = 1_000;
    if n >= MILLION {
        let m = n / MILLION;
        let r = (n % MILLION) / THOUSAND;
        if r == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{r}k")
        }
    } else if n >= THOUSAND {
        let k = n / THOUSAND;
        let r = n % THOUSAND;
        if r == 0 {
            format!("{k}k")
        } else {
            format!("{k}k{r}")
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
