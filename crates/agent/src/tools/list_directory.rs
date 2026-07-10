//! `list_directory` tool: list the direct children of a directory.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::read_policy::ReadPolicy;
use crate::tool::AgentTool;

use super::{resolve_path, schema};

pub struct ListDirectoryTool {
    pub(crate) cwd: Arc<PathBuf>,
    pub(crate) read_policy: ReadPolicy,
}

#[derive(Deserialize, JsonSchema)]
struct ListDirectoryInput {
    /// Directory path to list (defaults to cwd).
    #[serde(default)]
    path: Option<String>,
}

impl AgentTool for ListDirectoryTool {
    fn name(&self) -> &str {
        "list_directory"
    }
    fn description(&self) -> &str {
        "List the direct children (files and directories) of a directory."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<ListDirectoryInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<ListDirectoryInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let base = parsed
            .path
            .map(|p| resolve_path(&p, &self.cwd))
            .unwrap_or_else(|| self.cwd.as_ref().clone());
        let read_policy = self.read_policy.clone();
        cx.background_spawn(async move {
            // Deny listing sensitive subtrees (e.g. `~/.ssh`, `~/Library`) —
            // the read deny-list applies to directory enumeration too.
            read_policy.check(&base)?;
            let entries =
                std::fs::read_dir(&base).map_err(|e| format!("list_directory failed: {e}"))?;
            let mut lines: Vec<String> = Vec::new();
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let tag = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    "/"
                } else {
                    ""
                };
                lines.push(format!("{name}{tag}"));
            }
            lines.sort();
            Ok(lines.join("\n"))
        })
    }
}
