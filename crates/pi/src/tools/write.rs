// Write tool — creates or overwrites a file.
use crate::tool::{AgentTool, AgentToolResult, ToolError, ToolContext};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

pub struct WriteTool;

#[async_trait::async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str { "write" }
    fn description(&self) -> &str { "Write content to a file" }
    fn is_read_only(&self) -> bool { false }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file" },
                "content": { "type": "string", "description": "Content to write" }
            },
            "required": ["path", "content"]
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
        let content = params["content"].as_str().ok_or_else(|| ToolError::InvalidArguments("content is required".into()))?;

        let path = ctx.cwd().join(path);
        ctx.env().write_file(&path, content).await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        Ok(AgentToolResult::text(format!("Wrote file: {path}", path = path.display())))
    }
}