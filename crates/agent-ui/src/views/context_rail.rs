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

use agent::{Thread, ThreadEvent, i18n};
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, Entity, MouseButton, MouseUpEvent, Render,
    SharedString, WeakEntity, Window, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TITLE_BAR_HEIGHT, Theme, WindowExt as _,
    h_flex, notification::Notification, tooltip::Tooltip, v_flex,
};
use std::path::PathBuf;

use crate::Workspace;
use agent::{PlanSnapshot, PlanStepStatus};

use crate::cockpit::{CockpitPhase, cache_read_ratio, context_budget_pct};
use crate::git_status::{GitBranchDisplay, GitChangeStats};
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

// ── ContextRail view ──────────────────────────────────────────────────────

/// Right-side context sidecar. Owns the cockpit state (run phase, the model's
/// plan snapshot, per-cell counter animation state) and renders the
/// environment/cockpit panel that used to float as an absolute card over the
/// conversation.
pub(crate) struct ContextRail {
    pub(crate) thread: Entity<Thread>,
    /// Coarse run phase. Derived from `ThreadEvent`s routed here by
    /// `Workspace`; used to determine the main agent's status indicator.
    pub(crate) cockpit_phase: CockpitPhase,
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
    /// Last request's model-facing projection breakdown and optimization
    /// savings, including estimates collected by shadow-mode features.
    pub(crate) optimization: Option<agent::ContextOptimizationMetrics>,
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
            plan: None,
            cockpit_hide_tasks: false,
            plan_seen: false,
            cockpit_auto_compact_enabled: auto_compact_enabled,
            cockpit_auto_compact_threshold: auto_compact_threshold,
            weak_workspace,
            agents: Vec::new(),
            optimization: None,
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
        self.optimization = None;
        self.side_calls.clear();
        self.main_call = None;
        self.agents.clear();
        let new_phase = if running {
            CockpitPhase::Streaming
        } else {
            CockpitPhase::Idle
        };
        self.cockpit_phase = new_phase;
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
        let project = {
            let thread = self.thread.read(cx);
            thread.project().cloned()
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
            .child(self.render_agents_section(theme, cx))
            .child(self.render_branch_block(&project, theme, cx))
            .child(self.render_usage_section(theme, cx))
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

    /// Cumulative token total row with a hover tooltip consolidating main
    /// calls, side calls, and context optimization distribution data.
    /// Usage section: a header row with the cumulative token total, followed by
    /// a per-model token breakdown tree when per-model data is available.
    /// Side-call metrics and context optimization distribution data ride as a
    /// hover tooltip on the header row.
    fn render_usage_section(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme.muted_foreground;
        let total = crate::cockpit::format_tokens(
            self.thread.read(cx).cumulative_token_usage().total_tokens(),
        );
        let main_call = self.main_call.clone();
        let side_calls = self.side_calls.clone();
        let optimization = self.optimization.clone();
        let theme_clone = theme.clone();
        let has_tooltip = main_call.is_some() || !side_calls.is_empty() || optimization.is_some();
        let header = h_flex()
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
            );
        let header: AnyElement = if has_tooltip {
            header
                .id("usage-tooltip-trigger")
                .tooltip(move |window, cx| {
                    let theme = theme_clone.clone();
                    let main_call = main_call.clone();
                    let side_calls = side_calls.clone();
                    let optimization = optimization.clone();
                    Tooltip::element(move |_w, _c| {
                        build_usage_tooltip(
                            main_call.as_ref(),
                            &side_calls,
                            optimization.as_ref(),
                            &theme,
                        )
                    })
                    .build(window, cx)
                })
                .into_any_element()
        } else {
            header.into_any_element()
        };

        // Per-model token breakdown tree: model name → input/cache row → output row.
        let thread = self.thread.read(cx);
        let per_model = thread.per_model_token_usage();
        let mut section = v_flex().w_full().gap_0p5().child(header);
        if !per_model.is_empty() {
            let mut models: Vec<(&String, &agent::language_model::TokenUsage)> =
                per_model.iter().collect();
            models.sort_by_key(|(_, u)| -(u.total_tokens() as i64));
            for (model_id, usage) in models {
                let cache_pct = crate::cockpit::cache_read_ratio(*usage);
                let model_label = if let Some(pct) = cache_pct {
                    format!(
                        "{}  {}",
                        model_id,
                        i18n::t_str(
                            "workspace-env-cache-hit-rate",
                            &[("pct", &format!("{:.0}", pct * 100.0))]
                        )
                    )
                } else {
                    model_id.clone()
                };

                let throughput = format!(
                    "{} ↑{}  {} ↑{}",
                    i18n::t("workspace-env-throughput"),
                    crate::cockpit::format_tokens(usage.input_tokens),
                    i18n::t("workspace-env-cache"),
                    crate::cockpit::format_tokens(usage.cache_read_input_tokens),
                );
                let output_line = format!(
                    "{} ↓{}",
                    i18n::t("workspace-env-output"),
                    crate::cockpit::format_tokens(usage.output_tokens),
                );

                section = section
                    .child(
                        gpui::div()
                            .text_xs()
                            .text_color(theme.foreground)
                            .truncate()
                            .child(SharedString::from(model_label)),
                    )
                    .child(
                        gpui::div()
                            .pl(px(12.))
                            .text_xs()
                            .text_color(muted)
                            .child(SharedString::from(throughput)),
                    )
                    .child(
                        gpui::div()
                            .pl(px(12.))
                            .text_xs()
                            .text_color(muted)
                            .child(SharedString::from(output_line)),
                    );
            }
        }
        section.into_any_element()
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
                        .py_0p5()
                        .pl(px(12.))
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
                append_children(Some(&info.id), agents, weak_workspace, theme, rows);
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
                        .child("Captain"),
                )
                .into_any_element(),
        ];
        append_children(None, &self.agents, &self.weak_workspace, theme, &mut rows);

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
    /// Context-window fill — one line: `Context {pct}% {used} / {cap}`.
    /// The percentage is `active / window`; the cap is the model's real
    /// max_input_tokens. Omitted when the thread has no model / zero window.
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
        );
        let muted = theme.muted_foreground;
        let warn = theme.warning;

        // The label starts at a fixed x whether or not the leading slot holds
        // the icon (kept for alignment with sibling rows).
        const LEAD_W: f32 = 14.;

        let mut rows = v_flex().w_full().gap_0p5();

        if let Some(budget) = budget {
            let pct = (budget.used_pct.round() as i64).clamp(0, 100);
            let used = crate::cockpit::format_tokens(budget.active_tokens);
            let cap = crate::cockpit::format_tokens(budget.cap_tokens);
            let near = budget.used_pct >= 90.0;
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
                            .child(format!("Context {pct}% {used} / {cap}")),
                    ),
            );
        }

        rows.into_any_element()
    }

    /// Sort key for plan steps: InProgress (0) → Pending (1) → Completed (2).
    /// Within each priority group the original chronological order is preserved
    /// by `sort_by_key`'s stable sort.
    fn plan_sort_key(status: PlanStepStatus) -> u8 {
        match status {
            PlanStepStatus::InProgress => 0,
            PlanStepStatus::Pending => 1,
            PlanStepStatus::Completed => 2,
        }
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
            // Collapsed: show the first 5 steps sorted by priority
            // (InProgress → Pending → Completed), so the rail gives a
            // glanceable overview without expanding.
            let mut section = v_flex().w_full().gap_1().child(header);
            if plan.all_completed() {
                section = section.child(
                    gpui::div()
                        .pl(px(12.))
                        .text_xs()
                        .text_color(muted)
                        .child(i18n::t("cockpit-plan-all-done")),
                );
            } else {
                let mut sorted: Vec<&agent::PlanStep> = plan.steps.iter().collect();
                sorted.sort_by_key(|s| Self::plan_sort_key(s.status));
                let remaining = total.saturating_sub(done);
                for step in sorted.iter().take(5) {
                    section = section.child(self.render_plan_row(step, theme));
                }
                let shown = sorted.len().min(5);
                if remaining > shown || plan.steps.len() > 5 {
                    section =
                        section.child(gpui::div().pl(px(12.)).text_xs().text_color(muted).child(
                            i18n::t_str(
                                "cockpit-plan-remaining",
                                &[("count", &total.saturating_sub(shown).to_string())],
                            ),
                        ));
                }
            }
            return section.into_any_element();
        }

        // Expanded: every step, in a bounded scroll region so a long plan does
        // not push the rest of the rail off-screen. Steps sort by priority:
        // InProgress first, then Pending, then Completed; original
        // chronological order is preserved within each group.
        let mut list = v_flex().w_full().gap_1();
        let mut steps: Vec<&agent::PlanStep> = plan.steps.iter().collect();
        steps.sort_by_key(|s| Self::plan_sort_key(s.status));
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

