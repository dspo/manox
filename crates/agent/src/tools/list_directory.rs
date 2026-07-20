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

pub struct ListTool {
    pub(crate) cwd: Arc<PathBuf>,
    pub(crate) read_policy: ReadPolicy,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ListDirectoryInput {
    /// Directory path to list (defaults to cwd).
    #[serde(default)]
    path: Option<String>,
}

impl AgentTool for ListTool {
    fn name(&self) -> &str {
        "List"
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
        let parsed = match serde_json::from_value::<ListDirectoryInput>(input) {
            Ok(p) => p,
            Err(e) => {
                return cx.background_spawn(async move { Err(format!("input parse failed: {e}")) });
            }
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
                // Omit secret-named entries so even their filenames are not
                // surfaced to the model — contents are already blocked, but a
                // bare `id_rsa` / `.env` name in a listing is itself a leak.
                if crate::read_policy::is_likely_secret_file(&entry.path()) {
                    continue;
                }
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
