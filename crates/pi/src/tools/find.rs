// Find tool — file search by pattern.
use crate::tool::{AgentTool, AgentToolResult, ToolError, ToolContext};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

pub struct FindTool;

#[async_trait::async_trait]
impl AgentTool for FindTool {
    fn name(&self) -> &str { "find" }
    fn description(&self) -> &str { "Find files matching a pattern" }
    fn is_read_only(&self) -> bool { true }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "File pattern (glob)" },
                "path": { "type": "string", "description": "Directory to search" },
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
        let path = params["path"].as_str().unwrap_or(".");
        let limit = params["limit"].as_u64().unwrap_or(100);

        let cmd = format!("find {path} -name '{pattern}' -not -path '*/node_modules/*' -not -path '*/.git/*' | head -n {limit}");

        let result = ctx.env().exec(&cmd, std::time::Duration::from_secs(30)).await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        let output = if result.stdout.is_empty() {
            "No files found".to_string()
        } else {
            result.stdout
        };

        Ok(AgentToolResult::text(output))
    }
}