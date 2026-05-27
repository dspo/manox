use std::fs;

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cx_agent::approval::ToolCategory;

use super::{Tool, display_path, parse_args, validate_path};

pub struct EditFileTool;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditFileArgs {
    path: String,
    patch: String,
}

#[derive(Debug)]
struct UnifiedPatch {
    hunks: Vec<Hunk>,
    trailing_newline: Option<bool>,
}

#[derive(Debug)]
struct Hunk {
    old_start: usize,
    old_count: usize,
    _new_start: usize,
    new_count: usize,
    lines: Vec<HunkLine>,
}

#[derive(Debug)]
enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Write
    }

    fn description(&self) -> &'static str {
        "Apply a unified diff patch to an existing UTF-8 text file."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to an existing UTF-8 text file."
                },
                "patch": {
                    "type": "string",
                    "description": "Unified diff patch for that file."
                }
            },
            "required": ["path", "patch"],
            "additionalProperties": false
        })
    }

    async fn invoke(&self, arguments: Value) -> Result<String> {
        let args: EditFileArgs = parse_args(self.name(), arguments)?;
        let path = validate_path(self.name(), &args.path)?;
        let original = fs::read_to_string(&path)
            .map_err(|err| anyhow!("edit_file failed to read {}: {err}", display_path(&path)))?;
        let patch = parse_patch(&args.patch)?;
        let updated = apply_patch(&original, &patch)?;
        fs::write(&path, updated.as_bytes())
            .map_err(|err| anyhow!("edit_file failed to write {}: {err}", display_path(&path)))?;
        Ok(format!(
            "Applied {} hunk(s) to {}.",
            patch.hunks.len(),
            display_path(&path)
        ))
    }
}

