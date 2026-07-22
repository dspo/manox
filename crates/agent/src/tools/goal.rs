//! Stable main-thread Goal tools.

use gpui::{App, AsyncApp, Task, WeakEntity};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::db::GoalActor;
use crate::goal::GoalStatus;
use crate::thread::Thread;
use crate::tool::{AgentTool, ToolContext};

pub struct GetGoalTool {
    parent: WeakEntity<Thread>,
}

pub struct CreateGoalTool {
    parent: WeakEntity<Thread>,
}

pub struct UpdateGoalTool {
    parent: WeakEntity<Thread>,
}

impl GetGoalTool {
    pub fn new(parent: WeakEntity<Thread>) -> Self {
        Self { parent }
    }
}
impl CreateGoalTool {
    pub fn new(parent: WeakEntity<Thread>) -> Self {
        Self { parent }
    }
}
impl UpdateGoalTool {
    pub fn new(parent: WeakEntity<Thread>) -> Self {
        Self { parent }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EmptyInput {}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CreateGoalInput {
    /// Concrete objective explicitly requested by the user or system/developer instructions.
    objective: String,
    /// Positive token budget. Omit unless the user explicitly requested one.
    #[serde(default)]
    token_budget: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum ModelGoalStatus {
    Complete,
    Blocked,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateGoalInput {
    status: ModelGoalStatus,
    #[serde(default)]
    reason: Option<String>,
}

fn snapshot(thread: &Thread) -> String {
    match thread.goal() {
        Some(goal) => serde_json::to_string_pretty(&serde_json::json!({
            "goal": goal,
            "remaining_tokens": goal.remaining_tokens(),
        }))
        .expect("Goal snapshot serializes"),
        None => "{\n  \"goal\": null\n}".into(),
    }
}

impl AgentTool for GetGoalTool {
    fn name(&self) -> &str {
        super::GET_GOAL
    }
    fn description(&self) -> &str {
        "Return the current main-thread Goal snapshot, including status, objective, accounting, budget, and remaining tokens."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema::<EmptyInput>()
    }
    // Kept out of Plan mode together with the mutating Goal tools; Default's
    // registered tool list remains stable across every Goal state.
    fn is_read_only(&self) -> bool {
        false
    }
    fn run(
        &self,
        _input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parent = self.parent.clone();
        cx.spawn(async move |cx: &mut AsyncApp| {
            parent
                .read_with(cx, |thread, _| snapshot(thread))
                .map_err(|_| "thread dropped".into())
        })
    }
}

impl AgentTool for CreateGoalTool {
    fn name(&self) -> &str {
        super::CREATE_GOAL
    }
    fn description(&self) -> &str {
        "Create the persistent Goal only when the user or system/developer instructions explicitly request autonomous Goal work. Do not infer a Goal from an ordinary task. Omit token_budget unless explicitly requested. Fails while an unfinished Goal exists."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema::<CreateGoalInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parent = self.parent.clone();
        cx.spawn(async move |cx: &mut AsyncApp| {
            let input: CreateGoalInput =
                serde_json::from_value(input).map_err(|error| error.to_string())?;
            parent
                .update(cx, |thread, cx| {
                    thread.create_goal(
                        input.objective,
                        input.token_budget,
                        GoalActor::Model,
                        cx,
                    )?;
                    Ok(snapshot(thread))
                })
                .map_err(|_| "thread dropped".to_string())?
                .map_err(|error: anyhow::Error| error.to_string())
        })
    }
}

impl AgentTool for UpdateGoalTool {
    fn name(&self) -> &str {
        super::UPDATE_GOAL
    }
    fn description(&self) -> &str {
        "Report the current Goal as complete or genuinely blocked. Before complete, verify every part of the objective against current tool results and repository state. Use blocked only after the same blocking condition persists for at least three Goal turns and progress requires user input or external state. This tool cannot pause, resume, replace, clear, or budget-limit a Goal."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema::<UpdateGoalInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parent = self.parent.clone();
        cx.spawn(async move |cx: &mut AsyncApp| {
            let input: UpdateGoalInput =
                serde_json::from_value(input).map_err(|error| error.to_string())?;
            let status = match input.status {
                ModelGoalStatus::Complete => GoalStatus::Complete,
                ModelGoalStatus::Blocked => GoalStatus::Blocked,
            };
            parent
                .update(cx, |thread, cx| {
                    thread.update_goal_from_model(status, input.reason, cx)?;
                    Ok(snapshot(thread))
                })
                .map_err(|_| "thread dropped".to_string())?
                .map_err(|error: anyhow::Error| error.to_string())
        })
    }
}
