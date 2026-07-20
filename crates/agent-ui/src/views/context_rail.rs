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

use agent::{Thread, ThreadEvent, i18n};
use gpui::{
    Animation, AnimationExt as _, AnyElement, App, ClickEvent, Context, Entity, Render,
    SharedString, WeakEntity, Window, deferred, ease_out_quint, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TITLE_BAR_HEIGHT, Theme, h_flex,
    tooltip::Tooltip, v_flex,
};

use crate::Workspace;
use crate::cockpit::{
    CockpitPhase, Milestone, MilestoneStatus, cache_read_ratio, cockpit_phase_tag,
    context_budget_pct,
};
use crate::git_status::{GitBranchDisplay, GitChangeStats};
use crate::views::subagent_panel::{SubagentInfo, status_indicator, subagent_display_title};

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
    /// Last tag index the status-row slider committed. The slider animates from
    /// this to the freshly computed [`cockpit_phase_tag`] index whenever the
    /// phase drifts to a different tag; `cockpit_tag_gen` bumps to force a fresh
    /// 0→1 tween. Computed in render (not on every phase write) so the
    /// workspace's direct `cockpit_phase = …` assignments need no hook.
    pub(crate) cockpit_tag_prev: u8,
    pub(crate) cockpit_tag_gen: u64,
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
        weak_workspace: WeakEntity<Workspace>,
        auto_compact_enabled: bool,
        auto_compact_threshold: f64,
    ) -> Self {
        Self {
            thread,
            cockpit_phase: CockpitPhase::Idle,
            cockpit_tag_prev: cockpit_phase_tag(CockpitPhase::Idle),
            cockpit_tag_gen: 0,
            cockpit_milestones: Vec::new(),
            cockpit_hide_tasks: false,
            cockpit_auto_compact_enabled: auto_compact_enabled,
            cockpit_auto_compact_threshold: auto_compact_threshold,
            weak_workspace,
            agents: Vec::new(),
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
        self.agents.clear();
        let new_phase = if running {
            CockpitPhase::Streaming
        } else {
            CockpitPhase::Idle
        };
        self.cockpit_phase = new_phase;
        self.cockpit_tag_prev = cockpit_phase_tag(new_phase);
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

    pub(crate) fn set_agents(&mut self, agents: Vec<SubagentInfo>, cx: &mut Context<Self>) {
        self.agents = agents;
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

    /// Branch block: (1) the worktree directory basename in foreground, shown
    /// only while the thread is inside a worktree; (2) the branch row —
    /// resolved branch or detached sha (+ "(detached)") as the label with the
    /// changes counts as its right-aligned trailing. The branch line is the
    /// clickable affordance for the copy / exit-worktree context menu — it
    /// never exits directly, so a stray click cannot destroy the isolation
    /// context.
    fn render_branch_block(
        &mut self,
        project: &Option<PathBuf>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let display = self.git_branch_display.clone();
        let worktree_path = self.thread.read(cx).worktree().map(|w| w.path.clone());

        // Branch label (line 2): branch / detached sha + (detached). No
        // worktree suffix — that lives on its own line above when present.
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

        let menu_open = self.branch_menu.is_some();
        let trigger = env_row_clickable(
            IconName::Github,
            branch_label,
            Some(changes_line),
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

        let mut block = v_flex().w_full().gap_0p5();
        if let Some(path) = worktree_path {
            let basename = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            block = block.child(
                gpui::div()
                    .w_full()
                    .truncate()
                    .text_xs()
                    .text_color(theme.foreground)
                    .child(SharedString::from(basename)),
            );
        }
        block = block.child(trigger);

        if !menu_open {
            return block.into_any_element();
        }
        let menu = self
            .branch_menu
            .clone()
            .expect("branch_menu exists when open");
        // `deferred` + `with_priority(1)` paints the dropdown after the whole
        // rail panel tree, so it floats above the usage/budget/milestone rows
        // that follow the branch block instead of being overpainted by them.
        gpui::div()
            .relative()
            .child(block)
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

/// Multi-line run-status block. The phase slot is now an arbitrary element
/// (the sliding three-tag pill) rather than a plain label, so the caller owns
/// Status block: a leading xs icon plus the phase element (the sliding
/// three-tag pill), vertically centered on a single baseline like the other
/// rail rows.
fn cockpit_status_block(icon: IconName, phase: AnyElement, theme: &Theme) -> AnyElement {
    h_flex()
        .w_full()
        .items_center()
        .gap_2()
        .child(Icon::new(icon).xsmall().text_color(theme.muted_foreground))
        .child(v_flex().flex_1().min_w_0().child(phase))
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
