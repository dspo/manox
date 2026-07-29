// Write tool — creates or overwrites a file with diff output.
//
// Content that looks like pasted `read` output (a `[path#TAG]` header followed
// by `N:`-prefixed rows) is stripped of those prefixes before writing. After
// writing, a hashline snapshot is recorded so a follow-up edit has a valid
// tag, and a unified diff shows the changes (if the file previously existed).

use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::hashline;
use crate::tool::{AgentTool, AgentToolResult, ToolError, ToolContext};
use crate::tools::edit_diff;

pub struct WriteTool;

#[async_trait::async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to the specified file (overwrite). Use to create or rewrite a file. \
         If the content accidentally pastes `read` output (with a `[path#tag]` header and \
         `N:` line prefixes), those prefixes are stripped automatically."
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

        // Serialize against concurrent edits of the same file.
        let _guard = ctx.tool_state().mutation_queue.lock(&path).await;

        // Read existing content for diff, if the file exists.
        let old_content = ctx.env().read_file(&path, None, None).await.ok();

        let content = strip_hashline_prefixes(content);
        ctx.env()
            .write_file(&path, &content)
            .await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        // Record a snapshot so subsequent edit calls have a valid tag.
        let normalized = hashline::normalize_to_lf(&content);
        let snap = ctx
            .tool_state()
            .snapshots
            .lock()
            .expect("hashline snapshot store poisoned")
            .record(&path, &normalized);

        let mut output = format!("Wrote file: {path}", path = path.display());

        // Show a diff if the file previously existed.
        if let Some(old) = old_content {
            let diff = edit_diff::compute_unified_diff(&old, &content, &path);
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

        output.push_str(&format!("\n[{path_display}#{tag}]", path_display = path.display(), tag = snap.tag));

        Ok(AgentToolResult::text(output))
    }
}

/// Strip hashline prefixes from content if it looks like accidental `read`
/// output paste. Returns stripped content, or original if no prefixes detected.
///
/// Detection: first non-empty line matches `[path#tag]` header pattern AND
/// subsequent lines match `^\d+:` line number pattern. Conservative heuristic
/// to avoid stripping legitimate content that happens to start with `[`.
fn strip_hashline_prefixes(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return content.to_string();
    }

    // Find first non-empty line and check for hashline header.
    let first_non_empty = lines.iter().position(|l| !l.is_empty());
    let Some(header_idx) = first_non_empty else {
        return content.to_string();
    };
    let header = lines[header_idx];

    // Check if it matches hashline header pattern: `[path#tag]` where tag is 4 hex chars.
    let is_header = header.starts_with('[')
        && header.ends_with(']')
        && header.contains('#')
        && header.len() >= 7; // Minimal: `[#xxxx]`
    if !is_header {
        return content.to_string();
    }

    // Check if subsequent non-empty lines match `^\d+:` pattern.
    let has_line_numbers = lines[header_idx + 1..]
        .iter()
        .filter(|l| !l.is_empty())
        .take(5) // Sample first 5 non-empty lines after header
        .all(|l| {
            l.split_once(':')
                .map(|(n, _)| n.parse::<usize>().is_ok())
                .unwrap_or(false)
        });

    if !has_line_numbers {
        return content.to_string();
    }

    // Strip: skip the header line, remove `N:` prefix from all subsequent lines.
    let mut result = String::new();
    for line in &lines[header_idx + 1..] {
        if let Some((_num, rest)) = line.split_once(':') {
            result.push_str(rest);
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }
    // Preserve trailing newline status from original.
    if !content.ends_with('\n') && result.ends_with('\n') {
        result.pop();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_hashline_prefixes_with_header_and_numbers() {
        let input = "[src/foo.rs#ABCD]\n1:fn main() {\n2:    println!(\"hello\");\n3:}";
        let result = strip_hashline_prefixes(input);
        assert_eq!(result, "fn main() {\n    println!(\"hello\");\n}");
    }

    #[test]
    fn test_strip_hashline_prefixes_no_header() {
        let input = "fn main() {\n    println!(\"hello\");\n}";
        let result = strip_hashline_prefixes(input);
        assert_eq!(result, input);
    }

    #[test]
    fn test_strip_hashline_prefixes_header_only() {
        let input = "[src/foo.rs#ABCD]\nfn main() {\n    println!(\"hello\");\n}";
        let result = strip_hashline_prefixes(input);
        assert_eq!(result, input); // No line numbers, don't strip
    }

    #[test]
    fn test_strip_hashline_prefixes_empty() {
        let input = "";
        let result = strip_hashline_prefixes(input);
        assert_eq!(result, "");
    }

    #[test]
    fn test_strip_hashline_prefixes_with_blank_lines() {
        let input = "[src/foo.rs#ABCD]\n\n1:fn main() {\n2:\n3:}";
        let result = strip_hashline_prefixes(input);
        assert_eq!(result, "\nfn main() {\n\n}");
    }

    #[test]
    fn test_strip_hashline_prefixes_trailing_newline_preserved() {
        let input = "[src/foo.rs#ABCD]\n1:fn main() {\n2:}\n";
        let result = strip_hashline_prefixes(input);
        assert_eq!(result, "fn main() {\n}\n");
    }
}
