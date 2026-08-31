//! Right-hand context rail: a stable sidecar showing the active thread's
//! environment/cockpit information (run status, changes, branch, per-model
//! token usage, context budget, execution plan, sources).
//!
//! The rail is a first-class view owned by [`crate::Workspace`]. It holds the
//! cockpit state (run phase, the model's plan snapshot, per-cell counter
//! animation state)
//! that used to live directly on `Workspace`, plus strong handles to the
//! active gpui-free `ThreadHandle` and the
//! AgentServer-backed store mirror [`crate::ConversationState`] it renders
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

use crate::client_store_handle::ClientStoreHandle;
use crate::i18n;
use agent::ThreadEvent;
use gpui::{
    AnyElement, App, ClickEvent, ClipboardItem, Context, Entity, MouseButton, MouseUpEvent, Render,
    SharedString, WeakEntity, Window, prelude::*, px,
};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, TITLE_BAR_HEIGHT, Theme, WindowExt as _,
    h_flex, notification::Notification, tooltip::Tooltip, v_flex,
};
use std::collections::HashMap;
use std::path::PathBuf;

use agent::{PlanSnapshot, PlanStepStatus};

use crate::Workspace;
use crate::cockpit::{CockpitPhase, cache_read_ratio, context_budget_pct, format_cache_hit};
use crate::git_status::{GitBranchDisplay, GitChangeStats};
use crate::views::subagents::{SubagentInfo, status_indicator, subagent_display_title};

// ── Geometry ─────────────────────────────────────────────────────────────

/// Floating card width. Wide enough for the per-model usage block: model id
/// on the top line, then `├─ pct% used/cap` and `└─ ↑input ↓output Rcache
/// CHhit%` tree rows underneath.
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
    pub(crate) thread: agent::thread::ThreadHandle,
    /// γ-2a transitional read path: the AgentServer-backed store mirroring
    /// kernel state via `ServerNote`s. `None` when the workspace has not
    /// created the AgentServer connection; every kernel-state read dual-reads
    /// this store first and falls back to `self.thread`. Mutations keep going
    /// through `self.thread`.
    store: Option<Entity<ClientStoreHandle>>,
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
    agents: Vec<SubagentInfo>,
    pub(crate) side_calls: Vec<agent::SideCallMetric>,
    pub(crate) main_call: Option<agent::SideCallMetric>,
    /// Latest git change stats for the thread's cwd. Refreshed (debounced) by
    /// `Workspace` on thread attach and terminal stop.
    pub(crate) git_change_stats: Option<GitChangeStats>,
    /// Latest resolved branch display for the thread's cwd. `None` until the
    /// first refresh completes; the changes/branch rows render placeholders
    /// until then.
    pub(crate) git_branch_display: Option<GitBranchDisplay>,
    /// Reaches the workspace so agent rows can open their observation panel.
    weak_workspace: WeakEntity<Workspace>,
}

impl ContextRail {
    pub(crate) fn new(
        thread: agent::thread::ThreadHandle,
        store: Option<Entity<ClientStoreHandle>>,
    ) -> Self {
        Self {
            thread,
            store,
            cockpit_phase: CockpitPhase::Idle,
            plan: None,
            cockpit_hide_tasks: false,
            plan_seen: false,
            agents: Vec::new(),
            side_calls: Vec::new(),
            main_call: None,
            git_change_stats: None,
            git_branch_display: None,
            weak_workspace: WeakEntity::new_invalid(),
        }
    }

