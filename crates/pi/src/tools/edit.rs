// Edit tool — search-and-replace file editing.
use crate::tool::{AgentTool, AgentToolResult, ToolError, ToolContext};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

pub struct EditTool;

#[async_trait::async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str { "edit" }
    fn description(&self) -> &str { "Edit a file by replacing text" }
    fn is_read_only(&self) -> bool { false }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file" },
                "oldText": { "type": "string", "description": "Text to replace" },
                "newText": { "type": "string", "description": "Replacement text" }
            },
            "required": ["path", "oldText", "newText"]
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
        let old_text = params["oldText"].as_str().ok_or_else(|| ToolError::InvalidArguments("oldText is required".into()))?;
        let new_text = params["newText"].as_str().ok_or_else(|| ToolError::InvalidArguments("newText is required".into()))?;

        let path = ctx.cwd().join(path);
        let content = ctx.env().read_file(&path, None, None).await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        let count = content.matches(old_text).count();
        if count == 0 {
            return Err(ToolError::ExecutionFailed("oldText not found in file".into()));
        }
        if count > 1 {
            return Err(ToolError::ExecutionFailed(
                format!("oldText found {count} times — must be unique")
            ));
        }

        let new_content = content.replacen(old_text, new_text, 1);
        ctx.env().write_file(&path, &new_content).await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        Ok(AgentToolResult::text(format!("Edited file: {path}", path = path.display())))
    }
}