//! The `UpdatePlan` tool: the model publishes a structured task list that the
//! Context Rail renders as an execution overview.
//!
//! This replaces the old approach of inferring milestones from approved-plan
//! Markdown (unreliable — see the 27-bullet regression). The model is the only
//! authority on its own progress, so it reports the list explicitly and updates
//! the whole snapshot each time state changes.
//!
//! Not read-only (it drives UI state), yet it needs no approval — publishing a
//! task list mutates nothing on disk. Because it is not read-only, plan mode's
//! request-tool filter hides it, and the plan-mode backstop in
//! `run_tool_inner` rejects a hallucinated call. Validation lives in
//! [`PlanSnapshot::from_input`]; the tool only re-checks and shapes the result.

use gpui::{App, AppContext as _, Task};
use tokio_util::sync::CancellationToken;

use crate::plan::{PlanSnapshot, PlanStepStatus};
use crate::tool::{AgentTool as AgentToolTrait, ToolContext};

/// The `UpdatePlan` tool. Stateless: the validated snapshot is re-derived from
/// the call input by the thread (which owns the `PlanUpdated` emission), so the
/// tool holds no state and returns a human-readable confirmation.
pub struct UpdatePlanTool;

impl AgentToolTrait for UpdatePlanTool {
    fn name(&self) -> &str {
        super::UPDATE_PLAN
    }

    fn description(&self) -> &str {
        "Publish or update your task list for the current work. The full plan is a short, \
         ordered list of steps, each with a status (pending / in_progress / completed). Call \
         this when you begin non-trivial multi-step work (roughly 3+ steps) or right after a \
         plan is approved, then again whenever progress changes — always send the COMPLETE new \
         list, not a delta. Keep at most one step in_progress. Mark steps completed as you \
         finish them and mark all completed before you end. Titles must be concise, single-line, \
         and free of Markdown. Send an empty plan to clear the list. This drives the plan \
         overview shown to the user; it does not change files."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema::<crate::plan::UpdatePlanInput>()
    }

    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        // Validate here so a malformed call returns an error ToolResult and the
        // thread keeps the last valid snapshot. The thread re-validates the
        // same input to build the `PlanUpdated` event it emits, so the two
        // never disagree.
        let result = match PlanSnapshot::from_input(&input) {
            Ok(snapshot) => Ok(confirmation(&snapshot)),
            Err(e) => Err(e),
        };
        cx.background_spawn(async move { result })
    }
}

/// Model-facing confirmation string echoing the accepted plan so the model sees
/// exactly what was recorded. Kept terse; the rich view is the rail.
fn confirmation(snapshot: &PlanSnapshot) -> String {
    if snapshot.is_empty() {
        return "Plan cleared.".to_string();
    }
    let (done, total) = snapshot.progress();
    let mut out = format!("Plan updated: {done}/{total} completed.\n");
    for (i, step) in snapshot.steps.iter().enumerate() {
        let marker = match step.status {
            PlanStepStatus::Completed => "[x]",
            PlanStepStatus::InProgress => "[>]",
            PlanStepStatus::Pending => "[ ]",
        };
        out.push_str(&format!("{marker} {}. {}\n", i + 1, step.step));
    }
    out.truncate(out.trim_end().len());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_not_read_only() {
        let tool = UpdatePlanTool;
        assert_eq!(tool.name(), "UpdatePlan");
        // Not read-only: plan mode must hide it and the backstop must reject a
        // hallucinated call.
        assert!(!tool.is_read_only());
        // No approval: publishing a task list mutates nothing on disk.
        assert!(!tool.requires_approval(&serde_json::json!({ "plan": [] })));
    }

    #[test]
    fn schema_has_no_refs_and_is_object() {
        let schema = UpdatePlanTool.input_schema();
        assert_eq!(schema["type"], "object");
        assert!(!schema.as_object().unwrap().contains_key("$defs"));
        assert!(!schema.as_object().unwrap().contains_key("$schema"));
        assert!(!contains_ref(&schema));
    }

    fn contains_ref(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(map) => {
                map.contains_key("$ref") || map.values().any(contains_ref)
            }
            serde_json::Value::Array(arr) => arr.iter().any(contains_ref),
            _ => false,
        }
    }

    #[test]
    fn confirmation_lists_steps_with_markers() {
        let snap = PlanSnapshot::from_input(&serde_json::json!({
            "plan": [
                { "step": "Build", "status": "completed" },
                { "step": "Test", "status": "in_progress" },
            ],
        }))
        .unwrap();
        let c = confirmation(&snap);
        assert!(c.contains("1/2 completed"));
        assert!(c.contains("[x] 1. Build"));
        assert!(c.contains("[>] 2. Test"));
    }

    #[test]
    fn confirmation_empty_is_cleared() {
        let snap = PlanSnapshot::from_input(&serde_json::json!({ "plan": [] })).unwrap();
        assert_eq!(confirmation(&snap), "Plan cleared.");
    }
}
