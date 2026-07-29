// Write tool — creates or overwrites a file with diff output.
//
// After writing, computes a unified diff showing the changes (if the file
// previously existed).

use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentTool, AgentToolResult, ToolError, ToolContext};
use crate::tools::edit_diff;

pub struct WriteTool;

#[async_trait::async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file"
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write"
                }
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
        let path_str = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("path is required".into()))?;
        let content = params["content"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("content is required".into()))?;

        let path = ctx.cwd().join(path_str);

        // Read existing content for diff, if the file exists.
        let old_content = ctx.env().read_file(&path, None, None).await.ok();

        ctx.env()
            .write_file(&path, content)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        let mut output = format!("Wrote file: {path}", path = path.display());

        // Show a diff if the file previously existed.
        if let Some(old) = old_content {
            let diff = edit_diff::compute_unified_diff(&old, content, &path);
            if !edit_diff::is_diff_empty(&diff) {
                let hunks = edit_diff::count_diff_hunks(&diff);
                output.push_str(&format!(
                    "\n\nDiff ({hunks} hunk(s)):\n```diff\n{diff}```"
                ));
            }
        } else {
            let line_count = content.lines().count();
            let byte_count = content.len();
            output.push_str(&format!(
                "\nNew file: {line_count} lines, {byte_count} bytes"
            ));
        }

        Ok(AgentToolResult::text(output))
    }
}