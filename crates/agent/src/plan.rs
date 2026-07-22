//! Execution-time plan snapshot: the structured task list the model maintains
//! through the [`UpdatePlan`](crate::tools::update_plan) tool.
//!
//! This is distinct from the `<proposed_plan>` review block
//! ([`proposed_plan`](crate::proposed_plan)): the proposed plan is free-form
//! Markdown a human approves before work starts, whereas a [`PlanSnapshot`] is
//! the model's own machine-readable account of what it is doing right now. The
//! Context Rail renders the snapshot; nothing infers task state from prose.
//!
//! Snapshots are never persisted separately — each successful `UpdatePlan` call
//! is an ordinary `ToolUse`/`ToolResult` pair in the message history, so
//! [`rebuild_from_messages`] recovers the latest valid snapshot on reload by
//! replaying that history. No database column, no UI-note record.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::language_model::MessageContent;
use crate::message::Message;
use crate::tools::UPDATE_PLAN;

/// Upper bound on tasks in one snapshot. A rail overview stays legible only
/// while the list is short; a plan that needs more than this many steps is
/// really several plans.
pub const MAX_PLAN_STEPS: usize = 12;

/// Maximum length (chars) of a single task title after trimming.
pub const MAX_STEP_LEN: usize = 120;

/// Lifecycle of one plan step. Wire values are snake_case (`in_progress`) to
/// match the tool schema the model calls with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
}

/// One task in the plan: a short title and its current status. Order is
/// significant and preserved verbatim from the model's call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    /// Concise, single-line, Markdown-free task title.
    pub step: String,
    pub status: PlanStepStatus,
}

/// A validated plan the model published via `UpdatePlan`. An empty `steps`
/// clears the rail's plan (the model dropped its task list). `explanation` is
/// an optional one-line note about the latest update, not rendered as a task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSnapshot {
    pub explanation: Option<String>,
    pub steps: Vec<PlanStep>,
}

/// Raw `UpdatePlan` tool input, before validation. Kept private — callers go
/// through [`PlanSnapshot::from_input`], the single validation entry point
/// shared by the tool (to shape its result) and the thread (to recover the
/// snapshot it emits).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdatePlanInput {
    /// Optional one-line note about what changed in this update.
    #[serde(default)]
    pub explanation: Option<String>,
    /// The full task list. An empty array clears the current plan.
    pub plan: Vec<PlanStep>,
}

impl PlanSnapshot {
    /// Validate raw tool input into a snapshot. `Err` carries a model-facing
    /// message (English, no trailing punctuation) explaining the violation so
    /// the model can correct and retry; a rejected call never overwrites the
    /// last valid snapshot.
    ///
    /// Rules: at most [`MAX_PLAN_STEPS`] steps; each title trims to 1..=
    /// [`MAX_STEP_LEN`] chars, single-line, and unique across the list; at most
    /// one `in_progress` step. An empty list is valid and means "clear the
    /// plan".
    pub fn from_input(input: &serde_json::Value) -> Result<Self, String> {
        let parsed: UpdatePlanInput = serde_json::from_value(input.clone())
            .map_err(|e| format!("invalid UpdatePlan input: {e}"))?;

        if parsed.plan.len() > MAX_PLAN_STEPS {
            return Err(format!(
                "too many steps: {} (max {MAX_PLAN_STEPS}); split the work into a shorter plan",
                parsed.plan.len()
            ));
        }

        let mut steps = Vec::with_capacity(parsed.plan.len());
        let mut in_progress = 0usize;
        for (i, raw) in parsed.plan.iter().enumerate() {
            let title = raw.step.trim();
            if title.is_empty() {
                return Err(format!("step {} has an empty title", i + 1));
            }
            if title.contains('\n') {
                return Err(format!("step {} title must be a single line", i + 1));
            }
            if title.chars().count() > MAX_STEP_LEN {
                return Err(format!(
                    "step {} title exceeds {MAX_STEP_LEN} characters",
                    i + 1
                ));
            }
            if steps
                .iter()
                .any(|s: &PlanStep| s.step.eq_ignore_ascii_case(title))
            {
                return Err(format!("duplicate step title: {title:?}"));
            }
            if raw.status == PlanStepStatus::InProgress {
                in_progress += 1;
            }
            steps.push(PlanStep {
                step: title.to_string(),
                status: raw.status,
            });
        }

        if in_progress > 1 {
            return Err(format!(
                "at most one step may be in_progress at a time (found {in_progress})"
            ));
        }

        let explanation = parsed
            .explanation
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty());

