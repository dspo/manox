//! `write_file` tool: create or overwrite a file, sandbox-confined.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::sandbox::SandboxPolicy;
use crate::tool::AgentTool;

use super::{resolve_path_for_write, schema};

pub struct WriteFileTool {
    pub(crate) cwd: Arc<PathBuf>,
    pub(crate) sandbox: SandboxPolicy,
}

#[derive(Deserialize, JsonSchema)]
struct WriteFileInput {
    /// File path to write.
    path: String,
    /// Full content to write.
    content: String,
}

impl AgentTool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
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
        let Ok(parsed) = serde_json::from_value::<WriteFileInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let path = match resolve_path_for_write(&parsed.path, &self.cwd, &self.sandbox) {
            Ok(p) => p,
            Err(e) => return cx.background_spawn(async move { Err(e) }),
        };
        let owner = ctx.agent_label().to_string();
        let content_len = parsed.content.len();
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
            std::fs::write(&path, &parsed.content)
                .map(|_| format!("Wrote {} ({content_len} bytes)", path.display()))
                .map_err(|e| format!("write_file failed: {e}"))
        })
    }
}
