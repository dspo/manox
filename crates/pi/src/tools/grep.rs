// Grep tool — in-process content search with regex.
//
// Uses `ignore` for filesystem traversal (respects .gitignore) and `regex`
// for pattern matching. No shell execution — the LLM's input is treated as
// data, not as a command string.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::hashline;
use crate::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use crate::tools::truncate::{self, TruncateConfig};

pub struct GrepTool;

impl GrepTool {
    /// Default max output bytes.
    const DEFAULT_MAX_BYTES: usize = 128 * 1024;
    /// Default max output lines.
    const DEFAULT_MAX_LINES: usize = 2000;
    /// Default max matches.
    const DEFAULT_LIMIT: usize = 200;
}

#[async_trait::async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search for patterns in files"
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
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search (default: cwd)"
                },
                "glob": {
                    "type": "string",
                    "description": "File glob pattern to filter files (e.g. '*.rs')"
                },
                "ignoreCase": {
                    "type": "boolean",
                    "description": "Case-insensitive search"
                },
                "literal": {
                    "type": "boolean",
                    "description": "Treat pattern as a literal string, not regex"
                },
                "context": {
                    "type": "integer",
                    "description": "Lines of context around each match"
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
        let glob_pattern = params["glob"].as_str();
        let ignore_case = params["ignoreCase"].as_bool().unwrap_or(false);
        let is_literal = params["literal"].as_bool().unwrap_or(false);
        let context_lines = params["context"].as_u64().unwrap_or(0) as usize;
        let limit = params["limit"]
            .as_u64()
            .map(|v| v as usize)
            .unwrap_or(Self::DEFAULT_LIMIT);

        // Build the regex.
        let regex = if is_literal {
            let escaped = regex::escape(pattern);
            RegexBuilder::new(&escaped)
                .case_insensitive(ignore_case)
                .multi_line(true)
                .build()
                .map_err(|e| ToolError::InvalidArguments(format!("invalid pattern: {e}")))?
        } else {
            RegexBuilder::new(pattern)
                .case_insensitive(ignore_case)
                .multi_line(true)
                .build()
                .map_err(|e| ToolError::InvalidArguments(format!("invalid regex: {e}")))?
        };

        // Resolve the search path.
        let search_path = resolve_path(ctx, path_str);

        // Build the file glob filter.
        let glob_set = build_glob_filter(glob_pattern)?;

        // Walk the directory tree and collect matches.
        let (matches, matched_paths) =
            search_files(&search_path, &regex, &glob_set, context_lines, limit);

        if matches.is_empty() {
            return Ok(AgentToolResult::text("No matches found"));
        }

        // Record hashline snapshots for matched files so the model can edit
        // directly without re-reading. Limited to 20 files to bound I/O.
        {
            let mut store = ctx
                .tool_state()
                .snapshots
                .lock()
                .expect("hashline snapshot store poisoned");
            for path in matched_paths.iter().take(20) {
                if let Ok(raw) = std::fs::read_to_string(path) {
                    let normalized = hashline::normalize_to_lf(&raw);
                    let _ = store.record(path, &normalized);
                }
            }
        }

        let joined = matches.join("\n");

        // Truncate if too large.
        let config = TruncateConfig {
            max_bytes: Self::DEFAULT_MAX_BYTES,
            max_lines: Self::DEFAULT_MAX_LINES,
        };
        let result = truncate::truncate(&joined, &config);

        let mut output = result.content;
        if result.was_truncated {
            output.push_str(&format!(
                "\n\n[grep: {} matches, {} lines, {} bytes — output truncated]",
                matches.len(),
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

/// Build a `GlobSet` from an optional glob pattern string.
fn build_glob_filter(pattern: Option<&str>) -> Result<Option<globset::GlobSet>, ToolError> {
    let Some(pat) = pattern else {
        return Ok(None);
    };
    let mut builder = GlobSetBuilder::new();
    let glob = Glob::new(pat)
        .map_err(|e| ToolError::InvalidArguments(format!("invalid glob pattern: {e}")))?;
    builder.add(glob);
    let set = builder
        .build()
        .map_err(|e| ToolError::InvalidArguments(format!("invalid glob: {e}")))?;
    Ok(Some(set))
}

/// Search files for a regex pattern. Returns the formatted matches plus the
/// paths of files that produced at least one match, in first-match order
/// (deduplicated), so the caller can record hashline snapshots for them.
fn search_files(
    search_path: &Path,
    regex: &regex::Regex,
    glob_set: &Option<globset::GlobSet>,
    context_lines: usize,
    limit: usize,
) -> (Vec<String>, Vec<PathBuf>) {
    let mut results: Vec<String> = Vec::new();
    let mut matched_paths: Vec<PathBuf> = Vec::new();

    let walker = WalkBuilder::new(search_path)
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

        if let Some(globs) = glob_set
            && !globs.is_match(path)
        {
            continue;
        }

        if results.len() >= limit {
            break;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let lines: Vec<&str> = content.lines().collect();

        for (line_idx, line) in lines.iter().enumerate() {
            if results.len() >= limit {
                break;
            }

            if !regex.is_match(line) {
                continue;
            }

            let formatted = if context_lines > 0 {
                format_with_context(&lines, line_idx, context_lines, path)
            } else {
                format!("{}:{}:{}", path.display(), line_idx + 1, line)
            };

            if !matched_paths.iter().any(|p| p == path) {
                matched_paths.push(path.to_path_buf());
            }
            results.push(formatted);
        }
    }

    (results, matched_paths)
}

/// Format a match with surrounding context lines.
fn format_with_context(lines: &[&str], line_idx: usize, context: usize, path: &Path) -> String {
    let start = line_idx.saturating_sub(context);
    let end = (line_idx + context + 1).min(lines.len());

    let mut output = String::new();
    output.push_str(&format!("--- {} ---\n", path.display()));

    for (i, line) in lines.iter().enumerate().take(end).skip(start) {
        let marker = if i == line_idx { ">" } else { " " };
        output.push_str(&format!("{} {}:{}\n", marker, i + 1, line));
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_glob_filter_none() {
        let result = build_glob_filter(None).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_build_glob_filter_some() {
        let result = build_glob_filter(Some("*.rs")).unwrap();
        assert!(result.is_some());
    }

    #[test]
    fn test_build_glob_filter_invalid() {
        let result = build_glob_filter(Some("["));
        assert!(result.is_err());
    }

    #[test]
    fn test_format_with_context() {
        let lines: Vec<&str> = vec!["line0", "line1", "line2", "line3", "line4"];
        let output = format_with_context(&lines, 2, 1, Path::new("test.txt"));
        assert!(output.contains("test.txt"));
        assert!(output.contains("> 3:line2"));
        assert!(output.contains("  2:line1"));
        assert!(output.contains("  4:line3"));
    }
}
