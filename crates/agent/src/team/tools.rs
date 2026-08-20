//! Task tools (TaskCreate/TaskList/TaskUpdate/TaskGet).
//!
//! Phase D stub — these return "pending rewrite" errors. Phase D step 10
//! will reimplement them to use `Arc<Mutex<TaskList>>` (bus-owned) directly,
//! removing the retired `team_round_trip`/`TeamRequest`/`TeamOp` path.

use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::thread_engine::BackendNotice;

macro_rules! task_tool {
    ($name:ident, $tool_name:literal, $desc:literal) => {
        pub struct $name;

        impl $name {
            pub fn new(_notice_tx: mpsc::UnboundedSender<BackendNotice>) -> Self {
                Self
            }
        }

        #[async_trait::async_trait]
        impl AgentTool for $name {
            fn name(&self) -> &str {
                $tool_name
            }
            fn description(&self) -> &str {
                $desc
            }
            fn is_read_only(&self) -> bool {
                false
            }
            fn requires_approval(&self, _params: &serde_json::Value) -> bool {
                false
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object", "properties": {} })
            }
            async fn execute(
                &self,
                _id: &str,
                _params: serde_json::Value,
                _signal: CancellationToken,
                _ctx: &dyn ToolContext,
            ) -> Result<AgentToolResult, ToolError> {
                Err(ToolError::ExecutionFailed(
                    "Task tools pending Steer bus rewrite (Phase D)".into(),
                ))
            }
        }
    };
}

task_tool!(
    TaskCreateTool,
    "TaskCreate",
    "Create a task in the shared task list."
);
task_tool!(
    TaskListTool,
    "TaskList",
    "List all tasks in the shared task list."
);
task_tool!(
    TaskUpdateTool,
    "TaskUpdate",
    "Update a task's status, owner, or subject."
);
task_tool!(TaskGetTool, "TaskGet", "Get a task by id.");
