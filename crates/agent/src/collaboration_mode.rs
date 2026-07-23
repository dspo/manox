//! Plan review and unified developer instructions.
//!
//! The legacy Default/Plan mode distinction has been removed. The model
//! decides autonomously whether to explore deeply and produce a
//! `<proposed_plan>` block or to execute directly, guided by the unified
//! developer instructions injected at request-build time. The `/plan` slash
//! command remains as a strong hint that injects planning-focused
//! instructions as a user message.

use crate::language::Language;

const UNIFIED_INSTRUCTIONS_EN: &str =
    include_str!("prompt/templates/en/mode/unified_instructions.md");
const UNIFIED_INSTRUCTIONS_ZH_CN: &str =
    include_str!("prompt/templates/zh-CN/mode/unified_instructions.md");

/// Unified developer instructions for the `<collaboration_mode>` block,
/// selected by the thread's agent language.
pub fn unified_instructions(lang: Language) -> &'static str {
    match lang {
        Language::En => UNIFIED_INSTRUCTIONS_EN,
        Language::ZhCn => UNIFIED_INSTRUCTIONS_ZH_CN,
    }
}

/// The user's verdict on a turn-end proposed plan: implement the approved plan,
/// optionally on a fresh thread. Keeping the conversation going is not a
/// verdict — the user simply keeps typing, which dismisses the pending plan
/// and lets the model re-propose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanReviewChoice {
    /// Execute the approved plan on the current thread.
    Implement,
    /// Execute the approved plan on a fresh thread — the workspace archives
    /// the current thread and spawns a new one seeded with the plan, so the
    /// model starts a clean context. The plan text is re-injected as that
    /// new thread's first user message.
    ImplementClearContext,
}

/// The user turn that seeds an implement turn after the user approves a
/// proposed plan. The `<proposed_plan>` block is never persisted into the
/// assistant message (prefix-cache preservation), so the approved plan text
/// is re-injected as this user turn — identical text live and on rebuild, so
/// a reloaded thread renders the same verdict bubble the live view showed.
pub fn implement_plan_user_message(plan_text: &str) -> String {
    format!("Implement the approved plan:\n\n{plan_text}")
}
