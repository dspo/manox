//! `read_file` tool: read a file, snapshot it for hashline, return
//! `[PATH#TAG]` + `N:TEXT` numbered rows. Supports path selectors for
//! line-range and raw output.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::read_policy::ReadPolicy;
use crate::tool::AgentTool;

use super::path_selector::{Selector, split_path_and_sel};
use super::{resolve_path, schema};

pub struct ReadTool {
    pub(crate) cwd: Arc<PathBuf>,
    pub(crate) read_policy: ReadPolicy,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReadFileInput {
    /// Absolute or relative file path to read (relative to cwd). Append
    /// `:<sel>` for line ranges or raw mode: `:50-200` (inclusive range),
    /// `:50+150` (150 lines from 50), `:5-16,960-973` (multiple ranges),
    /// `:raw` (verbatim, no anchors/line numbers), `:raw:1-50` (compound).
    path: String,
}

impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }
    fn description(&self) -> &str {
        "Read a file with optional line-range selectors. Output format: first line \
         `[<abs-path>#<TAG>]` (4-hex snapshot tag), followed by `N:TEXT` numbered rows \
         (1-indexed). Append `:<sel>` to the path for partial reads: `:50-200` (inclusive \
         range), `:50+150` (150 lines from 50), `:5-16,960-973` (multiple ranges), \
         `:raw` (verbatim, no anchors/line numbers), `:raw:1-50` (compound). \
         Without a selector the full file is returned."
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
        let parsed = match serde_json::from_value::<ReadFileInput>(input) {
            Ok(p) => p,
            Err(e) => {
                return cx.background_spawn(async move { Err(format!("input parse failed: {e}")) });
            }
        };
        let (path_str, selector) = split_path_and_sel(&parsed.path);
        let path = resolve_path(path_str, &self.cwd);
        let read_policy = self.read_policy.clone();
        cx.background_spawn(async move {
            // Enforce the read deny-list before touching the file: SSH keys,
            // cloud creds, `.env`, media libraries, etc. The error routes the
            // model toward the approval-gated `bash` escape hatch.
            read_policy.check(&path)?;
            let raw =
                std::fs::read_to_string(&path).map_err(|e| format!("read_file failed: {e}"))?;
            let text = crate::hashline::normalize_to_lf(&raw);
            let path_display = path.display().to_string();

            // Snapshot always fingerprints the full file — only display is sliced.
            let snap = crate::hashline::global()
                .lock()
                .expect("hashline store poisoned")
                .record(&path, &text);

            match selector {
                None => Ok(crate::hashline::format_numbered(
                    &path_display,
                    &text,
                    &snap.tag,
                )),
                Some(Selector::Lines(ref ranges)) => Ok(crate::hashline::format_numbered_range(
                    &path_display,
                    &text,
                    &snap.tag,
                    ranges,
                )),
                Some(Selector::Raw) => Ok(crate::hashline::format_raw(&text, None)),
                Some(Selector::RawLines(ref ranges)) => {
                    Ok(crate::hashline::format_raw(&text, Some(ranges)))
                }
            }
        })
    }
}
