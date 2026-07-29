// Ls tool — directory listing.
use crate::tool::{AgentTool, AgentToolResult, ToolError, ToolContext};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

pub struct LsTool;

#[async_trait::async_trait]
impl AgentTool for LsTool {
    fn name(&self) -> &str { "ls" }
    fn description(&self) -> &str { "List directory contents" }
    fn is_read_only(&self) -> bool { true }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list" },
                "limit": { "type": "integer", "description": "Max entries" }
            }
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        _signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let path = params["path"].as_str().unwrap_or(".");
        let limit = params["limit"].as_u64().unwrap_or(100);

        let path = ctx.cwd().join(path);
        let entries = ctx.env().list_dir(&path).await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        let mut output = String::new();
        for entry in entries.iter().take(limit as usize) {
            let kind = if entry.is_dir { "d" } else { "f" };
            let name = entry.path.file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_else(|| entry.path.to_string_lossy());
            let size = if entry.is_dir { "-".to_string() } else { format!("{}B", entry.size) };
            output.push_str(&format!("{kind} {size:>8}  {name}\n"));
        }

        if entries.len() > limit as usize {
            output.push_str(&format!(
                "\n... and {} more entries (limit {})",
                entries.len() - limit as usize,
                limit
            ));
        }

        if output.is_empty() {
            output = "Directory is empty".to_string();
        }

        Ok(AgentToolResult::text(output))
    }
}