fn parse_patch(text: &str) -> Result<UnifiedPatch> {
    let header = Regex::new(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@(?: .*)?$")
        .map_err(|err| anyhow!("edit_file failed to build hunk regex: {err}"))?;
    let mut lines = text.lines().peekable();
    let mut hunks = Vec::new();
    let mut trailing_newline = None;

    while let Some(line) = lines.next() {
        if line.starts_with("@@") {
            let captures = header
                .captures(line)
                .ok_or_else(|| anyhow!("edit_file found malformed hunk header: {line}"))?;
            let old_start = parse_number(&captures, 1)?;
            let old_count = parse_optional_number(&captures, 2).unwrap_or(1);
            let new_start = parse_number(&captures, 3)?;
            let new_count = parse_optional_number(&captures, 4).unwrap_or(1);
            let mut hunk_lines = Vec::new();

            while let Some(next_line) = lines.peek().copied() {
                if next_line.starts_with("@@") {
                    break;
                }
                let next_line = lines.next().expect("peeked line");
                match next_line.chars().next() {
                    Some(' ') => hunk_lines.push(HunkLine::Context(next_line[1..].to_string())),
                    Some('-') => hunk_lines.push(HunkLine::Remove(next_line[1..].to_string())),
                    Some('+') => hunk_lines.push(HunkLine::Add(next_line[1..].to_string())),
                    Some('\\') if next_line == r"\ No newline at end of file" => {
                        trailing_newline = Some(false);
                    }
                    _ => {
                        return Err(anyhow!("edit_file found malformed hunk line: {next_line}"));
                    }
                }
            }

            if hunk_lines.is_empty() {
                return Err(anyhow!("edit_file found an empty hunk"));
            }

            hunks.push(Hunk {
                old_start,
                old_count,
                _new_start: new_start,
                new_count,
                lines: hunk_lines,
            });
        } else if line.trim().is_empty()
            || line.starts_with("---")
            || line.starts_with("+++")
            || line.starts_with("diff ")
            || line.starts_with("index ")
        {
            continue;
        } else {
            return Err(anyhow!(
                "edit_file expected a unified diff hunk header, found: {line}"
            ));
        }
    }

    if hunks.is_empty() {
        bail!("edit_file patch did not contain any hunks");
    }

    Ok(UnifiedPatch {
        hunks,
        trailing_newline,
    })
}

fn apply_patch(original: &str, patch: &UnifiedPatch) -> Result<String> {
    let (original_lines, original_trailing_newline) = split_lines(original);
    let mut cursor = 0usize;
    let mut result = Vec::new();

    for hunk in &patch.hunks {
        let start_index = hunk.old_start.saturating_sub(1);
        if start_index < cursor {
            return Err(anyhow!(
                "edit_file encountered overlapping hunks around line {}",
                hunk.old_start
            ));
        }
        if start_index > original_lines.len() {
            return Err(anyhow!(
                "edit_file hunk starts past end of file at line {}",
                hunk.old_start
            ));
        }

        result.extend(original_lines[cursor..start_index].iter().cloned());
        cursor = start_index;

        let mut observed_old_count = 0usize;
        let mut observed_new_count = 0usize;
        for line in &hunk.lines {
            match line {
                HunkLine::Context(expected) => {
                    let actual = original_lines.get(cursor).ok_or_else(|| {
                        anyhow!("edit_file context line {} is past end of file", cursor + 1)
                    })?;
                    if actual != expected {
                        return Err(anyhow!(
                            "edit_file context mismatch at line {}: expected {:?}, found {:?}",
                            cursor + 1,
                            expected,
                            actual
                        ));
                    }
                    result.push(actual.clone());
                    cursor += 1;
                    observed_old_count += 1;
                    observed_new_count += 1;
                }
                HunkLine::Remove(expected) => {
                    let actual = original_lines.get(cursor).ok_or_else(|| {
                        anyhow!("edit_file removal line {} is past end of file", cursor + 1)
                    })?;
                    if actual != expected {
                        return Err(anyhow!(
                            "edit_file removal mismatch at line {}: expected {:?}, found {:?}",
                            cursor + 1,
                            expected,
                            actual
                        ));
                    }
                    cursor += 1;
                    observed_old_count += 1;
                }
                HunkLine::Add(line) => {
                    result.push(line.clone());
                    observed_new_count += 1;
                }
            }
        }

        if observed_old_count != hunk.old_count {
            return Err(anyhow!(
                "edit_file hunk expected {} original lines but matched {}",
                hunk.old_count,
                observed_old_count
            ));
        }
        if observed_new_count != hunk.new_count {
            return Err(anyhow!(
                "edit_file hunk expected {} new lines but produced {}",
                hunk.new_count,
                observed_new_count
            ));
        }
    }

    result.extend(original_lines[cursor..].iter().cloned());
    let trailing_newline = patch.trailing_newline.unwrap_or(original_trailing_newline);
    Ok(join_lines(&result, trailing_newline))
}

fn split_lines(text: &str) -> (Vec<String>, bool) {
    if text.is_empty() {
        return (Vec::new(), false);
    }
    let trailing_newline = text.ends_with('\n');
    let mut lines = text
        .split('\n')
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if trailing_newline {
        lines.pop();
    }
    (lines, trailing_newline)
}

fn join_lines(lines: &[String], trailing_newline: bool) -> String {
    let mut output = lines.join("\n");
    if trailing_newline && !output.is_empty() {
        output.push('\n');
    }
    output
}

fn parse_number(captures: &regex::Captures<'_>, index: usize) -> Result<usize> {
    captures[index].parse::<usize>().map_err(|err| {
        anyhow!(
            "edit_file failed to parse hunk number {}: {err}",
            &captures[index]
        )
    })
}

fn parse_optional_number(captures: &regex::Captures<'_>, index: usize) -> Option<usize> {
    captures
        .get(index)
        .and_then(|value| value.as_str().parse::<usize>().ok())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::EditFileTool;
    use crate::cx_agent::tools::Tool;
    use crate::cx_agent::tools::test_support::TestDir;

    #[tokio::test(flavor = "current_thread")]
    async fn applies_unified_diff_patch() {
        let dir = TestDir::new("edit-file");
        let path = dir.write("notes.txt", "alpha\nbeta\ngamma\n");
        let tool = EditFileTool;

        let output = tool
            .invoke(json!({
                "path": path.display().to_string(),
                "patch": "--- a/notes.txt\n+++ b/notes.txt\n@@ -1,3 +1,3 @@\n alpha\n-beta\n+delta\n gamma\n"
            }))
            .await
            .expect("patch should apply");

        assert!(output.contains("Applied 1 hunk"));
        assert_eq!(dir.read("notes.txt"), "alpha\ndelta\ngamma\n");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_invalid_patches() {
        let dir = TestDir::new("edit-file-errors");
        let path = dir.write("notes.txt", "alpha\n");
        let tool = EditFileTool;

        let err = tool
            .invoke(json!({
                "path": path.display().to_string(),
                "patch": "@@ not-a-hunk @@\n"
            }))
            .await
            .expect_err("invalid patch should fail");

        assert!(err.to_string().contains("malformed hunk header"));
    }
}