// ── Free helpers ───────────────────────────────────────────────────────────

/// Build the hover tooltip for the usage row, consolidating main calls,
/// side calls, and context optimization distribution into one tree view.
fn build_usage_tooltip(
    main_call: Option<&agent::SideCallMetric>,
    side_calls: &[agent::SideCallMetric],
    optimization: Option<&agent::ContextOptimizationMetrics>,
    theme: &Theme,
) -> AnyElement {
    let muted = theme.muted_foreground;
    let tokens = |v: u64| crate::cockpit::format_tokens(v);

    v_flex()
        .gap_1()
        .when_some(main_call, |el, metric| {
            let is_last = side_calls.is_empty() && optimization.is_none();
            el.child(section_heading(
                &i18n::t("context-tooltip-main-calls"),
                muted,
            ))
            .child(call_tree_row(metric, None, is_last, muted, &tokens))
        })
        .when(!side_calls.is_empty(), |el| {
            let has_opt = optimization.is_some();
            let section = el.child(section_heading(
                &i18n::t("context-tooltip-side-calls"),
                muted,
            ));
            let total = side_calls.len();
            side_calls
                .iter()
                .enumerate()
                .fold(section, |el, (i, metric)| {
                    let is_last_in_section = !has_opt && i == total - 1;
                    el.child(call_tree_row(
                        metric,
                        Some(&metric.purpose),
                        is_last_in_section,
                        muted,
                        &tokens,
                    ))
                })
        })
        .when_some(optimization, |el, m| {
            el.child(section_heading(
                &i18n::t("context-tooltip-distribution"),
                muted,
            ))
            .child(opt_tree_row(
                "Projection",
                &format!(
                    "Sent {} Baseline {} Saved -{}",
                    tokens(m.projected_tokens),
                    tokens(m.estimated_baseline_tokens),
                    tokens(m.saved_tokens)
                ),
                false,
                muted,
            ))
            .child(opt_tree_row(
                "Breakdown",
                &format!(
                    "System {} Mode {} Project {} Schemas {} History {} Results {}",
                    tokens(m.system_tokens),
                    tokens(m.mode_tokens),
                    tokens(m.project_context_tokens),
                    tokens(m.tool_schema_tokens),
                    tokens(m.history_tokens),
                    tokens(m.tool_result_tokens)
                ),
                false,
                muted,
            ))
            .child({
                let mut parts = vec![format!(
                    "Schemas {}/{}",
                    m.active_tool_schemas, m.total_tool_schemas
                )];
                if m.discovery_saved_tokens > 0 {
                    parts.push(format!("Discovery -{}", tokens(m.discovery_saved_tokens)));
                }
                opt_tree_row("Tools", &parts.join(" "), false, muted)
            })
            .child(opt_tree_row(
                "Runtime",
                &format!("Prefix {}%", m.prefix_stability_pct),
                true,
                muted,
            ))
        })
        .into_any_element()
}

