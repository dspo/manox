// Edit tool — search-and-replace with diff-based fuzzy matching.
//
// The edit tool tries an exact string match first. If that fails, it uses
// the `similar` crate to compute a line-by-line diff and finds the closest
// matching block. This handles cases where the LLM's `oldText` has slightly
// different whitespace or indentation than the actual file content.

use crate::tool::{AgentTool, AgentToolResult, ToolError, ToolContext};
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

/// Ratio threshold for fuzzy matching (0.0–1.0).
/// Values below this are considered not a match.
const FUZZY_THRESHOLD: f64 = 0.6;

pub struct EditTool;

#[async_trait::async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str { "edit" }
    fn description(&self) -> &str { "Edit a file by replacing text" }
    fn is_read_only(&self) -> bool { false }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file" },
                "oldText": { "type": "string", "description": "Text to replace" },
                "newText": { "type": "string", "description": "Replacement text" },
                "replaceAll": {
                    "type": "boolean",
                    "description": "Replace all occurrences (default: false)"
                }
            },
            "required": ["path", "oldText", "newText"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        _signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let path_str = params["path"].as_str()
            .ok_or_else(|| ToolError::InvalidArguments("path is required".into()))?;
        let old_text = params["oldText"].as_str()
            .ok_or_else(|| ToolError::InvalidArguments("oldText is required".into()))?;
        let new_text = params["newText"].as_str()
            .ok_or_else(|| ToolError::InvalidArguments("newText is required".into()))?;
        let replace_all = params["replaceAll"].as_bool().unwrap_or(false);

        let path = ctx.cwd().join(path_str);
        let content = ctx.env().read_file(&path, None, None).await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        let new_content = if replace_all {
            // Replace all: exact match only.
            let count = content.matches(old_text).count();
            if count == 0 {
                return Err(ToolError::ExecutionFailed(
                    "oldText not found in file (replaceAll mode requires exact match)".into()
                ));
            }
            content.replace(old_text, new_text)
        } else {
            // Single replace: try exact match first, then fuzzy.
            match try_exact_replace(&content, old_text, new_text) {
                Ok(result) => result,
                Err(_) => {
                    // Try fuzzy matching.
                    try_fuzzy_replace(&content, old_text, new_text)?
                }
            }
        };

        ctx.env().write_file(&path, &new_content).await
            .map_err(|e| ToolError::ExecutionFailed(format!("{e}")))?;

        Ok(AgentToolResult::text(format!(
            "Edited file: {path}", path = path.display()
        )))
    }
}

/// Try an exact string match replacement.
fn try_exact_replace(
    content: &str,
    old_text: &str,
    new_text: &str,
) -> Result<String, ()> {
    let count = content.matches(old_text).count();
    if count == 0 {
        return Err(());
    }
    if count > 1 {
        return Err(()); // Not unique — caller should use replaceAll or be more specific.
    }
    Ok(content.replacen(old_text, new_text, 1))
}

/// Try fuzzy matching using line-by-line diff similarity.
///
/// Splits both the file content and oldText into lines, then uses the
/// `similar` crate to find the best matching block. Handles whitespace
/// normalization for better matching.
fn try_fuzzy_replace(
    content: &str,
    old_text: &str,
    new_text: &str,
) -> Result<String, ToolError> {
    let file_lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<&str> = old_text.lines().collect();

    if old_lines.is_empty() {
        return Err(ToolError::ExecutionFailed("oldText is empty".into()));
    }

    // Find the best matching block in the file.
    let best_match = find_best_match(&file_lines, &old_lines);

    match best_match {
        Some((start, end, similarity)) => {
            if similarity < FUZZY_THRESHOLD {
                return Err(ToolError::ExecutionFailed(format!(
                    "oldText not found in file (best fuzzy match similarity: {:.2}, threshold: {:.2})",
                    similarity, FUZZY_THRESHOLD
                )));
            }

            // Replace the matched block with the new text.
            let mut result_lines: Vec<&str> = file_lines[..start].to_vec();
            for line in new_text.lines() {
                result_lines.push(line);
            }
            result_lines.extend_from_slice(&file_lines[end..]);

            Ok(result_lines.join("\n"))
        }
        None => {
            Err(ToolError::ExecutionFailed(
                "oldText not found in file (no fuzzy match)".into()
            ))
        }
    }
}

