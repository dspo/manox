// Read tool — reads files with optional offset and limit.
use crate::tool::{AgentTool, AgentToolResult, ToolError, ToolContext};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

pub struct ReadTool;

#[async_trait::async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str { "read" }
    fn description(&self) -> &str { "Read a file from the filesystem" }
    fn is_read_only(&self) -> bool { true }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file" },
                "offset": { "type": "integer", "description": "Line offset" },
                "limit": { "type": "integer", "description": "Max lines to read" }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        _signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let path = params["path"].as_str().ok_or_else(|| ToolError::InvalidArguments("path is required".into()))?;
        let offset = params["offset"].as_u64().map(|v| v as usize);
        let limit = params["limit"].as_u64().map(|v| v as usize);

        let path = ctx.cwd().join(path);
        let content = ctx.env().read_file(&path, offset, limit).await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        Ok(AgentToolResult::text(content))
    }
}