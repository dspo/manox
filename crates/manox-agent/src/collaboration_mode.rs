//! Plan review verdicts and plan-mode prompt rendering.
//!
//! Plan mode is a structured propose flow: the model researches read-only,
//! writes the plan to a file, and submits it through the `ProposePlan` tool
//! (see [`crate::plan_mode`]); the user picks one of four execution verdicts
//! on the review card. The plan-mode instructions are English-only prose,
//! injected every turn while plan mode is active.

/// The user's verdict on a proposed plan (oh-my-pi's four execution options).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanReviewChoice {
    /// Execute the approved plan in fresh context: the workspace archives the
    /// current thread and spawns a new one seeded with the execution
    /// directive referencing the plan file.
    ExecuteFresh,
    /// Distill the planning discussion via compaction, then execute the
    /// approved plan on the current thread.
    ExecuteCompact,
    /// Execute the approved plan on the current thread, keeping the full
    /// planning context.
    ExecuteKeep,
    /// Do not execute yet: the user gives feedback, the model updates the
    /// plan file and proposes again (plan mode stays active).
    Refine,
}

/// Rendered plan-mode-active instructions for a turn injection (plan mode ON):
/// read-only discipline, plan-file conventions, research discipline, and the
/// propose contract. `plans_dir` is shown to the model as the plan-file root.
pub fn render_plan_mode_active(plans_dir: &str) -> anyhow::Result<String> {
    crate::prompt::render(
        crate::prompt::PromptTemplate::ModePlanActive,
        &PlanModeActiveData { plans_dir },
    )
}

/// Rendered execution seed after a plan is approved: read the plan file and
/// implement it top to bottom.
pub fn render_plan_mode_approved(plan_file: &str) -> anyhow::Result<String> {
    crate::prompt::render(
        crate::prompt::PromptTemplate::ModePlanApproved,
        &PlanModeApprovedData { plan_file },
    )
}

#[derive(serde::Serialize)]
struct PlanModeActiveData<'a> {
    plans_dir: &'a str,
}

#[derive(serde::Serialize)]
struct PlanModeApprovedData<'a> {
    plan_file: &'a str,
}

/// Compaction steering for `ExecuteCompact`: the summary keeps the planning
/// context needed to execute; the plan file itself carries the detail.
pub fn plan_compact_instructions(plan_file: &str) -> String {
    format!(
        "The plan at {plan_file} was just approved. Distill this planning discussion \
         into the context needed to execute that plan: the task's intent, key findings, \
         and decisions made. The plan file itself carries the implementation detail — \
         reference it by path instead of restating it."
    )
}
