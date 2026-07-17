//! Collaboration modes — the design manox ports from codex.
//!
//! A thread is always in exactly one [`ModeKind`]. The mode carries per-mode
//! overrides for `model`, `reasoning_effort`, and `developer_instructions`,
//! resolved at request-build time as a fixed-position `<collaboration_mode>`
//! message (see `thread::build_completion_request`) — never woven into the
//! system prompt, so the provider prefix cache stays warm across mode-stable
//! turns. In Plan mode the tool set is filtered to read-only and the model
//! submits a plan by emitting a single `<proposed_plan>` block
//! ([`crate::stream_parser`]), not by calling a submit tool.
//!
//! Faithful adaptation of codex's `config_types` / `collaboration_mode_presets`,
//! reusing manox's own [`ReasoningEffort`]. Two confirmed deviations from codex:
//! manox ships only `Default` + `Plan` (codex hides `PairProgramming`/`Execute`
//! behind `skip`); and [`ModeKind::allows_user_input`] returns `true` for both
//! modes, since manox exposes `AskUserQuestion` generally (codex restricts
//! `request_user_input` to Plan only).

use crate::language_model::ReasoningEffort;
use serde::{Deserialize, Serialize};

const PLAN_INSTRUCTIONS: &str = include_str!("prompt/templates/mode/plan_instructions.md");
const DEFAULT_INSTRUCTIONS: &str = include_str!("prompt/templates/mode/default_instructions.md");

/// The collaboration mode a thread is in. `Default` is the execution mode;
/// `Plan` is the read-only research-and-plan mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModeKind {
    #[default]
    Default,
    Plan,
}

/// The modes a user can cycle through with shift-tab / `/plan` / the `+` menu.
pub const CYCLABLE: &[ModeKind] = &[ModeKind::Default, ModeKind::Plan];

impl ModeKind {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Default => "Default",
            Self::Plan => "Plan",
        }
    }

    pub const fn is_visible(self) -> bool {
        matches!(self, Self::Default | Self::Plan)
    }

    /// Whether `AskUserQuestion` is offered in this mode. codex restricts
    /// `request_user_input` to Plan; manox exposes the tool in both modes.
    pub const fn allows_user_input(self) -> bool {
        true
    }

    /// Next mode when cycling [`CYCLABLE`].
    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::Plan,
            Self::Plan => Self::Default,
        }
    }
}

/// Per-mode settings. Each field is an override over the thread's base
/// (`self.model` / `self.reasoning_effort`); `None` means inherit the base.
/// Doubles as the built-in preset shape and the user-config override shape
/// (`[modes.plan]` / `[modes.default]` in `settings.toml`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<ReasoningEffort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer_instructions: Option<String>,
}

impl ModeSettings {
    /// Overlay `override` onto `self`: a `Some` field in `override` wins,
    /// otherwise `self`'s field is preserved. Mirrors codex `apply_mask`
    /// for the TOML-config path (field-absent = inherit, there is no
    /// explicit-clear channel in TOML, unlike codex's `Option<Option<T>>`
    /// protocol mask).
    pub fn resolved(self, override_: &ModeSettings) -> ModeSettings {
        ModeSettings {
            model: override_.model.clone().or(self.model),
            reasoning_effort: override_.reasoning_effort.or(self.reasoning_effort),
            developer_instructions: override_
                .developer_instructions
                .clone()
                .or(self.developer_instructions),
        }
    }
}

/// Per-mode user overrides read from `settings.toml` `[modes.*]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeSettingsMap {
    #[serde(default)]
    pub default: Option<ModeSettings>,
    #[serde(default)]
    pub plan: Option<ModeSettings>,
}

impl ModeSettingsMap {
    pub fn get(&self, mode: ModeKind) -> Option<&ModeSettings> {
        match mode {
            ModeKind::Default => self.default.as_ref(),
            ModeKind::Plan => self.plan.as_ref(),
        }
    }

    /// Whether the map carries no per-mode overrides — used to skip
    /// serializing an empty `[modes]` table.
    pub fn is_empty(&self) -> bool {
        self.default.is_none() && self.plan.is_none()
    }
}

/// Built-in preset for a mode. Plan sets `reasoning_effort = Medium` and the
/// plan-mode developer instructions; Default carries the default-mode
/// instructions. Both are overlaid by any user `[modes.*]` override.
pub fn preset_for(mode: ModeKind) -> ModeSettings {
    match mode {
        ModeKind::Plan => ModeSettings {
            model: None,
            reasoning_effort: Some(ReasoningEffort::Medium),
            developer_instructions: Some(PLAN_INSTRUCTIONS.to_string()),
        },
        ModeKind::Default => ModeSettings {
            model: None,
            reasoning_effort: None,
            developer_instructions: Some(DEFAULT_INSTRUCTIONS.to_string()),
        },
    }
}

/// Resolve the effective settings for `mode`: built-in preset overlaid with the
/// user's per-mode override. Called at request-build time.
pub fn resolve(mode: ModeKind, user: &ModeSettingsMap) -> ModeSettings {
    preset_for(mode).resolved(user.get(mode).unwrap_or(&ModeSettings::default()))
}

/// The user's verdict on a turn-end proposed plan: implement the approved plan,
/// optionally after clearing the context. Staying in Plan mode to refine is not
/// a verdict — the user simply keeps typing, which dismisses the pending plan
/// and lets the model re-propose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanReviewChoice {
    /// Exit Plan mode and execute the approved plan.
    Implement,
    /// Exit Plan mode, clear prior context, then execute — the plan text is
    /// re-injected as the seed of a fresh context.
    ImplementClearContext,
}

