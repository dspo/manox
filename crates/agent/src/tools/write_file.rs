//! `write_file` tool: create or overwrite a file, sandbox-confined.
//! Strips accidental hashline prefixes and records a snapshot for subsequent edits.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::sandbox::SandboxPolicy;
use crate::tool::AgentTool;

use super::{resolve_path_for_write, schema};

pub struct WriteTool {
    pub(crate) cwd: Arc<PathBuf>,
    pub(crate) sandbox: SandboxPolicy,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WriteFileInput {
    /// File path to write.
    path: String,
    /// Full content to write. If the model accidentally pastes `read_file` output
    /// (with `[path#tag]` header and `N:` line prefixes), those prefixes are
    /// stripped automatically.
    content: String,
}

impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        super::WRITE
    }
    fn description(&self) -> &str {
        "Write content to the specified file (overwrite). Use to create or rewrite a file."
    }
    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<WriteFileInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parsed = match serde_json::from_value::<WriteFileInput>(input) {
            Ok(p) => p,
            Err(e) => {
                return cx.background_spawn(async move { Err(format!("input parse failed: {e}")) });
            }
        };
        let path = match resolve_path_for_write(&parsed.path, &self.cwd, &self.sandbox) {
            Ok(p) => p,
            Err(e) => return cx.background_spawn(async move { Err(e) }),
        };
        let owner = ctx.agent_label().to_string();
        cx.background_spawn(async move {
            let _lock = match crate::tools::file_lock::try_acquire(&path, &owner) {
                Ok(g) => g,
                Err(held) => {
                    return Err(format!(
                        "write_file blocked: {} is being written by {}; coordinate write ranges or retry shortly",
                        path.display(),
                        held.owner
                    ));
                }
            };
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            let content = strip_hashline_prefixes(&parsed.content);
            std::fs::write(&path, &content).map_err(|e| format!("write_file failed: {e}"))?;

            // Record snapshot so subsequent edit_file calls have a valid tag.
            let normalized = crate::hashline::normalize_to_lf(&content);
            let snap = crate::hashline::global()
                .lock()
                .expect("hashline store poisoned")
                .record(&path, &normalized);

            Ok(format!(
                "Wrote {} ({} bytes) [{}#{}]",
                path.display(),
                content.len(),
                path.display(),
                snap.tag
            ))
        })
    }
}

/// Strip hashline prefixes from content if it looks like accidental `read_file`
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
