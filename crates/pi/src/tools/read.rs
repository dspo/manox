// Read tool — reads files with optional offset, limit, and line-number formatting.
//
// Output is truncated to avoid overwhelming the context window.

use std::path::Path;

use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentTool, AgentToolResult, ToolError, ToolContext};
use crate::tools::truncate::{self, TruncateConfig};

pub struct ReadTool;

impl ReadTool {
    /// Default max bytes for output.
    const DEFAULT_MAX_BYTES: usize = 128 * 1024;
    /// Default max lines for output.
    const DEFAULT_MAX_LINES: usize = 2000;
}

#[async_trait::async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read a file from the filesystem"
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file"
                },
                "offset": {
                    "type": "integer",
                    "description": "Line number to start reading from (1-based)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of lines to read"
                }
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
        let path_str = params["path"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("path is required".into()))?;
        let offset = params["offset"].as_u64().map(|v| v as usize);
        let limit = params["limit"].as_u64().map(|v| v as usize);

        let path = ctx.cwd().join(path_str);

        // Read the whole file first, then apply offset/limit.
        let content = ctx
            .env()
            .read_file(&path, None, None)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        let lines: Vec<&str> = content.lines().collect();
        let start_line = offset.map(|o| o.saturating_sub(1)).unwrap_or(0);
        let end_line = match limit {
            Some(l) => (start_line + l).min(lines.len()),
            None => lines.len(),
        };

        if start_line >= lines.len() {
            return Ok(AgentToolResult::text(""));
        }

        let selected = &lines[start_line..end_line];
        let display_start = start_line + 1;

        let formatted = format_with_line_numbers(selected, display_start, &path);

        // Truncate if too large.
        let config = TruncateConfig {
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_lines: Self::DEFAULT_MAX_LINES,
        };
        let result = truncate::truncate(&formatted, &config);

        let mut output = result.content;
        if result.was_truncated {
            output.push_str(&format!(
                "\n\n[read: {} lines, {} bytes — output truncated]",
                result.original_lines, result.original_bytes
            ));
        }

        Ok(AgentToolResult::text(output))
    }
}

/// Format lines with line numbers, mimicking `cat -n`.
fn format_with_line_numbers(lines: &[&str], start_line: usize, _path: &Path) -> String {
    let total_lines = lines.len();
    let line_num_width = format!("{}", start_line + total_lines - 1).len().max(1);

    let mut output = String::new();
    for (i, line) in lines.iter().enumerate() {
        let line_num = start_line + i;
        output.push_str(&format!(
            "{:>width$}\t{}\n",
            line_num,
            line,
            width = line_num_width
        ));
    }

    output
}