use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cx_agent::approval::ToolCategory;

use super::{Tool, parse_args, validate_path};

const DEFAULT_MAX_RESULTS: usize = 200;

pub struct GlobTool;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobArgs {
    pattern: String,
    root: Option<String>,
    include_dirs: Option<bool>,
    max_results: Option<usize>,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Read
    }

    fn description(&self) -> &'static str {
        "Find files by glob pattern."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern such as src/**/*.rs."
                },
                "root": {
                    "type": "string",
                    "description": "Optional search root. Defaults to the current directory."
                },
                "include_dirs": {
                    "type": "boolean",
                    "description": "Include directories in the result set."
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
        let args: GlobArgs = parse_args(self.name(), arguments)?;
        if args.pattern.trim().is_empty() {
            return Err(anyhow!("glob pattern must not be empty"));
        }

        let root = match args.root.as_deref() {
            Some(raw) => validate_path(self.name(), raw)?,
            None => PathBuf::from("."),
        };
        if !root.exists() {
            return Err(anyhow!("glob root does not exist: {}", root.display()));
        }

        let max_results = args.max_results.unwrap_or(DEFAULT_MAX_RESULTS);
        if max_results == 0 {
            return Err(anyhow!("glob max_results must be >= 1"));
        }

        let search_pattern = build_search_pattern(&root, &args.pattern);
        let options = ::glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: false,
            require_literal_leading_dot: false,
        };

        let mut matches = Vec::new();
        for entry in ::glob::glob_with(&search_pattern, options)
            .map_err(|err| anyhow!("glob pattern is invalid: {err}"))?
        {
            let path = entry.map_err(|err| anyhow!("glob failed while walking matches: {err}"))?;
            if args.include_dirs.unwrap_or(false) || path.is_file() {
                matches.push(format_match(&root, &path));
            }
        }

        matches.sort();
        matches.dedup();
        if matches.len() > max_results {
            matches.truncate(max_results);
        }

        if matches.is_empty() {
            Ok("No matches found.".to_string())
        } else {
            Ok(matches.join("\n"))
        }
    }
}

fn build_search_pattern(root: &Path, pattern: &str) -> String {
    let path = Path::new(pattern);
    if path.is_absolute() {
        pattern.to_string()
    } else {
        root.join(pattern).to_string_lossy().into_owned()
    }
}

fn format_match(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::GlobTool;
    use crate::cx_agent::tools::Tool;
    use crate::cx_agent::tools::test_support::TestDir;

    #[tokio::test(flavor = "current_thread")]
    async fn finds_matching_files() {
        let dir = TestDir::new("glob");
        dir.write("src/main.rs", "fn main() {}\n");
        dir.write("src/lib.rs", "pub fn lib() {}\n");
        dir.write("README.md", "ignored\n");
        let tool = GlobTool;

        let output = tool
            .invoke(json!({
                "pattern": "src/**/*.rs",
                "root": dir.path().display().to_string()
            }))
            .await
            .expect("glob output");

        assert!(output.contains("src/main.rs"));
        assert!(output.contains("src/lib.rs"));
        assert!(!output.contains("README.md"));
    }
}
