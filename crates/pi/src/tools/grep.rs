// Grep tool — content search with regex.
use crate::tool::{AgentTool, AgentToolResult, ToolError, ToolContext};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

pub struct GrepTool;

#[async_trait::async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str { "grep" }
    fn description(&self) -> &str { "Search for patterns in files" }
    fn is_read_only(&self) -> bool { true }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern to search for" },
                "path": { "type": "string", "description": "Directory or file to search" },
                "glob": { "type": "string", "description": "File glob pattern" },
                "ignoreCase": { "type": "boolean", "description": "Case-insensitive search" },
                "literal": { "type": "boolean", "description": "Treat pattern as literal" },
                "context": { "type": "integer", "description": "Lines of context" },
                "limit": { "type": "integer", "description": "Max results" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        _signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let pattern = params["pattern"].as_str().ok_or_else(|| ToolError::InvalidArguments("pattern is required".into()))?;

        // For now, use `grep` command via ExecutionEnv as a simple implementation.
        let path = params["path"].as_str().unwrap_or(".");
        let mut cmd = format!("grep -rn -- '{pattern}' {path}");

        if params.get("ignoreCase").and_then(|v| v.as_bool()).unwrap_or(false) {
            cmd = format!("grep -rni -- '{pattern}' {path}");
        }
        if let Some(limit) = params["limit"].as_u64() {
            cmd = format!("{cmd} | head -n {limit}");
        }

        let result = ctx.env().exec(&cmd, std::time::Duration::from_secs(30)).await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        let output = if result.stdout.is_empty() {
            "No matches found".to_string()
        } else {
            result.stdout
        };

        Ok(AgentToolResult::text(output))
    }
}