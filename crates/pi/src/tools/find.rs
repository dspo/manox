// Find tool — in-process file search by glob pattern.
//
// Uses `ignore` for filesystem traversal and `globset` for pattern matching.
// No shell execution — the LLM's input is treated as data, not as a command string.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use crate::tools::truncate::{self, TruncateConfig};

pub struct FindTool;

impl FindTool {
    /// Default max output bytes.
    const DEFAULT_MAX_BYTES: usize = 128 * 1024;
    /// Default max output lines.
    const DEFAULT_MAX_LINES: usize = 2000;
    /// Default limit for results.
    const DEFAULT_LIMIT: usize = 1000;
}

#[async_trait::async_trait]
impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        "Find files matching a glob pattern"
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
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of results to return"
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

        let search_path = resolve_path(ctx, path_str);

        let glob = Glob::new(pattern)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid glob pattern: {e}")))?;

        let mut builder = GlobSetBuilder::new();
        builder.add(glob);
        let glob_set = builder
            .build()
            .map_err(|e| ToolError::InvalidArguments(format!("invalid glob: {e}")))?;

        let mut results: Vec<String> = Vec::new();

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

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                continue;
            }

            let path = entry.path();

            if !glob_set.is_match(path) {
                continue;
            }

            if results.len() >= limit {
                break;
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

            results.push(display_path);
        }

        if results.is_empty() {
            return Ok(AgentToolResult::text("No files found"));
        }

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
                "\n\n[find: {} files, {} lines, {} bytes — output truncated]",
                results.len(),
                result.original_lines,
                result.original_bytes
            ));
        }

        Ok(AgentToolResult::text(output))
    }
}

/// Resolve a path string to an absolute path.
fn resolve_path(ctx: &dyn ToolContext, path_str: &str) -> PathBuf {
    let path = Path::new(path_str);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.cwd().join(path)
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

        let result = FindTool
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
    fn test_find_tool_schema() {
        let tool = FindTool;
        let schema = tool.parameters_schema();
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("pattern")));
    }
}
