// Bash tool — shell command execution with output truncation.
//
// Output is truncated to avoid overwhelming the context window.

use std::time::Duration;

use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use crate::tools::truncate::{self, TruncateConfig};

pub struct BashTool {
    command_prefix: Option<String>,
}

impl BashTool {
    pub fn new(command_prefix: Option<String>) -> Self {
        BashTool { command_prefix }
    }
}

impl BashTool {
    /// Default max bytes for output.
    const DEFAULT_MAX_BYTES: usize = 128 * 1024;
    /// Default max lines for output.
    const DEFAULT_MAX_LINES: usize = 2000;
}

#[async_trait::async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command"
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn requires_approval(&self, _params: &JsonValue) -> bool {
        true
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The command to execute"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default: 120000)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let command = params["command"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("command is required".into()))?;
        let timeout_ms = params["timeout"].as_u64().unwrap_or(120_000);

        let command = if let Some(ref prefix) = self.command_prefix {
            format!("{prefix} {command}")
        } else {
            command.to_string()
        };

        let result = ctx
            .env()
            .exec(&command, Duration::from_millis(timeout_ms), signal)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        let mut output = result.stdout;

        if !result.stderr.is_empty() {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str("[stderr]\n");
            output.push_str(&result.stderr);
        }

        if result.exit_code != 0 {
            output.push_str(&format!("\n\n[exit code: {}]", result.exit_code));
        }

        // Truncate output to avoid overwhelming the context window.
        let config = TruncateConfig {
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_lines: Self::DEFAULT_MAX_LINES,
        };
        let truncated = truncate::truncate(&output, &config);

        let mut final_output = truncated.content;
        if truncated.was_truncated {
            final_output.push_str(&format!(
                "\n\n[output truncated: {} lines, {} bytes]",
                truncated.original_lines, truncated.original_bytes
            ));
        }

        Ok(AgentToolResult::text(final_output))
    }
}