/// Section heading in the usage tooltip (e.g. "主调用", "辅助调用", "Tokens 分布").
fn section_heading(text: &str, muted: gpui::Hsla) -> AnyElement {
    gpui::div()
        .text_xs()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(muted.opacity(0.7))
        .child(SharedString::from(text))
        .into_any_element()
}

/// A tree row for a call metric (main or side call).
fn call_tree_row(
    metric: &agent::SideCallMetric,
    purpose_prefix: Option<&str>,
    is_last: bool,
    muted: gpui::Hsla,
    tokens: &dyn Fn(u64) -> String,
) -> AnyElement {
    let prefix = if is_last { "╰─ " } else { "├─ " };
    let avg_ms = metric.latency_ms / metric.calls.max(1);
    let cache_pct = cache_read_ratio(metric.token_usage)
        .map(|r| format!("{:.0}%", r * 100.0))
        .unwrap_or_else(|| "--".into());
    let calls_unit = i18n::t("context-tooltip-calls-unit");
    let text = match purpose_prefix {
        Some(purpose) => format!(
            "{}{}·{}·{}{}·↑[{}]{} / {} ↓{}·RTT {}ms",
            prefix,
            purpose,
            metric.model,
            metric.calls,
            calls_unit,
            cache_pct,
            tokens(metric.token_usage.cache_read_input_tokens),
            tokens(metric.token_usage.input_tokens),
            tokens(metric.token_usage.output_tokens),
            avg_ms,
        ),
        None => format!(
            "{}{}·{}{}·↑[{}]{} / {} ↓{}·RTT {}ms",
            prefix,
            metric.model,
            metric.calls,
            calls_unit,
            cache_pct,
            tokens(metric.token_usage.cache_read_input_tokens),
            tokens(metric.token_usage.input_tokens),
            tokens(metric.token_usage.output_tokens),
            avg_ms,
        ),
    };
    gpui::div()
        .pl(px(8.))
        .text_xs()
        .text_color(muted)
        .child(SharedString::from(text))
        .into_any_element()
}

/// A tree row for an optimization distribution category.
fn opt_tree_row(category: &str, detail: &str, is_last: bool, muted: gpui::Hsla) -> AnyElement {
    let prefix = if is_last { "╰─ " } else { "├─ " };
    gpui::div()
        .pl(px(8.))
        .text_xs()
        .text_color(muted)
        .child(SharedString::from(format!(
            "{}{} | {}",
            prefix, category, detail
        )))
        .into_any_element()
}

/// A clickable row with an icon, label, and optional trailing element.
/// `icon_path` is a `icons/…` asset path resolved through `ExtrasAssetSource`,
/// not an `IconName`.
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
