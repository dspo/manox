use std::fs;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cx_agent::approval::ToolCategory;

use super::{Tool, display_path, parse_args, validate_path};

pub struct ReadFileTool;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadFileArgs {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }

    fn description(&self) -> &'static str {
        "Read a UTF-8 text file, optionally slicing by 1-based line range."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to a UTF-8 text file."
                },
                "start_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional 1-based start line."
                },
                "end_line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Optional 1-based end line, inclusive."
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    async fn invoke(&self, arguments: Value) -> Result<String> {
        let args: ReadFileArgs = parse_args(self.name(), arguments)?;
        let path = validate_path(self.name(), &args.path)?;
        let contents = fs::read_to_string(&path)
            .map_err(|err| anyhow!("read_file failed to read {}: {err}", display_path(&path)))?;

        let lines = split_lines(&contents);
        if lines.is_empty() {
            return Ok(format!("path: {}\n(empty file)", display_path(&path)));
        }

        let start_line = args.start_line.unwrap_or(1);
        let end_line = args.end_line.unwrap_or(lines.len());

        if start_line == 0 {
            bail!("read_file start_line must be >= 1");
        }
        if end_line == 0 {
            bail!("read_file end_line must be >= 1");
        }
        if start_line > lines.len() {
            bail!(
                "read_file start_line {} is past the end of {} ({} lines)",
                start_line,
                display_path(&path),
                lines.len()
            );
        }
        if end_line < start_line {
            bail!("read_file end_line must be >= start_line");
        }

        let end_index = end_line.min(lines.len());
        let body = lines[(start_line - 1)..end_index]
            .iter()
            .enumerate()
            .map(|(idx, line)| format!("{}: {}", start_line + idx, line))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(format!("path: {}\n{}", display_path(&path), body))
    }
}

fn split_lines(contents: &str) -> Vec<String> {
    if contents.is_empty() {
        return Vec::new();
    }
    contents.lines().map(ToString::to_string).collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::ReadFileTool;
    use crate::cx_agent::tools::Tool;
    use crate::cx_agent::tools::test_support::TestDir;

    #[tokio::test(flavor = "current_thread")]
    async fn reads_requested_line_range() {
        let dir = TestDir::new("read-file");
        let path = dir.write("notes.txt", "alpha\nbeta\ngamma\n");
        let tool = ReadFileTool;

        let output = tool
            .invoke(json!({
                "path": path.display().to_string(),
                "start_line": 2,
                "end_line": 3
            }))
            .await
            .expect("read file");

        assert!(output.contains("2: beta"));
        assert!(output.contains("3: gamma"));
        assert!(!output.contains("1: alpha"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_invalid_ranges() {
        let dir = TestDir::new("read-file-errors");
        let path = dir.write("notes.txt", "alpha\n");
        let tool = ReadFileTool;

        let err = tool
            .invoke(json!({
                "path": path.display().to_string(),
                "start_line": 3
            }))
            .await
            .expect_err("range should fail");

        assert!(err.to_string().contains("past the end"));
    }
}
