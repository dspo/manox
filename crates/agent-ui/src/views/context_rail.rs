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

use agent::compact::MIN_COMPACTION_CONTEXT_WINDOW;
use agent::{Thread, ThreadEvent, i18n};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, ClickEvent, Context, Entity, Render,
    SharedString, Window, deferred, ease_out_quint, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TITLE_BAR_HEIGHT, Theme,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};

use crate::cockpit::{
    CockpitPhase, Milestone, MilestoneStatus, cache_read_ratio, context_budget_pct,
};
use crate::git_status::{GitBranchDisplay, GitChangeStats};

// ── Geometry ─────────────────────────────────────────────────────────────

/// Floating card width. Wide enough for the per-model usage block: model id
/// (plus trailing cache-hit badge) on the top line, `├── 穿透` (input /
/// output) and `└── 缓存` (cache create / cache read) tree rows underneath,
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

/// Right-side context sidecar. Owns the cockpit state (run phase, milestones,
/// per-cell counter animation state) and renders the environment/cockpit
/// panel that used to float as an absolute card over the conversation.
pub(crate) struct ContextRail {
    pub(crate) thread: Entity<Thread>,
    /// Coarse run phase shown in the status row. Derived from `ThreadEvent`s
    /// routed here by `Workspace`; the per-second thinking ticker's
    /// `cx.notify()` also refreshes the elapsed display.
    pub(crate) cockpit_phase: CockpitPhase,
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
    /// In-flight sub-agent tool-use ids, fed by `SubagentProgress` events.
    /// An id is inserted while its status is non-terminal and removed once the
    /// child thread reports `Success` / `Error`. Drives the cockpit's
    /// "Running N Explore agents…" aggregation row.
    pub(crate) active_agents: std::collections::HashSet<String>,
    /// Per-cell scoreboard state, keyed by `"{model_name}|{field}"` where
    /// `field ∈ {in, out, cache_create, cache_read}`. Stored as
    /// `(gen, last_value)`: `gen` is bumped on every value delta so the
    /// animation's element id changes and gpui fires a fresh 0→1 tween;
    /// `last_value` is the value the previous render committed and becomes
    /// the start of the count-up interpolation. Rebuilt every render so
    /// cells whose model disappeared (e.g. thread reset) are pruned, keeping
    /// the map bounded by the number of live models.
    pub(crate) env_counter_state: HashMap<String, (u64, u64)>,
    /// Latest git change stats for the thread's cwd. Refreshed (debounced) by
    /// `Workspace` on thread attach, terminal stop, and enter/exit worktree.
    pub(crate) git_change_stats: Option<GitChangeStats>,
    /// Latest resolved branch display for the thread's cwd. `None` until the
    /// first refresh completes; the changes/branch rows render placeholders
    /// until then.
    pub(crate) git_branch_display: Option<GitBranchDisplay>,
    /// Open branch-row context menu entity + its dismiss subscription. Created
    /// on open, dropped on close — mirrors the title-menu pattern.
    pub(crate) branch_menu: Option<Entity<gpui_component::menu::PopupMenu>>,
    branch_menu_sub: Option<gpui::Subscription>,
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
            cockpit_milestones: Vec::new(),
            cockpit_hide_tasks: false,
            cockpit_auto_compact_enabled: auto_compact_enabled,
            cockpit_auto_compact_threshold: auto_compact_threshold,
            active_agents: std::collections::HashSet::new(),
            env_counter_state: HashMap::new(),
            git_change_stats: None,
            git_branch_display: None,
            branch_menu: None,
            branch_menu_sub: None,
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
    /// milestones, running-tool title, and per-model counter state do not
    /// apply to the incoming one. Mirrors the old `Workspace::set_active_thread`
    /// reset. Also clears the cached git stats so the incoming thread shows
    /// placeholders until its own refresh lands.
    pub(crate) fn reset_for_thread_switch(&mut self, running: bool, cx: &mut Context<Self>) {
        self.env_counter_state.clear();
        self.active_agents.clear();
        self.cockpit_phase = if running {
            CockpitPhase::Streaming
        } else {
            CockpitPhase::Idle
        };
        self.cockpit_milestones = Vec::new();
        self.git_change_stats = None;
        self.git_branch_display = None;
        self.close_branch_menu();
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

    /// Drop the open branch-row context menu entity + subscription.
    pub(crate) fn close_branch_menu(&mut self) {
        self.branch_menu = None;
        self.branch_menu_sub = None;
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

    /// Track an in-flight sub-agent for the cockpit's "Running N Explore
    /// agents…" aggregation row. Non-terminal statuses insert the id; a
    /// terminal `Success` / `Error` / `Denied` / `Cancelled` removes it so
    /// the count reflects only children still working.
    pub(crate) fn record_subagent_progress(
        &mut self,
        id: &str,
        status: agent::ToolCallStatus,
        cx: &mut Context<Self>,
    ) {
        use agent::ToolCallStatus as S;
        let terminal = matches!(status, S::Success | S::Error | S::Denied | S::Cancelled);
        let changed = if terminal {
            self.active_agents.remove(id)
        } else {
            self.active_agents.insert(id.to_string())
        };
        if changed {
            cx.notify();
        }
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
            .children(self.render_active_agents_row(theme))
            .child(env_row(
                IconName::Bot,
                self.thread.read(cx).display_title().into(),
                None,
                theme,
            ))
            .child(self.render_changes_row(&project, theme))
            .child(self.render_branch_row(&project, theme, cx))
            .child(self.render_usage_section(theme, cx, per_model))
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

    /// Per-model token usage as a two-row tree: `├── 穿透` (input / output) and
    /// `└── 缓存` (cache create / cache read), each with `↑↓` animated counters.
    /// The throughput / cache split keeps the cache-read share legible as a
    /// branch rather than burying it among four flat rows. A cache-hit ratio
    /// rides beside the model name when the model has any cache-readable input.
    /// Counter animation state is diffed and rebuilt here so the per-cell `gen`
    /// bumps on every value delta (see `env_counter_state`).
    fn render_usage_section(
        &mut self,
        theme: &Theme,
        _cx: &mut Context<Self>,
        per_model: HashMap<String, agent::language_model::TokenUsage>,
    ) -> AnyElement {
        let muted = theme.muted_foreground;
        let mut model_rows: Vec<_> = per_model
            .into_iter()
            .filter(|(_, u)| u.total_tokens() > 0)
            .collect();
        model_rows.sort_by_key(|b| std::cmp::Reverse(b.1.total_tokens()));

        let throughput_label = i18n::t("workspace-env-throughput");
        let cache_label = i18n::t("workspace-env-cache");

        let mut new_state: HashMap<String, (u64, u64)> = HashMap::new();
        // Each model block carries: model id text + the two tree rows
        // (throughput, cache). The capture-based build keeps the closure
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
                    None => (0u64, 1u64),
                    Some(&(gen_, last)) if last == *value => (last, gen_),
                    Some(&(gen_, last)) => (last, gen_.wrapping_add(1)),
                };
                new_state.insert(cell_key, (new_gen, *value));
                from_to_gen[i] = (old_value, *value, new_gen);
            }
            let (in_f, in_t, in_g) = from_to_gen[0];
            let (out_f, out_t, out_g) = from_to_gen[1];
            let (cce_f, cce_t, cce_g) = from_to_gen[2];
            let (ccr_f, ccr_t, ccr_g) = from_to_gen[3];

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
                .child(counter_animated("↓", out_f, out_t, "out", out_g));
            let cache_row = h_flex()
                .pl(px(12.))
                .gap_1()
                .items_center()
                .text_xs()
                .text_color(muted)
                .child(branch_prefix("└── "))
                .child(cache_label.clone())
                .child(counter_animated("+", cce_f, cce_t, "cache_create", cce_g))
                .child(counter_animated("=", ccr_f, ccr_t, "cache_read", ccr_g));
            model_blocks.push((model_display, hit_rate, throughput_row, cache_row));
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
                    ),
            )
            .children(model_blocks.into_iter().map(
                |(model_display, hit_rate, throughput_row, cache_row)| {
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
                        .child(cache_row)
                        .into_any_element()
                },
            ))
            .into_any_element()
    }

    /// Run-status block: a multi-line card so a long phase / tool title is
    /// not cramped into one truncated line. Line 1 carries the phase label
    /// (semibold) + elapsed + cumulative tokens; line 2 is the elapsed/tokens
    /// meta in xs muted; line 3 (when a tool is running) shows the tool title
    /// in xs muted truncate. The elapsed still refreshes per-second via the
    /// thinking ticker's `cx.notify()`.
    /// Changes row: `+added` (green) / `-deleted` (red) plus an untracked
    /// count badge when there are untracked files. Before the first git
    /// refresh lands (or when no project is bound) the trailing slot shows
    /// `--` so the row keeps its height instead of flickering.
    fn render_changes_row(&self, project: &Option<PathBuf>, theme: &Theme) -> AnyElement {
        let Some(stats) = self.git_change_stats.as_ref() else {
            let trailing = if project.is_some() {
                SharedString::from("--")
            } else {
                i18n::t("workspace-env-no-project")
            };
            return env_row(
                IconName::Frame,
                i18n::t("workspace-env-changes"),
                Some(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(trailing)
                        .into_any_element(),
                ),
                theme,
            );
        };
        let added = format!("+{}", stats.added);
        let deleted = format!("-{}", stats.deleted);
        let trailing = h_flex()
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
            });
        env_row(
            IconName::Frame,
            i18n::t("workspace-env-changes"),
            Some(trailing.into_any_element()),
            theme,
        )
    }

    /// Branch row. Renders the resolved branch (or detached short sha, or a
    /// not-a-repo label) with a `(worktree)` suffix when the thread is inside a
    /// git worktree. Clicking opens a context menu to copy the branch name /
    /// worktree path or to exit the worktree — it never exits directly, so a
    /// stray click cannot destroy the isolation context.
    fn render_branch_row(
        &mut self,
        project: &Option<PathBuf>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let display = self.git_branch_display.clone();
        let is_worktree = self.thread.read(cx).worktree().is_some();

        let label: SharedString = match &display {
            Some(d) if d.is_no_repo() => i18n::t("workspace-env-git-not-a-repo"),
            Some(d) => {
                let mut s = d
                    .branch
                    .clone()
                    .or_else(|| d.detached_sha.clone())
                    .map(SharedString::from)
                    .unwrap_or_else(|| i18n::t("workspace-env-git-unavailable"));
                // A detached HEAD shows the short sha plus a muted "(detached)"
                // hint so the row reads honestly rather than looking like a
                // branch name.
                if d.branch.is_none() && d.detached_sha.is_some() {
                    s = SharedString::from(format!(
                        "{} {}",
                        s,
                        i18n::t("workspace-env-git-detached")
                    ));
                }
                if d.is_worktree || is_worktree {
                    s = SharedString::from(format!(
                        "{} {}",
                        s,
                        i18n::t("workspace-env-git-worktree-suffix")
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

        let menu_open = self.branch_menu.is_some();
        let trigger = env_row_clickable(
            IconName::Github,
            label,
            None,
            theme,
            menu_open,
            cx.listener(move |this, _: &ClickEvent, window, cx| {
                if this.branch_menu.is_some() {
                    this.close_branch_menu();
                    cx.notify();
                    return;
                }
                let rail = cx.entity().downgrade();
                let branch = this
                    .git_branch_display
                    .as_ref()
                    .and_then(|d| d.branch.clone());
                let worktree_path = this.thread.read(cx).worktree().map(|w| w.path.clone());
                let in_worktree = this.thread.read(cx).worktree().is_some();
                let thread = this.thread.clone();
                let menu =
                    gpui_component::menu::PopupMenu::build(window, cx, move |menu, _w, _cx| {
                        let mut menu = menu.label(i18n::t("workspace-env-changes"));
                        if let Some(b) = &branch {
                            let b = b.clone();
                            let r = rail.clone();
                            menu = menu.item(
                                gpui_component::menu::PopupMenuItem::new(i18n::t(
                                    "workspace-env-git-copy-branch",
                                ))
                                .on_click(move |_, _, cx| {
                                    let _ = r.update(cx, |this, cx| {
                                        this.copy_to_clipboard(b.clone(), cx);
                                        this.close_branch_menu();
                                        cx.notify();
                                    });
                                }),
                            );
                        }
                        if let Some(p) = &worktree_path {
                            let p = p.clone();
                            let r = rail.clone();
                            menu = menu.item(
                                gpui_component::menu::PopupMenuItem::new(i18n::t(
                                    "workspace-env-git-copy-path",
                                ))
                                .on_click(move |_, _, cx| {
                                    let _ = r.update(cx, |this, cx| {
                                        this.copy_to_clipboard(p.display().to_string(), cx);
                                        this.close_branch_menu();
                                        cx.notify();
                                    });
                                }),
                            );
                        }
                        if in_worktree {
                            let t = thread.clone();
                            let r = rail.clone();
                            menu = menu.separator().item(
                                gpui_component::menu::PopupMenuItem::new(i18n::t(
                                    "workspace-env-git-exit-worktree",
                                ))
                                .on_click(move |_, _, cx| {
                                    let _ = r.update(cx, |this, cx| {
                                        this.close_branch_menu();
                                        t.update(cx, |thread, cx| {
                                            let _ = thread.exit_worktree(cx);
                                        });
                                        cx.notify();
                                    });
                                }),
                            );
                        }
                        menu
                    });
                let sub = cx.subscribe(
                    &menu,
                    |this: &mut ContextRail,
                     _menu: Entity<gpui_component::menu::PopupMenu>,
                     _: &gpui::DismissEvent,
                     cx: &mut Context<ContextRail>| {
                        this.close_branch_menu();
                        cx.notify();
                    },
                );
                this.branch_menu = Some(menu);
                this.branch_menu_sub = Some(sub);
                cx.notify();
            }),
        );

        if !menu_open {
            return trigger;
        }
        let menu = self
            .branch_menu
            .clone()
            .expect("branch_menu exists when open");
        // `deferred` + `with_priority(1)` paints the dropdown after the whole
        // rail panel tree, so it floats above the usage/budget/milestone rows
        // that follow the branch row instead of being overpainted by them.
        gpui::div()
            .relative()
            .child(trigger)
            .child(
                deferred(
                    gpui::div()
                        .id("branch-dropdown")
                        .absolute()
                        .top_full()
                        .left_0()
                        .occlude()
                        .child(menu),
                )
                .with_priority(1),
            )
            .into_any_element()
    }

    /// Copy a string to the clipboard. Silent success — clipboard writes need
    /// no separate UI feedback; the branch menu closes on click.
    fn copy_to_clipboard(&self, value: String, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(value));
    }

    /// Aggregation row shown while sub-agents are in flight: "Running N
    /// Explore agents…". Rendered only when `active_agents` is non-empty, so
    /// the row vanishes the moment the last child reports a terminal status.
    fn render_active_agents_row(&self, theme: &Theme) -> Option<AnyElement> {
        let n = self.active_agents.len();
        if n == 0 {
            return None;
        }
        Some(
            h_flex()
                .items_center()
                .gap_1p5()
                .child(Icon::new(IconName::Bot).text_color(theme.primary).size_3())
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(i18n::t_count("agent-metrics-running-agents", n as i64)),
                )
                .into_any_element(),
        )
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
        let meta = i18n::t_str(
            "cockpit-run-status-meta",
            &[("elapsed", elapsed.as_str()), ("tokens", tokens.as_str())],
        );
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
        cockpit_status_block(icon, phase_label.into(), meta, theme)
    }

    /// Context-budget row. Reads the thread's cumulative usage so the display
    /// reflects the conversation's token spend and stays stable across turn
    /// warm-up — never a "waiting" placeholder. Hidden (no row) only when the
    /// thread has no model with a known window. The trailing element renders
    /// the explicit `current / cap` token counts so the user can read the
    /// absolute numbers behind the percentage.
    fn render_cockpit_context_budget(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let thread = self.thread.read(cx);
        let max_input = thread.model().map(|m| m.max_token_count()).unwrap_or(0);
        let budget = context_budget_pct(
            max_input,
            thread.latest_request_usage().unwrap_or_default(),
            self.cockpit_auto_compact_enabled,
            self.cockpit_auto_compact_threshold,
        );
        let Some(budget) = budget else {
            // No model / zero window: nothing honest to show, so omit the row
            // rather than invent a placeholder.
            return gpui::div().into_any_element();
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
        let current = crate::cockpit::format_tokens(budget.active_tokens);
        let cap = crate::cockpit::format_tokens(budget.cap_tokens);
        let ratio = format!("{current} / {cap}");
        env_row(
            IconName::BatteryFull,
            label,
            Some(
                h_flex()
                    .gap_1()
                    .text_xs()
                    .text_color(color)
                    .child(gpui::div().child(ratio))
                    .child(
                        gpui::div()
                            .text_color(theme.muted_foreground.opacity(0.7))
                            .child(i18n::t("cockpit-context-estimate")),
                    )
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

/// Multi-line run-status block. Replaces the single-line `env_row` the status
/// row used to occupy: phase on a semibold line, an xs muted meta line
/// (elapsed + tokens). The tool title is intentionally omitted — a long title
/// never fits one line, so the row stays a phase + meta summary only. The icon
/// anchors the left column.
fn cockpit_status_block(
    icon: IconName,
    phase: SharedString,
    meta: SharedString,
    theme: &Theme,
) -> AnyElement {
    h_flex()
        .w_full()
        .items_start()
        .gap_2()
        .child(Icon::new(icon).xsmall().text_color(theme.muted_foreground))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    gpui::div()
                        .text_sm()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .text_color(theme.foreground)
                        .child(phase),
                )
                .child(
                    gpui::div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(meta),
                ),
        )
        .into_any_element()
}

/// A clickable variant of [`env_row`]: the whole row is a pointer cursor with
/// an `on_click` handler, and tints the icon foreground when `open` so the
/// affordance matches an open dropdown below it. Used by the branch row to
/// open its context menu.
fn env_row_clickable(
    icon: IconName,
    label: SharedString,
    trailing: Option<AnyElement>,
    theme: &Theme,
    open: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let icon_color = if open {
        theme.accent
    } else {
        theme.muted_foreground
    };
    h_flex()
        .id("env-row-clickable")
        .w_full()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .on_click(on_click)
        .child(Icon::new(icon).xsmall().text_color(icon_color))
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