/// The user turn that seeds an implement turn after the user approves a proposed
/// plan. The `<proposed_plan>` block is never persisted into the assistant
/// message (prefix-cache preservation), so the approved plan text is re-injected
/// as this user turn — identical text live and on rebuild, so a reloaded thread
/// renders the same verdict bubble the live view showed.
pub fn implement_plan_user_message(plan_text: &str) -> String {
    format!("Implement the approved plan:\n\n{plan_text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_cycles_default_and_plan() {
        assert_eq!(ModeKind::Default.next(), ModeKind::Plan);
        assert_eq!(ModeKind::Plan.next(), ModeKind::Default);
        // A full cycle returns to the start.
        assert_eq!(ModeKind::Default.next().next(), ModeKind::Default);
        assert_eq!(ModeKind::Plan.next().next(), ModeKind::Plan);
    }

    #[test]
    fn cyclable_contains_both_visible_modes() {
        assert_eq!(CYCLABLE, &[ModeKind::Default, ModeKind::Plan]);
        for m in CYCLABLE {
            assert!(m.is_visible());
            assert!(m.allows_user_input());
        }
    }

    #[test]
    fn display_name_is_human_readable() {
        assert_eq!(ModeKind::Default.display_name(), "Default");
        assert_eq!(ModeKind::Plan.display_name(), "Plan");
    }

    #[test]
    fn resolved_overlay_takes_override_when_present() {
        let base = ModeSettings {
            model: Some("base-model".into()),
            reasoning_effort: Some(ReasoningEffort::Low),
            developer_instructions: Some("base".into()),
        };
        let ovr = ModeSettings {
            model: Some("ovr-model".into()),
            reasoning_effort: Some(ReasoningEffort::High),
            developer_instructions: Some("ovr".into()),
        };
        let r = base.resolved(&ovr);
        assert_eq!(r.model.as_deref(), Some("ovr-model"));
        assert_eq!(r.reasoning_effort, Some(ReasoningEffort::High));
        assert_eq!(r.developer_instructions.as_deref(), Some("ovr"));
    }

    #[test]
    fn resolved_preserves_base_when_override_absent() {
        let base = ModeSettings {
            model: Some("base-model".into()),
            reasoning_effort: Some(ReasoningEffort::Low),
            developer_instructions: Some("base".into()),
        };
        // Empty override: every base field preserved.
        let r = base.clone().resolved(&ModeSettings::default());
        assert_eq!(r, base);

        // Field-by-field preservation: override only sets what it carries.
        let partial = ModeSettings {
            model: Some("ovr-model".into()),
            ..Default::default()
        };
        let r = base.resolved(&partial);
        assert_eq!(r.model.as_deref(), Some("ovr-model"));
        assert_eq!(r.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(r.developer_instructions.as_deref(), Some("base"));
    }

    #[test]
    fn preset_for_plan_sets_medium_effort_and_plan_instructions() {
        let p = preset_for(ModeKind::Plan);
        assert_eq!(p.reasoning_effort, Some(ReasoningEffort::Medium));
        assert!(p.developer_instructions.is_some());
        assert!(p.model.is_none());
    }

    #[test]
    fn preset_for_default_carries_default_instructions_no_effort() {
        let p = preset_for(ModeKind::Default);
        assert_eq!(p.reasoning_effort, None);
        assert!(p.developer_instructions.is_some());
        assert!(p.model.is_none());
    }

    #[test]
    fn resolve_returns_preset_when_no_user_override() {
        let user = ModeSettingsMap::default();
        let plan = resolve(ModeKind::Plan, &user);
        assert_eq!(plan.reasoning_effort, Some(ReasoningEffort::Medium));

        let default = resolve(ModeKind::Default, &user);
        assert_eq!(default.reasoning_effort, None);
    }

    #[test]
    fn resolve_overlays_user_override_on_preset() {
        let user = ModeSettingsMap {
            plan: Some(ModeSettings {
                model: Some("custom-plan-model".into()),
                reasoning_effort: Some(ReasoningEffort::High),
                ..Default::default()
            }),
            ..Default::default()
        };
        let r = resolve(ModeKind::Plan, &user);
        assert_eq!(r.model.as_deref(), Some("custom-plan-model"));
        assert_eq!(r.reasoning_effort, Some(ReasoningEffort::High));
        // Preset's developer_instructions survives (override didn't set it).
        assert!(r.developer_instructions.is_some());
    }

    #[test]
    fn modesettingsmap_get_and_is_empty() {
        let empty = ModeSettingsMap::default();
        assert!(empty.is_empty());
        assert!(empty.get(ModeKind::Plan).is_none());

        let m = ModeSettingsMap {
            plan: Some(ModeSettings::default()),
            ..Default::default()
        };
        assert!(!m.is_empty());
        assert!(m.get(ModeKind::Plan).is_some());
        assert!(m.get(ModeKind::Default).is_none());
    }

    #[test]
    fn modekind_serde_round_trips_snake_case() {
        let json = serde_json::to_string(&ModeKind::Plan).unwrap();
        assert_eq!(json, "\"plan\"");
        let m: ModeKind = serde_json::from_str("\"default\"").unwrap();
        assert_eq!(m, ModeKind::Default);
        assert_eq!(ModeKind::default(), ModeKind::Default);
    }
}
