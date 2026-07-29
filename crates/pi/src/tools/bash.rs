// Bash tool — shell command execution.
use crate::tool::{AgentTool, AgentToolResult, ToolError, ToolContext};
use serde_json::Value as JsonValue;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub struct BashTool {
    command_prefix: Option<String>,
}

impl BashTool {
    pub fn new(command_prefix: Option<String>) -> Self {
        BashTool { command_prefix }
    }
}

#[async_trait::async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str { "bash" }
    fn description(&self) -> &str { "Execute a shell command" }
    fn is_read_only(&self) -> bool { false }
    fn requires_approval(&self, _params: &JsonValue) -> bool { true }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command to execute" },
                "timeout": { "type": "integer", "description": "Timeout in milliseconds" }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        _signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let command = params["command"].as_str().ok_or_else(|| ToolError::InvalidArguments("command is required".into()))?;
        let timeout_ms = params["timeout"].as_u64().unwrap_or(120_000);

        let command = if let Some(ref prefix) = self.command_prefix {
            format!("{prefix} {command}")
        } else {
            command.to_string()
        };

        let result = ctx.env().exec(&command, Duration::from_millis(timeout_ms)).await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        let mut output = result.stdout;
        if !result.stderr.is_empty() {
            output.push_str("\n\n[stderr]\n");
            output.push_str(&result.stderr);
        }
        if result.exit_code != 0 {
            output.push_str(&format!("\n\n[exit code: {}]", result.exit_code));
        }

        Ok(AgentToolResult::text(output))
    }
}