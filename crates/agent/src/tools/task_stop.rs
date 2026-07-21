//! `TaskStop` tool — stop a background task (Monitor or background Bash).
//! No approval required; passing an unknown id returns a summary of
//! currently running tasks.

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::background_task;
use crate::tool::AgentTool;
use crate::tools::schema;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskStopInput {
    /// The task id to stop (e.g. `monitor_1`, `ws_1`, `bash_1`).
    task_id: String,
}

pub struct TaskStopTool;

impl AgentTool for TaskStopTool {
    fn name(&self) -> &str {
        "TaskStop"
    }

    fn description(&self) -> &str {
        "Stop a background task (Monitor or background Bash) by its task id. \
         No approval required. If the task id is unknown, returns a summary of \
         all currently running tasks that can be stopped."
    }

    fn input_schema(&self) -> serde_json::Value {
        schema::<TaskStopInput>()
    }

    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        false
    }

    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parsed: TaskStopInput = match serde_json::from_value(input) {
            Ok(p) => p,
            Err(e) => {
                return cx.background_spawn(async move {
                    Err(format!("TaskStop input parse failed: {e}"))
                });
            }
        };

        let task_id = parsed.task_id;
        cx.background_spawn(async move {
            match background_task::stop(&task_id) {
                Ok(()) => Ok(format!("Task {task_id} stopped.")),
                Err(e) => Err(e),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_task_stop_input() {
        let v = serde_json::json!({"task_id": "monitor_1"});
        let p: TaskStopInput = serde_json::from_value(v).unwrap();
        assert_eq!(p.task_id, "monitor_1");
    }

    #[test]
    fn does_not_require_approval() {
        assert!(!TaskStopTool.requires_approval(&serde_json::json!({"task_id": "x"})));
    }
}