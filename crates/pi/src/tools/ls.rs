// Ls tool — directory listing with output truncation.
//
// Supports long format (size/mtime columns), hidden files, and recursive depth.
// Use this over `ls` in Bash for plain directory listings: no sandbox, no
// approval in read-only mode. Use Bash `ls` only when format flags exceed
// what this tool offers (e.g. `-laR`, `-lhS`, `--sort=time`).

use std::path::Path;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
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
        "Ls"
    }

    fn description(&self) -> &str {
        "List directory contents. Use this over `ls` in Bash for plain \
         directory listings: no sandbox, no approval in read-only mode. \
         Supports long format (size/mtime), hidden files, and recursive depth. \
         Use Bash `ls` only when format flags exceed what this tool offers \
         (e.g. `-laR`, `-lhS`, `--sort=time`)."
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
                "cwd": {
                    "type": "string",
                    "description": "Working directory for this call; relative paths resolve against it. Omit to reuse the previous tool call's directory (the session's start directory initially)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of entries to return"
                },
                "long": {
                    "type": "boolean",
                    "description": "Show detailed format with size and modification time columns"
                },
                "hidden": {
                    "type": "boolean",
                    "description": "Show hidden files (those starting with '.')"
                },
                "depth": {
                    "type": "integer",
                    "description": "Recursion depth for subdirectories (0 = no recursion, 1 = immediate children, etc.)"
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
        let long = params["long"].as_bool().unwrap_or(false);
        let hidden = params["hidden"].as_bool().unwrap_or(false);
        let depth = params["depth"].as_u64().map(|v| v as usize);

        let cwd = crate::tools::path_utils::resolve_effective_cwd(ctx, params["cwd"].as_str())
            .map_err(ToolError::InvalidArguments)?;
        let path = cwd.join(path_str);

        let entries = if let Some(d) = depth {
            list_recursive(&path, d, hidden, limit)
        } else {
            let entries = ctx
                .env()
                .list_dir(&path)
                .await
                .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;
            if hidden {
                entries
            } else {
                entries
                    .into_iter()
                    .filter(|e| {
                        e.path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| !n.starts_with('.'))
                            .unwrap_or(true)
                    })
                    .collect::<Vec<_>>()
            }
        };

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

            if long {
                let mtime = entry
                    .path
                    .metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| {
                        t.duration_since(UNIX_EPOCH)
                            .ok()
                            .map(|d| d.as_secs() as i64)
                    })
                    .unwrap_or(0);
                let mtime_str = if mtime > 0 {
                    // Format as ISO-like date-time or relative time.
                    let secs = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let age = secs - mtime;
                    if age < 3600 {
                        format!("{}m", age / 60)
                    } else if age < 86400 {
                        format!("{}h", age / 3600)
                    } else {
                        format!("{}d", age / 86400)
                    }
                } else {
                    "-".to_string()
                };
                output.push_str(&format!("{kind} {size:>8} {mtime_str:>5}  {name}\n"));
            } else {
                output.push_str(&format!("{kind} {size:>8}  {name}\n"));
            }
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

/// Recursively list directory contents up to a given depth.
#[allow(clippy::only_used_in_recursion)]
fn list_recursive(
    base: &Path,
    depth: usize,
    show_hidden: bool,
    _limit: usize,
) -> Vec<crate::env::FileInfo> {
    let mut results = Vec::new();
    list_recursive_impl(base, base, depth, show_hidden, &mut results);
    results
}

#[allow(clippy::only_used_in_recursion)]
fn list_recursive_impl(
    base: &Path,
    current: &Path,
    remaining: usize,
    show_hidden: bool,
    results: &mut Vec<crate::env::FileInfo>,
) {
    let Ok(entries) = std::fs::read_dir(current) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !show_hidden
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.starts_with('.')
        {
            continue;
        }
        let is_dir = path.is_dir();
        let size = if is_dir {
            0
        } else {
            path.metadata().map(|m| m.len()).unwrap_or(0)
        };
        results.push(crate::env::FileInfo {
            path: path.to_path_buf(),
            is_dir,
            size,
        });
        if is_dir && remaining > 0 {
            list_recursive_impl(base, &path, remaining - 1, show_hidden, results);
        }
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