        Ok(Self { explanation, steps })
    }

    /// Whether the plan carries no tasks — the model cleared its list.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// Count of steps in each terminal/active state, as `(completed, total)`.
    pub fn progress(&self) -> (usize, usize) {
        let done = self
            .steps
            .iter()
            .filter(|s| s.status == PlanStepStatus::Completed)
            .count();
        (done, self.steps.len())
    }

    /// The step the model is actively working: the first `in_progress`, else
    /// the first `pending`. `None` when every step is completed (or the plan is
    /// empty).
    pub fn current(&self) -> Option<&PlanStep> {
        self.steps
            .iter()
            .find(|s| s.status == PlanStepStatus::InProgress)
            .or_else(|| {
                self.steps
                    .iter()
                    .find(|s| s.status == PlanStepStatus::Pending)
            })
    }

    /// Whether every step is completed (and there is at least one step).
    pub fn all_completed(&self) -> bool {
        !self.steps.is_empty()
            && self
                .steps
                .iter()
                .all(|s| s.status == PlanStepStatus::Completed)
    }
}

/// Recover the latest valid plan snapshot from message history. Scans from the
/// tail for the most recent `UpdatePlan` `ToolUse` whose paired `ToolResult`
/// did not error, and re-validates its input. A successful empty plan (the
/// model cleared its list) recovers as `None` — there is no live plan to show.
///
/// Pure over `messages`: reload, thread switch, and post-compaction rebuild all
/// route through here, so the rail's plan state is always a function of the
/// canonical history and never drifts from it.
pub fn rebuild_from_messages(messages: &[Message]) -> Option<PlanSnapshot> {
    // Collect the ids of tool results that errored, so a failed UpdatePlan call
    // is skipped rather than treated as authoritative.
    let mut errored: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for m in messages {
        for c in &m.content {
            if let MessageContent::ToolResult(r) = c
                && r.is_error
            {
                errored.insert(r.tool_use_id.as_str());
            }
        }
    }

    for m in messages.iter().rev() {
        for c in m.content.iter().rev() {
            if let MessageContent::ToolUse(tu) = c
                && tu.name.as_ref() == UPDATE_PLAN
                && !errored.contains(tu.id.as_str())
                && let Ok(snapshot) = PlanSnapshot::from_input(&tu.input)
            {
                return (!snapshot.is_empty()).then_some(snapshot);
            }
        }
    }
    let state = crate::compact::latest_compaction_state(messages)?;
    let steps = state.plan_steps?;
    let snapshot = PlanSnapshot {
        explanation: None,
        steps: steps
            .into_iter()
            .map(|step| {
                let status = serde_json::from_value(serde_json::Value::String(step.status))
                    .unwrap_or(PlanStepStatus::Pending);
                PlanStep {
                    step: step.title,
                    status,
                }
            })
            .collect(),
    };
    (!snapshot.is_empty()).then_some(snapshot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::{LanguageModelToolResult, LanguageModelToolUse};
    use crate::message::Message;
    use serde_json::json;

    fn input(plan: serde_json::Value) -> serde_json::Value {
        json!({ "plan": plan })
    }

    #[test]
    fn valid_snapshot_trims_and_preserves_order() {
        let snap = PlanSnapshot::from_input(&json!({
            "explanation": "  starting  ",
            "plan": [
                { "step": "  First  ", "status": "completed" },
                { "step": "Second", "status": "in_progress" },
                { "step": "Third", "status": "pending" },
            ],
        }))
        .unwrap();
        assert_eq!(snap.explanation.as_deref(), Some("starting"));
        assert_eq!(snap.steps.len(), 3);
        assert_eq!(snap.steps[0].step, "First");
        assert_eq!(snap.steps[0].status, PlanStepStatus::Completed);
        assert_eq!(snap.current().unwrap().step, "Second");
        assert_eq!(snap.progress(), (1, 3));
        assert!(!snap.all_completed());
    }

    #[test]
    fn empty_plan_is_valid_and_clears() {
        let snap = PlanSnapshot::from_input(&input(json!([]))).unwrap();
        assert!(snap.is_empty());
        assert_eq!(snap.current(), None);
    }

    #[test]
    fn rejects_more_than_max_steps() {
        let plan: Vec<_> = (0..MAX_PLAN_STEPS + 1)
            .map(|i| json!({ "step": format!("s{i}"), "status": "pending" }))
            .collect();
        let err = PlanSnapshot::from_input(&input(json!(plan))).unwrap_err();
        assert!(err.contains("too many steps"), "{err}");
    }

    #[test]
    fn rejects_empty_multiline_and_overlong_titles() {
        assert!(
            PlanSnapshot::from_input(&input(json!([{ "step": "   ", "status": "pending" }])))
                .unwrap_err()
                .contains("empty title")
        );
        assert!(
            PlanSnapshot::from_input(&input(json!([{ "step": "a\nb", "status": "pending" }])))
                .unwrap_err()
                .contains("single line")
        );
        let long = "x".repeat(MAX_STEP_LEN + 1);
        assert!(
            PlanSnapshot::from_input(&input(json!([{ "step": long, "status": "pending" }])))
                .unwrap_err()
                .contains("exceeds")
        );
    }

    #[test]
    fn rejects_duplicate_titles_case_insensitive() {
        let err = PlanSnapshot::from_input(&input(json!([
            { "step": "Build", "status": "pending" },
            { "step": "build", "status": "pending" },
        ])))
        .unwrap_err();
        assert!(err.contains("duplicate"), "{err}");
    }

    #[test]
    fn rejects_multiple_in_progress() {
        let err = PlanSnapshot::from_input(&input(json!([
            { "step": "A", "status": "in_progress" },
            { "step": "B", "status": "in_progress" },
        ])))
        .unwrap_err();
        assert!(err.contains("at most one"), "{err}");
    }

    #[test]
    fn zero_in_progress_is_allowed() {
        // Relaxed to ≤1: a turn gap with no active step (all pending, or all
        // done) must not be rejected — rejecting would trap the model in a
        // reject/retry loop.
        let snap = PlanSnapshot::from_input(&input(json!([
            { "step": "A", "status": "pending" },
            { "step": "B", "status": "pending" },
        ])))
        .unwrap();
        assert_eq!(snap.current().unwrap().step, "A");
    }

    #[test]
    fn rejects_unknown_fields_and_bad_status() {
        assert!(
            PlanSnapshot::from_input(&json!({ "plan": [], "extra": 1 })).is_err(),
            "unknown top-level field must be rejected"
        );
        assert!(
            PlanSnapshot::from_input(&input(json!([{ "step": "A", "status": "blocked" }])))
                .is_err(),
            "unknown status must be rejected"
        );
    }

    #[test]
    fn all_completed_detects_done_plan() {
        let snap = PlanSnapshot::from_input(&input(json!([
            { "step": "A", "status": "completed" },
            { "step": "B", "status": "completed" },
        ])))
        .unwrap();
        assert!(snap.all_completed());
        assert_eq!(snap.current(), None);
    }

    fn tool_use(id: &str, plan: serde_json::Value) -> Message {
        Message::assistant(vec![MessageContent::ToolUse(LanguageModelToolUse {
            id: id.to_string(),
            name: UPDATE_PLAN.into(),
            raw_input: String::new(),
            input: input(plan),
            is_input_complete: true,
            thought_signature: None,
        })])
    }

    fn tool_result(id: &str, is_error: bool) -> Message {
        Message::user_with_content(vec![MessageContent::ToolResult(LanguageModelToolResult {
            tool_use_id: id.to_string(),
            tool_name: UPDATE_PLAN.into(),
            is_error,
            content: "ok".to_string(),
        })])
    }

    #[test]
    fn rebuild_recovers_latest_successful_snapshot() {
        let messages = vec![
            tool_use("1", json!([{ "step": "First", "status": "completed" }])),
            tool_result("1", false),
            tool_use("2", json!([{ "step": "Second", "status": "in_progress" }])),
            tool_result("2", false),
        ];
        let snap = rebuild_from_messages(&messages).unwrap();
        assert_eq!(snap.steps.len(), 1);
        assert_eq!(snap.steps[0].step, "Second");
    }

    /// Regression for the motivating failure: the old milestone parser turned a
    /// design doc's numbered `##` sections + nested implementation bullets into
    /// 27 rail rows. Now the model publishes its own 7-step execution plan and
    /// the rail shows exactly those 7 clean titles — no Markdown markers, no
    /// bullet explosion, regardless of how granular the approved design was.
    #[test]
    fn structured_plan_yields_clean_titles_not_bullet_dump() {
        let messages = vec![
            tool_use(
                "1",
                json!([
                    { "step": "Add shared PlanStep / PlanSnapshot types", "status": "completed" },
                    { "step": "Implement the UpdatePlan tool + schema", "status": "completed" },
                    { "step": "Wire PlanUpdated through the thread", "status": "in_progress" },
                    { "step": "Delete the milestone state machine", "status": "pending" },
                    { "step": "Rewrite the Context Rail", "status": "pending" },
                    { "step": "Update prompts and i18n", "status": "pending" },
                    { "step": "Tests, docs, and delivery", "status": "pending" },
                ]),
            ),
            tool_result("1", false),
        ];
        let snap = rebuild_from_messages(&messages).unwrap();
        assert_eq!(
            snap.steps.len(),
            7,
            "exactly the published steps, no explosion"
        );
        for step in &snap.steps {
            assert!(
                !step.step.contains("**") && !step.step.contains('`') && !step.step.contains('#'),
                "titles carry no Markdown noise: {:?}",
                step.step
            );
        }
        assert_eq!(snap.progress(), (2, 7));
        assert_eq!(
            snap.current().unwrap().step,
            "Wire PlanUpdated through the thread"
        );
    }

    #[test]
    fn rebuild_skips_errored_call() {
        let messages = vec![
            tool_use("1", json!([{ "step": "Good", "status": "in_progress" }])),
            tool_result("1", false),
            tool_use("2", json!([{ "step": "Bad", "status": "pending" }])),
            tool_result("2", true),
        ];
        let snap = rebuild_from_messages(&messages).unwrap();
        assert_eq!(snap.steps[0].step, "Good");
    }

    #[test]
    fn rebuild_empty_plan_recovers_none() {
        let messages = vec![
            tool_use("1", json!([{ "step": "Old", "status": "pending" }])),
            tool_result("1", false),
            tool_use("2", json!([])),
            tool_result("2", false),
        ];
        assert!(rebuild_from_messages(&messages).is_none());
    }

    #[test]
    fn rebuild_recovers_plan_from_latest_compaction_capsule() {
        let state =
            crate::compact::collect_compaction_state(crate::compact::CompactionStateInput {
                cwd: std::path::Path::new("/tmp"),
                covered_message_id: Some("covered"),
                worktree_branch: None,
                worktree_path: None,
                git_branch: None,
                git_status: None,
                plan_steps: Some(vec![
                    crate::compact::PlanStepCapsule {
                        title: "Recovered".into(),
                        status: "in_progress".into(),
                    },
                    crate::compact::PlanStepCapsule {
                        title: "Then verify".into(),
                        status: "pending".into(),
                    },
                ]),
                goal: None,
                collaboration_mode: Some("Plan"),
                active_tools: Vec::new(),
                active_skills: Vec::new(),
                background_shells: Vec::new(),
                artifacts: Vec::new(),
            });
        let messages = vec![Message::user_with_content(vec![
            MessageContent::Compaction(crate::compact::build_compaction_envelope(
                "handoff".into(),
                state,
            )),
        ])];

        let snapshot = rebuild_from_messages(&messages).unwrap();
        assert_eq!(snapshot.steps[0].step, "Recovered");
        assert_eq!(snapshot.steps[0].status, PlanStepStatus::InProgress);
        assert_eq!(snapshot.steps[1].step, "Then verify");
    }

    #[test]
    fn rebuild_none_without_any_call() {
        assert!(rebuild_from_messages(&[]).is_none());
    }
}
