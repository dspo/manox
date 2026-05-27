use std::fs::{self, OpenOptions};
use std::io::Write;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::cx_agent::approval::ToolCategory;

use super::{Tool, display_path, parse_args, validate_path};

pub struct WriteFileTool;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteFileArgs {
    path: String,
    content: String,
    create_dirs: Option<bool>,
    append: Option<bool>,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    fn category(&self) -> ToolCategory {
        ToolCategory::Write
    }

    fn description(&self) -> &'static str {
        "Write a UTF-8 text file, optionally creating parent directories or appending."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to write."
                },
                "content": {
                    "type": "string",
                    "description": "Full text content to write."
                },
                "create_dirs": {
                    "type": "boolean",
                    "description": "Create missing parent directories before writing."
                },
                "append": {
                    "type": "boolean",
                    "description": "Append to the file instead of replacing it."
                }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    async fn invoke(&self, arguments: Value) -> Result<String> {
        let args: WriteFileArgs = parse_args(self.name(), arguments)?;
        let path = validate_path(self.name(), &args.path)?;
        let create_dirs = args.create_dirs.unwrap_or(false);
        let append = args.append.unwrap_or(false);

        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            if create_dirs {
                fs::create_dir_all(parent).map_err(|err| {
                    anyhow!(
                        "write_file failed to create parent directories for {}: {err}",
                        display_path(&path)
                    )
                })?;
            } else if !parent.exists() {
                return Err(anyhow!(
                    "write_file parent directory does not exist for {}",
                    display_path(&path)
                ));
            }
        }

        if append {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|err| {
                    anyhow!(
                        "write_file failed to open {} for append: {err}",
                        display_path(&path)
                    )
                })?;
            file.write_all(args.content.as_bytes()).map_err(|err| {
                anyhow!(
                    "write_file failed to append {} bytes to {}: {err}",
                    args.content.len(),
                    display_path(&path)
                )
            })?;
            Ok(format!(
                "Appended {} bytes to {}.",
                args.content.len(),
                display_path(&path)
            ))
        } else {
            fs::write(&path, args.content.as_bytes()).map_err(|err| {
                anyhow!(
                    "write_file failed to write {} bytes to {}: {err}",
                    args.content.len(),
                    display_path(&path)
                )
            })?;
            Ok(format!(
                "Wrote {} bytes to {}.",
                args.content.len(),
                display_path(&path)
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::WriteFileTool;
    use crate::cx_agent::tools::Tool;
    use crate::cx_agent::tools::test_support::TestDir;

    #[tokio::test(flavor = "current_thread")]
    async fn writes_and_appends_to_files() {
        let dir = TestDir::new("write-file");
        let path = dir.file_path("nested/output.txt");
        let tool = WriteFileTool;

        let first = tool
            .invoke(json!({
                "path": path.display().to_string(),
                "content": "alpha",
                "create_dirs": true
            }))
            .await
            .expect("write file");
        assert!(first.contains("Wrote 5 bytes"));

        let second = tool
            .invoke(json!({
                "path": path.display().to_string(),
                "content": "beta",
                "append": true
            }))
            .await
            .expect("append file");
        assert!(second.contains("Appended 4 bytes"));

        assert_eq!(dir.read("nested/output.txt"), "alphabeta");
    }
}
