use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use regex::{Regex, RegexBuilder};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cx_agent::approval::ToolCategory;

use super::{Tool, display_path, parse_args, validate_path};

const DEFAULT_MAX_RESULTS: usize = 50;

pub struct GrepTool;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepArgs {
    pattern: String,
    root: Option<String>,
    file_glob: Option<String>,
    case_sensitive: Option<bool>,
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }

    fn description(&self) -> &'static str {
        "Search UTF-8 text files under a file or directory using a regular expression."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regular expression to search for."
                },
                "root": {
                    "type": "string",
                    "description": "Optional file or directory root. Defaults to the current directory."
                },
                "file_glob": {
                    "type": "string",
                    "description": "Optional glob pattern applied relative to root, for example src/**/*.rs."
                },
                "case_sensitive": {
                    "type": "boolean",
                    "description": "Whether the regex should be case sensitive."
                },
                "max_results": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of matches to return."
                }
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    async fn invoke(&self, arguments: Value) -> Result<String> {
        let args: GrepArgs = parse_args(self.name(), arguments)?;
        if args.pattern.is_empty() {
            return Err(anyhow!("grep pattern must not be empty"));
        }

        let root = match args.root.as_deref() {
            Some(raw) => validate_path(self.name(), raw)?,
            None => PathBuf::from("."),
        };
        if !root.exists() {
            return Err(anyhow!("grep root does not exist: {}", display_path(&root)));
        }

        let regex = build_regex(&args.pattern, args.case_sensitive.unwrap_or(true))?;
        let file_glob = match args.file_glob.as_deref() {
            Some(pattern) => Some(
                ::glob::Pattern::new(pattern)
                    .map_err(|err| anyhow!("grep file_glob is invalid: {err}"))?,
            ),
            None => None,
        };
        let max_results = args.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
        if max_results == 0 {
            return Err(anyhow!("grep max_results must be >= 1"));
        }

        let mut matches = Vec::new();
        let mut skipped_non_utf8 = 0usize;
        search_path(
            &root,
            &root,
            &regex,
            file_glob.as_ref(),
            max_results,
            &mut matches,
            &mut skipped_non_utf8,
        )?;

        if matches.is_empty() {
            if skipped_non_utf8 > 0 {
                Ok(format!(
                    "No matches found.\nSkipped {skipped_non_utf8} non-UTF-8 files."
                ))
            } else {
                Ok("No matches found.".to_string())
            }
        } else {
            let mut output = matches.join("\n");
            if skipped_non_utf8 > 0 {
                output.push_str(&format!("\nSkipped {skipped_non_utf8} non-UTF-8 files."));
            }
            Ok(output)
        }
    }
}

fn build_regex(pattern: &str, case_sensitive: bool) -> Result<Regex> {
    RegexBuilder::new(pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|err| anyhow!("grep pattern is invalid: {err}"))
}

fn search_path(
    root: &Path,
    path: &Path,
    regex: &Regex,
    file_glob: Option<&::glob::Pattern>,
    max_results: usize,
    matches: &mut Vec<String>,
    skipped_non_utf8: &mut usize,
) -> Result<()> {
    if matches.len() >= max_results {
        return Ok(());
    }

    if path.is_dir() {
        let mut entries = fs::read_dir(path)
            .map_err(|err| {
                anyhow!(
                    "grep failed to read directory {}: {err}",
                    display_path(path)
                )
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| {
                anyhow!(
                    "grep failed to iterate directory {}: {err}",
                    display_path(path)
                )
            })?;
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let file_type = entry.file_type().map_err(|err| {
                anyhow!(
                    "grep failed to read file type for {}: {err}",
                    display_path(&entry.path())
                )
            })?;
            if file_type.is_symlink() {
                continue;
            }
            search_path(
                root,
                &entry.path(),
                regex,
                file_glob,
                max_results,
                matches,
                skipped_non_utf8,
            )?;
            if matches.len() >= max_results {
                break;
            }
        }
        return Ok(());
    }

    if let Some(pattern) = file_glob {
        let relative = relative_path(root, path);
        if !pattern.matches_path(&relative) {
            return Ok(());
        }
    }

    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::InvalidData => {
            *skipped_non_utf8 += 1;
            return Ok(());
        }
        Err(err) => {
            return Err(anyhow!("grep failed to read {}: {err}", display_path(path)));
        }
    };

    for (line_number, line) in contents.lines().enumerate() {
        if regex.is_match(line) {
            matches.push(format!(
                "{}:{}:{}",
                relative_path(root, path).display(),
                line_number + 1,
                line
            ));
            if matches.len() >= max_results {
                break;
            }
        }
    }

    Ok(())
}

fn relative_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(PathBuf::from)
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::GrepTool;
    use crate::cx_agent::tools::Tool;
    use crate::cx_agent::tools::test_support::TestDir;

    #[tokio::test(flavor = "current_thread")]
    async fn finds_matches_with_line_numbers() {
        let dir = TestDir::new("grep");
        dir.write("src/main.rs", "fn main() {}\nlet value = 1;\n");
        dir.write("src/lib.rs", "value = 2;\n");
        let tool = GrepTool;

        let output = tool
            .invoke(json!({
                "pattern": "value",
                "root": dir.path().display().to_string(),
                "file_glob": "src/**/*.rs"
            }))
            .await
            .expect("grep output");

        assert!(output.contains("src/lib.rs:1:value = 2;"));
        assert!(output.contains("src/main.rs:2:let value = 1;"));
    }
}