    /// Injected by the owning workspace after construction so agent rows can
    /// open their observation panel.
    pub(crate) fn set_workspace(&mut self, weak: WeakEntity<Workspace>) {
        self.weak_workspace = weak;
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

    /// Upsert one sub-agent observation row from a `SubagentProgress` event
    /// (the pi harness emits these around its ephemeral nested sessions;
    /// the retired manox harness maintained its list from child threads
    /// instead). `health` carries the watchdog's one-line verdict while the
    /// run is live; `None` leaves the stored verdict untouched.
    pub(crate) fn apply_subagent_progress(
        &mut self,
        id: &str,
        subagent_type: &str,
        description: Option<&str>,
        status: agent::ToolCallStatus,
        health: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        if let Some(info) = self.agents.iter_mut().find(|info| info.id == id) {
            info.status = status;
            if !subagent_type.is_empty() {
                info.subagent_type = subagent_type.to_string();
            }
            if let Some(description) = description
                && info.description.is_empty()
            {
                info.description = description.to_string();
            }
            if let Some(health) = health {
                info.health = Some(health.to_string());
            }
        } else {
            self.agents.push(SubagentInfo {
                id: id.to_string(),
                subagent_type: subagent_type.to_string(),
                description: description.unwrap_or_default().to_string(),
                status,
                health: health.map(str::to_string),
            });
        }
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
        let project = self
            .store
            .as_ref()
            .map(|s| std::path::PathBuf::from(s.read(cx).store.cwd.clone()));
        let agents_section = self.render_agents_section(theme, cx);

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
            .child(agents_section)
            .child(self.render_branch_block(&project, theme, cx))
            .child(self.render_usage_section(theme, cx))
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
    /// and side calls.
    /// Usage section: a header row (icon + "消费" + total tokens), followed by
    /// a per-model token breakdown tree when per-model data is available. Each
    /// model node now carries a tree prefix (├─ / └─), shows its context-window
    /// size as `[1m]`, and integrates its context-budget fill as the first
    /// tree child (Context X% used/cap). Input/cache and output are the second
    /// and third children.
    fn render_usage_section(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme.muted_foreground;
        let total = crate::cockpit::format_tokens(
            self.store
                .as_ref()
                .map(|s| {
                    s.read(cx)
                        .store
                        .cumulative_usage
                        .as_ref()
                        .map(|u| u.input + u.output)
                        .unwrap_or(0)
                })
                .unwrap_or(0),
        );
        // Rate-card cost (#418 wire-boundary pricing); backends/sessions
        // without pricing keep the tokens-only header.
        let cumulative_cost = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.cumulative_cost)
            .unwrap_or(0.0);
        let total = if cumulative_cost > 0.0 {
            SharedString::from(format!("{total} · {}", format_cost(cumulative_cost)))
        } else {
            SharedString::from(total)
        };
        let main_call = self.main_call.clone();
        let side_calls = self.side_calls.clone();
        let theme_clone = theme.clone();
        let has_tooltip = main_call.is_some() || !side_calls.is_empty();
        let header = h_flex()
            .items_center()
            .gap_2()
            .child(
                Icon::default()
                    .path("icons/zodiac-scorpio.svg")
                    .xsmall()
                    .text_color(muted),
            )
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
                    .child(total),
            );
        let header: AnyElement = if has_tooltip {
            header
                .id("usage-tooltip-trigger")
                .tooltip(move |window, cx| {
                    let theme = theme_clone.clone();
                    let main_call = main_call.clone();
                    let side_calls = side_calls.clone();
                    Tooltip::element(move |_w, _c| {
                        build_usage_tooltip(main_call.as_ref(), &side_calls, &theme)
                    })
                    .build(window, cx)
                })
                .into_any_element()
        } else {
            header.into_any_element()
        };

        // Per-model token breakdown tree with context budget integrated.
        let s = &self
            .store
            .as_ref()
            .expect("foreground store present")
            .read(cx)
            .store;
        let per_model = s
            .per_model_usage
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    agent::TokenUsage {
                        input_tokens: v.input,
                        output_tokens: v.output,
                        cache_creation_input_tokens: v.cache_creation,
                        cache_read_input_tokens: v.cache_read,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let per_model_last = s
            .per_request_usage
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    agent::TokenUsage {
                        input_tokens: v.input,
                        output_tokens: v.output,
                        cache_creation_input_tokens: v.cache_creation,
                        cache_read_input_tokens: v.cache_read,
                    },
                )
            })
            .collect::<HashMap<_, _>>();
        let per_model_cost = s.per_model_cost.clone();
        let warn_color = theme.warning;
        let mut section = v_flex().w_full().gap_0p5().child(header);
        if !per_model.is_empty() {
            let mut models: Vec<(&String, &agent::language_model::TokenUsage)> =
                per_model.iter().collect();
            models.sort_by_key(|(_, u)| -(u.total_tokens() as i64));
            let total_models = models.len();
            for (i, (model_name, usage)) in models.iter().enumerate() {
                let is_last_model = i == total_models - 1;
                // Tree glyphs: model node gets the root branch glyph; children
                // share a 4-column indent prefix (vertical line or blank) plus
                // their own branch glyph.
                let branch = if is_last_model { "└─" } else { "├─" };
                let indent = if is_last_model { "    " } else { "│   " };

                // Model row: "{provider display}/{model display}" with the
                // model segment tinted by its wire api; the raw composite key
                // renders verbatim when the registry cannot resolve it.
                let model_row = match model_name.split_once('/').and_then(|(provider, id)| {
                    agent::pi_providers::global().resolve_model(provider, id)
                }) {
                    Some(m) => h_flex()
                        .text_xs()
                        .min_w_0()
                        .child(
                            gpui::div()
                                .flex_none()
                                .text_color(theme.foreground)
                                .child(format!("{branch} ")),
                        )
                        .child(
                            gpui::div()
                                .flex_none()
                                .text_color(theme.muted_foreground)
                                .child(agent::pi_providers::display_provider_name(&m)),
                        )
                        .child(
                            gpui::div()
                                .flex_none()
                                .text_color(theme.muted_foreground)
                                .child("/"),
                        )
                        .child(
                            gpui::div()
                                .min_w_0()
                                .truncate()
                                .text_color(Workspace::pi_wire_text_color(&m.api, theme))
                                .child(agent::pi_providers::display_name(&m)),
                        )
                        .into_any_element(),
                    None => gpui::div()
                        .text_xs()
                        .text_color(theme.foreground)
                        .truncate()
                        .child(SharedString::from(format!("{branch} {model_name}")))
                        .into_any_element(),
                };
                section = section.child(model_row);

                // Context budget row — first tree child, only when the model is
                // registered (so its window size is resolvable).
                let window_tokens = model_window_tokens(model_name);
                let budget = window_tokens.and_then(|cap| {
                    per_model_last.get(*model_name).and_then(|u| {
                        let active = u
                            .input_tokens
                            .saturating_add(u.cache_creation_input_tokens)
                            .saturating_add(u.cache_read_input_tokens);
                        context_budget_pct(cap, active)
                    })
                });
                if let Some(budget) = budget {
                    let used = crate::cockpit::format_tokens_pi(budget.active_tokens);
                    let cap = crate::cockpit::format_tokens_pi(budget.cap_tokens);
                    let near_full = budget.used_pct >= 90.0;
                    let ctx_color = if near_full { warn_color } else { muted };
                    section = section.child(
                        gpui::div()
                            .text_xs()
                            .text_color(ctx_color)
                            .truncate()
                            .child(SharedString::from(format!(
                                "{indent}├─ {:.1}% {used}/{cap}",
                                budget.used_pct
                            ))),
                    );
                }

                // Token line: ↑input ↓output Rcache_read CHcache_hit_rate. `--`
                // (the tooltip convention) when there is no input to measure.
                // With a priced model the cost row follows as the last child.
                let cache_hit = crate::cockpit::cache_read_ratio(**usage)
                    .map(|r| format_cache_hit(r, 1))
                    .unwrap_or_else(|| "--".into());
                let cost = per_model_cost.get(*model_name).copied().unwrap_or(0.0);
                let token_branch = if cost > 0.0 { "├─" } else { "└─" };
                section = section.child(gpui::div().text_xs().text_color(muted).truncate().child(
                    SharedString::from(format!(
                        "{indent}{token_branch} ↑{} ↓{} R{} CH{}",
                        crate::cockpit::format_tokens_pi(usage.input_tokens),
                        crate::cockpit::format_tokens_pi(usage.output_tokens),
                        crate::cockpit::format_tokens_pi(usage.cache_read_input_tokens),
                        cache_hit,
                    )),
                ));
                if cost > 0.0 {
                    section =
                        section.child(gpui::div().text_xs().text_color(muted).truncate().child(
                            SharedString::from(format!("{indent}└─ {}", format_cost(cost))),
                        ));
                }
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

    /// Branch block: (1) the working-directory basename, shown only while the
    /// session's effective cwd is reported — click copies the name, double-click copies
    /// the absolute path; (2) the branch row — resolved branch or detached sha
    /// (+ "(detached)") as the label with the changes counts as its right-aligned
    /// trailing. Both rows copy on click with a notification for feedback.
    fn render_branch_block(
        &mut self,
        project: &Option<PathBuf>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The store mirrors the session's effective cwd as a string.
        let cwd_path = self
            .store
            .as_ref()
            .and_then(|s| s.read(cx).store.cwd_path.clone());
        let display = self.git_branch_display.clone();

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
        if let Some(path) = cwd_path {
            // The dual-read yields a path string; re-path it for the
            // `file_name`/`display` calls below.
            let path = PathBuf::from(path);
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

    /// Status indicator for the Captain (main agent) row.
    /// The Captain uses `ship-wheel` for the completed state, distinguishing it
    /// from sub-agents that use `circle-check-big`.
    fn captain_status_indicator(status: agent::ToolCallStatus, theme: &Theme) -> AnyElement {
        use agent::ToolCallStatus;
        match status {
            ToolCallStatus::PendingApproval | ToolCallStatus::Running => {
                crate::views::braille_spinner::BrailleSpinner::new()
                    .xsmall()
                    .color(theme.accent_foreground)
                    .into_any_element()
            }
            ToolCallStatus::Success | ToolCallStatus::Continued => Icon::default()
                .path("icons/ship-wheel.svg")
                .xsmall()
                .text_color(theme.success)
                .into_any_element(),
            ToolCallStatus::Error | ToolCallStatus::Denied => Icon::new(IconName::CircleX)
                .xsmall()
                .text_color(theme.danger)
                .into_any_element(),
            ToolCallStatus::Cancelled => Icon::new(IconName::Minus)
                .xsmall()
                .text_color(theme.muted_foreground)
                .into_any_element(),
        }
    }

    /// Agents observation section: Captain (main thread) row plus one row per
    /// sub-agent the pi harness reported via `SubagentProgress`. Rows are
    /// observe-only — the retired manox harness drilled into per-child panels,
    /// but pi sub-agents are ephemeral nested sessions with no child-thread
    /// entity to open.
    fn render_agents_section(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let running = self
            .store
            .as_ref()
            .map(|s| s.read(cx).store.running)
            .unwrap_or(false);
        let main_status = if self.cockpit_phase == CockpitPhase::Failed {
            agent::ToolCallStatus::Error
        } else if running {
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
                .child(Self::captain_status_indicator(main_status, theme))
                .child(gpui::div().text_xs().text_color(theme.foreground).child(
                    crate::views::message::author_display(&agent::MessageAuthor::Lead),
                ))
                .into_any_element(),
        ];
        // Flat list: pi sub-agents are ephemeral nested sessions that never
        // nest deeper than one level, so no tree recursion is needed.
        for info in &self.agents {
            let title = subagent_display_title(info);
            let tooltip_text = match &info.health {
                Some(health) => format!("{title} — {health}"),
                None => title.clone(),
            };
            let weak = self.weak_workspace.clone();
            let id = info.id.clone();
            let subagent_type = info.subagent_type.clone();
            let topic = info.description.clone();
            let status = info.status;
            // Health verdict coloring: stalls warn, loops alarm, plain
            // activity stays muted.
            let health_color = info.health.as_deref().map(|h| {
                if h.starts_with("stalled") {
                    theme.warning
                } else if h.starts_with("looping") {
                    theme.danger
                } else {
                    theme.muted_foreground
                }
            });
            let health = info.health.clone();
            let row = h_flex()
                .id(SharedString::from(format!("context-agent-{}", info.id)))
                .w_full()
                .min_w_0()
                .py_0p5()
                .pl(px(12.))
                .gap_1p5()
                .items_center()
                .rounded(px(4.))
                .cursor_pointer()
                .tooltip(move |window, cx| Tooltip::new(tooltip_text.clone()).build(window, cx))
                .on_click(move |_, _window, cx| {
                    if let Some(ws) = weak.upgrade() {
                        ws.update(cx, |ws, cx| {
                            ws.open_subagent_tab(&id, &subagent_type, &topic, status, cx);
                        });
                    }
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
                .when_some(health.zip(health_color), |el, (health, color)| {
                    el.child(
                        gpui::div()
                            .max_w(px(160.))
                            .truncate()
                            .text_xs()
                            .text_color(color)
                            .child(health),
                    )
                });
            rows.push(row.into_any_element());
        }

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

/// Build the hover tooltip for the usage row, consolidating main and side
/// calls into one tree view.
fn build_usage_tooltip(
    main_call: Option<&agent::SideCallMetric>,
    side_calls: &[agent::SideCallMetric],
    theme: &Theme,
) -> AnyElement {
    let muted = theme.muted_foreground;
    let tokens = |v: u64| crate::cockpit::format_tokens(v);

    v_flex()
        .gap_1()
        .when_some(main_call, |el, metric| {
            let is_last = side_calls.is_empty();
            el.child(section_heading(
                &i18n::t("context-tooltip-main-calls"),
                muted,
            ))
            .child(call_tree_row(metric, None, is_last, muted, &tokens))
        })
        .when(!side_calls.is_empty(), |el| {
            let section = el.child(section_heading(
                &i18n::t("context-tooltip-side-calls"),
                muted,
            ));
            let total = side_calls.len();
            side_calls
                .iter()
                .enumerate()
                .fold(section, |el, (i, metric)| {
                    let is_last_in_section = i == total - 1;
                    el.child(call_tree_row(
                        metric,
                        Some(&metric.purpose),
                        is_last_in_section,
                        muted,
                        &tokens,
                    ))
                })
        })
        .into_any_element()
}

/// Section heading in the usage tooltip (e.g. "主调用", "辅助调用").
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
        .map(|r| format_cache_hit(r, 0))
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

/// USD cost for the rail: `$1.23` at a dollar and above, three decimals for
/// cents, four below a cent — rate cards price per 1M tokens, so sub-cent
/// totals are the common case early in a session.
fn format_cost(cost: f64) -> String {
    if cost >= 1.0 {
        format!("${cost:.2}")
    } else if cost >= 0.01 {
        format!("${cost:.3}")
    } else {
        format!("${cost:.4}")
    }
}

/// Context window for a model name on the usage rail: the pi build probes
/// the shared pi provider registry (by wire id, then display name); the
/// retired manox build probes its own registry.
fn model_window_tokens(model_name: &str) -> Option<u64> {
    {
        let registry = agent::pi_providers::global();
        // per_model keys are composite "{provider}/{model_id}"; resolve O(1).
        // Bare ids (legacy keys) fall through to the scan below.
        if let Some((provider, id)) = model_name.split_once('/')
            && let Some(m) = registry.resolve_model(provider, id)
        {
            return Some(m.context_window as u64);
        }
        registry
            .models()
            .iter()
            .find(|m| {
                m.id == model_name
                    || m.metadata
                        .get("name")
                        .and_then(|v| v.as_str())
                        .is_some_and(|name| name == model_name)
            })
            .map(|m| m.context_window as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::format_cost;

    #[test]
    fn format_cost_tiers() {
        assert_eq!(format_cost(1.2345), "$1.23");
        assert_eq!(format_cost(0.0234), "$0.023");
        assert_eq!(format_cost(0.000123), "$0.0001");
        assert_eq!(format_cost(12.0), "$12.00");
        assert_eq!(format_cost(0.01), "$0.010");
    }
}
