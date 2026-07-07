//! `read_file` tool: read a file, snapshot it for hashline, return
//! `[PATH#TAG]` + `N:TEXT` numbered rows.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;

use super::{resolve_path, schema};

pub struct ReadFileTool {
    pub(crate) cwd: Arc<PathBuf>,
}

#[derive(Deserialize, JsonSchema)]
struct ReadFileInput {
    /// Absolute or relative file path to read (relative to cwd).
    path: String,
}

impl AgentTool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read the full contents of the specified file. Output format: first line `[<abs-path>#<TAG>]`, \
         e.g. `[/Users/me/proj/src/lib.rs#A557]` where TAG is a 4-hex snapshot tag; \
         followed by `N:TEXT` line-numbered rows (1-indexed)."
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<ReadFileInput>()
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
        let Ok(parsed) = serde_json::from_value::<ReadFileInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let path = resolve_path(&parsed.path, &self.cwd);
        cx.background_spawn(async move {
            let raw =
                std::fs::read_to_string(&path).map_err(|e| format!("read_file failed: {e}"))?;
            let text = crate::hashline::normalize_to_lf(&raw);
            let snap = crate::hashline::global()
                .lock()
                .expect("hashline store poisoned")
                .record(&path, &text);
            Ok(crate::hashline::format_numbered(
                &path.display().to_string(),
                &text,
                &snap.tag,
            ))
        })
    }
}