/// Find the best matching block of `old_lines` in `file_lines`.
///
/// Returns `(start_index, end_index, similarity_ratio)` of the best match.
fn find_best_match(
    file_lines: &[&str],
    old_lines: &[&str],
) -> Option<(usize, usize, f64)> {
    if file_lines.is_empty() || old_lines.is_empty() {
        return None;
    }

    let old_len = old_lines.len();
    let mut best_similarity = 0.0f64;
    let mut best_match: Option<(usize, usize)> = None;

    // Slide a window of size old_lines.len() across the file.
    for start in 0..=file_lines.len().saturating_sub(old_len) {
        let end = start + old_len;
        let window = &file_lines[start..end];

        let similarity = compute_similarity(window, old_lines);

        if similarity > best_similarity {
            best_similarity = similarity;
            best_match = Some((start, end));
        }

        // Early exit on near-perfect match.
        if similarity > 0.99 {
            break;
        }
    }

    best_match.map(|(s, e)| (s, e, best_similarity))
}

/// Compute the similarity ratio between two slices of lines.
///
/// Uses the `similar` crate's `ChangeTag` to compute a token-level ratio.
fn compute_similarity(a: &[&str], b: &[&str]) -> f64 {
    // Normalize whitespace for comparison.
    let a_normalized: Vec<String> = a.iter().map(|s| normalize_whitespace(s)).collect();
    let b_normalized: Vec<String> = b.iter().map(|s| normalize_whitespace(s)).collect();

    let a_text = a_normalized.join("\n");
    let b_text = b_normalized.join("\n");

    let diff = similar::TextDiff::from_lines(&a_text, &b_text);

    let mut same = 0usize;
    let mut total = 0usize;

    for change in diff.iter_all_changes() {
        total += 1;
        if matches!(change.tag(), similar::ChangeTag::Equal) {
            same += 1;
        }
    }

    if total == 0 {
        return 1.0;
    }

    same as f64 / total as f64
}

/// Normalize whitespace for comparison.
fn normalize_whitespace(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_was_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !prev_was_space {
                result.push(' ');
                prev_was_space = true;
            }
        } else {
            result.push(ch);
            prev_was_space = false;
        }
    }
    result.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_whitespace() {
        assert_eq!(normalize_whitespace("  hello   world  "), "hello world");
        assert_eq!(normalize_whitespace("\tline1\n\tline2"), "line1 line2");
    }

    #[test]
    fn test_compute_similarity_exact() {
        let a = vec!["hello", "world"];
        let b = vec!["hello", "world"];
        assert!(compute_similarity(&a, &b) > 0.99);
    }

    #[test]
    fn test_compute_similarity_different() {
        let a = vec!["hello", "world"];
        let b = vec!["goodbye", "mars"];
        assert!(compute_similarity(&a, &b) < 0.5);
    }

    #[test]
    fn test_find_best_match() {
        let file = vec![
            "fn main() {",
            "    let x = 1;",
            "    let y = 2;",
            "    println!(\"{}\", x + y);",
            "}",
        ];
        let old = vec![
            "    let x = 1;",
            "    let y = 2;",
        ];

        let result = find_best_match(&file, &old);
        assert!(result.is_some());
        let (start, end, similarity) = result.unwrap();
        assert_eq!(start, 1);
        assert_eq!(end, 3);
        assert!(similarity > 0.9);
    }

    #[test]
    fn test_fuzzy_replace_whitespace() {
        let content = "fn main() {\n    let x = 1;\n    let y = 2;\n}";
        let old_text = "let x = 1;\nlet y = 2;"; // no indentation
        let new_text = "let x = 10;\nlet y = 20;";

        let result = try_fuzzy_replace(content, old_text, new_text);
        assert!(result.is_ok());
        let new_content = result.unwrap();
        assert!(new_content.contains("let x = 10;"));
        assert!(new_content.contains("let y = 20;"));
    }
}