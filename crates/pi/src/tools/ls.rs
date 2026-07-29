// Ls tool — directory listing with output truncation.

use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentTool, AgentToolResult, ToolError, ToolContext};
use crate::tools::truncate::{self, TruncateConfig};

pub struct LsTool;

impl LsTool {
    /// Default max bytes for output.
    const DEFAULT_MAX_BYTES: usize = 128 * 1024;
    /// Default max lines for output.
    const DEFAULT_MAX_LINES: usize = 2000;
    /// Default limit for entries.
    const DEFAULT_LIMIT: usize = 200;
}

#[async_trait::async_trait]
impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List directory contents"
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
                    "description": "Directory to list (default: cwd)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of entries to return"
                }
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
        let path_str = params["path"].as_str().unwrap_or(".");
        let limit = params["limit"]
            .as_u64()
            .map(|v| v as usize)
            .unwrap_or(Self::DEFAULT_LIMIT);

        let path = ctx.cwd().join(path_str);
        let entries = ctx
            .env()
            .list_dir(&path)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        let total = entries.len();

        let mut output = String::new();
        for entry in entries.iter().take(limit) {
            let kind = if entry.is_dir { "d" } else { "f" };
            let name = entry
                .path
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or_else(|| entry.path.to_string_lossy());
            let size = if entry.is_dir {
                "-".to_string()
            } else {
                format_size(entry.size)
            };
            output.push_str(&format!("{kind} {size:>8}  {name}\n"));
        }

        if total > limit {
            output.push_str(&format!(
                "\n... and {} more entries (showing {}/{})",
                total - limit,
                limit,
                total
            ));
        }

        if output.is_empty() {
            output = "Directory is empty".to_string();
        }

        // Truncate if too large.
        let config = TruncateConfig {
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_lines: Self::DEFAULT_MAX_LINES,
        };
        let result = truncate::truncate(&output, &config);

        let mut final_output = result.content;
        if result.was_truncated {
            final_output.push_str(&format!(
                "\n\n[output truncated: {} lines, {} bytes]",
                result.original_lines, result.original_bytes
            ));
        }

        Ok(AgentToolResult::text(final_output))
    }
}

/// Format a file size in a human-readable way.
fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "K", "M", "G", "T"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{}B", bytes)
    } else {
        format!("{:.1}{}", size, UNITS[unit_idx])
    }
}