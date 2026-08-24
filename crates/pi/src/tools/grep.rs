// Grep tool — in-process content search with regex.
//
// Uses the `grep-searcher` engine for matching and `ignore` for filesystem
// traversal (respects .gitignore). No shell execution — the LLM's input is
// treated as data, not as a command string.

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSetBuilder};
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::WalkBuilder;
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
    /// Default max matches, matching the upstream tool.
    const DEFAULT_LIMIT: usize = 100;
}

#[async_trait::async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "Grep"
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

        // Build the matcher on the ripgrep engine. Literal mode escapes the
        // pattern first; the engine otherwise interprets it as a regex.
        let matcher = {
            let pattern = if is_literal {
                regex::escape(pattern)
            } else {
                pattern.to_string()
            };
            RegexMatcherBuilder::new()
                .case_insensitive(ignore_case)
                .multi_line(true)
                .build(&pattern)
                .map_err(|e| ToolError::InvalidArguments(format!("invalid regex: {e}")))?
        };

        // Resolve the search path.
        let search_path = resolve_path(ctx, path_str);

        // Build the file glob filter.
        let glob_set = build_glob_filter(glob_pattern)?;

        // Walk the directory tree and collect matches.
        let (matches, matched_paths) =
            search_files(&search_path, &matcher, &glob_set, context_lines, limit);
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
                    let tag = store.record(path, &normalized);
                    // Extract line numbers from matches for this file.
                    let path_str = path.display().to_string();
                    let lines: std::collections::HashSet<usize> = matches
                        .iter()
                        .filter_map(|m| {
                            let prefix = format!("{path_str}:");
                            if let Some(rest) = m.strip_prefix(&prefix) {
                                rest.split(':').next()?.parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .collect();
                    if !lines.is_empty() {
                        store.record_seen_lines(path, &tag.tag, &lines);
                    }
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

/// Search files for a pattern with the ripgrep engine. Returns the formatted
/// matches plus the paths of files that produced at least one match, in
/// first-match order (deduplicated), so the caller can record hashline
/// snapshots for them.
fn search_files(
    search_path: &Path,
    matcher: &RegexMatcher,
    glob_set: &Option<globset::GlobSet>,
    context_lines: usize,
    limit: usize,
) -> (Vec<String>, Vec<PathBuf>) {
    let mut results: Vec<String> = Vec::new();
    let mut matched_paths: Vec<PathBuf> = Vec::new();
    let mut searcher = SearcherBuilder::new()
        // Ripgrep semantics: a NUL byte marks the file binary, and the search
        // stops there instead of reporting garbage matches.
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();

    let walker = WalkBuilder::new(search_path)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .max_depth(None)
        .build();

    for entry in walker {
        if results.len() >= limit {
            break;
        }
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

        let mut sink = CollectSink {
            remaining: limit - results.len(),
            matches: Vec::new(),
        };
        let _ = searcher.search_path(matcher, path, &mut sink);
        if sink.matches.is_empty() {
            continue;
        }

        // Paths read relative to the search root: an absolute path repeats
        // the root on every match, spending context on nothing. Searching a
        // single file strips to nothing, so that case falls back to the
        // file name rather than emitting an empty path.
        let display_path = match path.strip_prefix(search_path) {
            Ok(rel) if rel.as_os_str().is_empty() => path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            Ok(rel) => rel.display().to_string(),
            Err(_) => path.display().to_string(),
        };

        if context_lines > 0 {
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            let lines: Vec<&str> = content.lines().collect();
            for (line_no, _) in &sink.matches {
                results.push(format_with_context(
                    &lines,
                    line_no.saturating_sub(1) as usize,
                    context_lines,
                    &display_path,
                ));
            }
        } else {
            for (line_no, line) in &sink.matches {
                results.push(format!("{display_path}:{line_no}:{line}"));
            }
        }

        // Record only files that produced output: a context search that can no
        // longer re-read the file yields no lines and no snapshot entry.
        if !matched_paths.iter().any(|p| p == path) {
            matched_paths.push(path.to_path_buf());
        }
    }

    (results, matched_paths)
}

/// Collects matched lines for one file while the searcher runs. The searcher
/// feeds each line separately; `remaining` bounds the total emitted results
/// across files, stopping the search of a file once the limit is reached.
struct CollectSink {
    remaining: usize,
    matches: Vec<(u64, String)>,
}

impl Sink for CollectSink {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch) -> Result<bool, Self::Error> {
        if self.matches.len() >= self.remaining {
            return Ok(false);
        }
        // `bytes()` includes the line terminator; strip it and the `\r` of a
        // CRLF ending so CRLF files report the same line content as
        // `str::lines()` does — the output stays byte-identical to the
        // previous line-by-line scan. A lone trailing `\r` at EOF (no `\n`)
        // is content and survives, matching `str::lines()`.
        let mut line = String::from_utf8_lossy(mat.bytes()).into_owned();
        if line.ends_with('\n') {
            line.pop();
            if line.ends_with('\r') {
                line.pop();
            }
        }
        self.matches.push((mat.line_number().unwrap_or(0), line));
        Ok(true)
    }
}

/// Format a match with surrounding context lines.
fn format_with_context(lines: &[&str], line_idx: usize, context: usize, path: &str) -> String {
    let start = line_idx.saturating_sub(context);
    let end = (line_idx + context + 1).min(lines.len());

    let mut output = String::new();
    output.push_str(&format!("--- {path} ---\n"));

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
    fn matches_report_paths_relative_to_the_search_root() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src/deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("hit.rs"), "fn needle() {}\n").unwrap();

        let matcher = RegexMatcherBuilder::new().build("needle").unwrap();
        let (matches, _) = search_files(dir.path(), &matcher, &None, 0, 10);
        let output = matches.join("\n");
        assert!(
            output.contains("src/deep/hit.rs:1:"),
            "path must be relative to the search root: {output}"
        );
        assert!(
            !output.contains(&dir.path().display().to_string()),
            "the absolute root must not repeat on every match: {output}"
        );
    }

    #[test]
    fn searching_a_single_file_reports_its_name() {
        // Stripping the search root off the root itself leaves nothing, so this
        // case must fall back to the file name or the match loses its path.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("target.rs");
        std::fs::write(&file, "fn needle() {}\n").unwrap();

        let matcher = RegexMatcherBuilder::new().build("needle").unwrap();
        let (matches, _) = search_files(&file, &matcher, &None, 0, 10);
        let output = matches.join("\n");
        assert!(
            output.starts_with("target.rs:1:"),
            "a single-file search must still name the file: {output}"
        );
    }

    #[test]
    fn crlf_lines_report_content_without_trailing_cr() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("win.rs");
        std::fs::write(&file, "fn a() {\r\nneedle()\r\n}\r\n").unwrap();

        let matcher = RegexMatcherBuilder::new().build("needle").unwrap();
        let (matches, _) = search_files(dir.path(), &matcher, &None, 0, 10);
        let output = matches.join("\n");
        assert!(
            output.contains("win.rs:2:needle()"),
            "CRLF content must be reported without a trailing \\r: {output:?}"
        );
        assert!(
            !output.contains('\r'),
            "no carriage return may leak into the output: {output:?}"
        );
    }

    #[test]
    fn test_format_with_context() {
        let lines: Vec<&str> = vec!["line0", "line1", "line2", "line3", "line4"];
        let output = format_with_context(&lines, 2, 1, "test.txt");
        assert!(output.contains("test.txt"));
        assert!(output.contains("> 3:line2"));
        assert!(output.contains("  2:line1"));
        assert!(output.contains("  4:line3"));
    }
}
