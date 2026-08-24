// Read tool — reads a file, snapshots it for hashline, and returns
// `[path#TAG]` + `N:TEXT` numbered rows.
//
// An unqualified read caps at 2000 lines with a paging hint; offset/limit map
// onto a hashline `LineRange` for partial reads. Output is additionally
// truncated by a byte guard to avoid overwhelming the context window.

use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::hashline::{self, LineRange};
use crate::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use crate::tools::truncate::{self, TruncateConfig};

pub struct ReadTool;

impl ReadTool {
    /// Default max bytes for output.
    const DEFAULT_MAX_BYTES: usize = 128 * 1024;
    /// Default max lines for output.
    const DEFAULT_MAX_LINES: usize = 2000;
    /// Lines returned by an unqualified read (no offset/limit).
    const MAX_READ_LINES: usize = 2000;
}

#[async_trait::async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }
    fn description(&self) -> &str {
        "Read a file with optional line-range paging. Output format: first line \
         `[<path>#<TAG>]` (6-hex snapshot tag for follow-up edits), followed by \
         `N:TEXT` numbered rows (1-indexed). Without offset/limit the first \
         2000 lines are returned; use offset/limit to page through longer files."
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

        let raw = ctx
            .env()
            .read_file(&path, None, None)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;
        let text = hashline::normalize_to_lf(&raw);
        let path_display = path.display().to_string();

        // The snapshot always fingerprints the full file — only display is sliced.
        let snap = {
            let mut store = ctx.tool_state().snapshots.lock().expect("hashline snapshot store poisoned");
            let snap = store.record(&path, &text);
            // Record which lines were displayed.
            let displayed: std::collections::HashSet<usize> = match (offset, limit) {
                (None, None) => {
                    let count = text.lines().count();
                    if count <= ReadTool::MAX_READ_LINES {
                        (1..=count).collect()
                    } else {
                        (1..=ReadTool::MAX_READ_LINES).collect()
                    }
                }
                _ => {
                    let start = offset.unwrap_or(1);
                    let end = limit.map(|l| (start + l - 1).min(text.lines().count())).unwrap_or(text.lines().count());
                    (start..=end).collect()
                }
            };
            store.record_seen_lines(&path, &snap.tag, &displayed);
            snap
        };

        let formatted = match (offset, limit) {
            (None, None) => format_full_read(&path_display, &text, &snap.tag),
            _ => {
                let start = offset.unwrap_or(1);
                let end = limit.map(|l| start.saturating_add(l).saturating_sub(1));
                let ranges = [LineRange { start, end }];
                hashline::format_numbered_range(&path_display, &text, &snap.tag, &ranges)
            }
        };
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

/// Format an unqualified read. The output caps at [`ReadTool::MAX_READ_LINES`]
/// lines — a full-file dump of a 100k-line file would flood the context; the
/// hint points the model at offset/limit paging for the rest.
fn format_full_read(path_display: &str, text: &str, tag: &str) -> String {
    const MAX: usize = ReadTool::MAX_READ_LINES;
    let line_count = text.lines().count();
    if line_count <= MAX {
        return hashline::format_numbered(path_display, text, tag);
    }
    let ranges = [LineRange {
        start: 1,
        end: Some(MAX),
    }];
    let mut out = hashline::format_numbered_range(path_display, text, tag, &ranges);
    out.push_str(&format!(
        "\n[Showing lines 1-{MAX} of {line_count}. \
         Page through the rest with offset/limit, e.g. offset {} limit {}]",
        MAX + 1,
        MAX,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_file_is_not_capped() {
        let text = "a\nb\nc";
        let out = format_full_read("/tmp/f.txt", text, "AB12");
        assert!(out.contains("3:c"));
        assert!(!out.contains("Showing lines"));
    }

    #[test]
    fn large_file_caps_at_max_lines_with_paging_hint() {
        let text: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
        let out = format_full_read("/tmp/big.txt", &text, "AB12");
        assert!(out.contains("1:line 1"));
        assert!(out.contains("2000:line 2000"));
        // format_numbered_range appends 3 trailing context lines; nothing
        // beyond those may appear.
        assert!(!out.contains("2004:line 2004"));
        assert!(out.contains("Showing lines 1-2000 of 5000"));
        assert!(out.contains("offset 2001"), "paging hint: {out}");
    }
}
