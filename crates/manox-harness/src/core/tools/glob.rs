// Glob tool — in-process file search by glob pattern.
//
// Uses `ignore` for filesystem traversal and `globset` for pattern matching.
// No shell execution — the LLM's input is treated as data, not as a command string.
//
// Supports mtime filtering, kind (files/dirs), and mtime-sorted output.
// Use Bash with `find` only when the glob pattern language is insufficient
// (e.g. complex boolean predicates, custom sort, multi-pattern combinations).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use crate::tools::truncate::{self, TruncateConfig};

pub struct GlobTool;

impl GlobTool {
    /// Default max output bytes.
    const DEFAULT_MAX_BYTES: usize = 128 * 1024;
    /// Default max output lines.
    const DEFAULT_MAX_LINES: usize = 2000;
    /// Default limit for results.
    const DEFAULT_LIMIT: usize = 1000;
}

#[async_trait::async_trait]
impl AgentTool for GlobTool {
    fn name(&self) -> &str {
        "Glob"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern. Use this over `find` in Bash for \
         simple tree searches: no sandbox, no approval in read-only mode. \
         Supports mtime filtering, kind (files/dirs), and mtime-sorted output. \
         Use Bash `find` only when the glob language is insufficient (complex \
         boolean predicates, custom sort, multi-pattern combinations)."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "File glob pattern (e.g. '*.rs', '**/test*')"
                },
                "path": {
                    "type": "string",
                    "description": "Directory to search (default: cwd)"
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory for this call; relative paths resolve against it. Omit to reuse the previous tool call's directory (the session's start directory initially)."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return"
                },
                "modified_within_secs": {
                    "type": "integer",
                    "description": "Only return entries modified within this many seconds from now"
                },
                "kind": {
                    "type": "string",
                    "enum": ["files", "dirs"],
                    "description": "Filter to only files or only directories. Omit for both"
                },
                "sort_by_mtime": {
                    "type": "string",
                    "enum": ["asc", "desc"],
                    "description": "Sort results by modification time, ascending or descending"
                }
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
        let pattern = params["pattern"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("pattern is required".into()))?;
        let path_str = params["path"].as_str().unwrap_or(".");
        let limit = params["limit"]
            .as_u64()
            .map(|v| v as usize)
            .unwrap_or(Self::DEFAULT_LIMIT);
        let modified_within_secs = params["modified_within_secs"].as_u64();
        let kind = params["kind"].as_str();
        let sort_by_mtime = params["sort_by_mtime"].as_str();

        let cwd = crate::tools::path_utils::resolve_effective_cwd(ctx, params["cwd"].as_str())
            .map_err(ToolError::InvalidArguments)?;
        let search_path = resolve_path(path_str, &cwd);

        let glob = Glob::new(pattern)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid glob pattern: {e}")))?;

        let mut builder = GlobSetBuilder::new();
        builder.add(glob);
        let glob_set = builder
            .build()
            .map_err(|e| ToolError::InvalidArguments(format!("invalid glob: {e}")))?;

        let now = SystemTime::now();
        let cutoff = modified_within_secs.map(|secs| {
            now.duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .saturating_sub(secs)
        });

        // Collect entries with optional metadata for sorting/filtering.
        struct Entry {
            path: String,
            mtime: Option<u64>,
        }

        let mut entries: Vec<Entry> = Vec::new();

        let walker = WalkBuilder::new(&search_path)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .max_depth(None)
            .build();

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let ft = entry.file_type();
            let is_file = ft.is_some_and(|ft| ft.is_file());
            let is_dir = ft.is_some_and(|ft| ft.is_dir());

            // Apply kind filter.
            match kind {
                Some("files") if !is_file => continue,
                Some("dirs") if !is_dir => continue,
                _ => {}
            }

            // Only files and dirs, skip other types.
            if !is_file && !is_dir {
                continue;
            }

            let path = entry.path();

            if !glob_set.is_match(path) {
                continue;
            }

            // Emit paths relative to the search root. Searching a single file
            // strips to nothing, so that case falls back to the file name
            // rather than emitting an empty line.
            let display_path = match path.strip_prefix(&search_path) {
                Ok(rel) if rel.as_os_str().is_empty() => path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string()),
                Ok(rel) => rel.display().to_string(),
                Err(_) => path.display().to_string(),
            };

            // Get mtime for filtering and sorting.
            let mtime = std::fs::metadata(path).ok().and_then(|m| {
                m.modified().ok().and_then(|t| {
                    t.duration_since(SystemTime::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs())
                })
            });

            // Apply mtime filter.
            if let Some(cutoff) = cutoff {
                match mtime {
                    Some(mtime) if mtime < cutoff => continue,
                    None => continue, // skip if we can't get mtime and filter is active
                    _ => {}
                }
            }

            entries.push(Entry {
                path: display_path,
                mtime,
            });
        }

        // Sort by mtime if requested.
        if let Some(order) = sort_by_mtime {
            match order {
                "asc" => entries.sort_by_key(|e| e.mtime),
                "desc" => {
                    entries.sort_by_key(|e| std::cmp::Reverse(e.mtime));
                }
                _ => {}
            }
        }

        if entries.is_empty() {
            return Ok(AgentToolResult::text("No files found"));
        }

        let results: Vec<&str> = entries
            .iter()
            .take(limit)
            .map(|e| e.path.as_str())
            .collect();
        let joined = results.join("\n");

        // Truncate if too large.
        let config = TruncateConfig {
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_lines: Self::DEFAULT_MAX_LINES,
        };
        let result = truncate::truncate(&joined, &config);

        let mut output = result.content;
        if result.was_truncated {
            output.push_str(&format!(
                "\n\n[glob: {} files, {} lines, {} bytes — output truncated]",
                entries.len(),
                result.original_lines,
                result.original_bytes
            ));
        }

        Ok(AgentToolResult::text(output))
    }
}

/// Resolve a path string against the call's effective working directory.
fn resolve_path(path_str: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(path_str);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Searching a single file strips the root to nothing, so the result must
    /// fall back to the file name rather than emitting a blank line.
    #[tokio::test]
    async fn searching_a_single_file_reports_its_name() {
        struct Ctx {
            env: crate::env::TokioExecutionEnv,
            cwd: PathBuf,
            state: crate::tool::ToolState,
        }
        impl ToolContext for Ctx {
            fn env(&self) -> &dyn crate::env::ExecutionEnv {
                &self.env
            }
            fn cwd(&self) -> &Path {
                &self.cwd
            }
            fn tool_state(&self) -> &crate::tool::ToolState {
                &self.state
            }
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("target.rs"), "x").unwrap();
        let ctx = Ctx {
            env: crate::env::TokioExecutionEnv::new(dir.path()),
            cwd: dir.path().to_path_buf(),
            state: crate::tool::ToolState::new(),
        };

        let result = GlobTool
            .execute(
                "t1",
                serde_json::json!({ "pattern": "*.rs", "path": "target.rs" }),
                CancellationToken::new(),
                &ctx,
            )
            .await
            .unwrap();
        let text = match &result.content[0] {
            crate::types::ContentBlock::Text { text, .. } => text.clone(),
            other => panic!("expected text: {other:?}"),
        };
        assert!(
            text.contains("target.rs"),
            "a single-file search must name the file: {text}"
        );
    }

    #[test]
    fn test_glob_tool_schema() {
        let tool = GlobTool;
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("pattern")));
    }
}
