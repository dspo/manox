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

/// Lines returned by an unqualified `read_file` (no `:start-end` selector).
const MAX_READ_LINES: usize = 2000;

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
         Without a selector the first 2000 lines are returned; use a range selector \
         to page through longer files."
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
                None => Ok(format_full_read(
                    &path_display,
                    &text,
                    &snap.tag,
                    &parsed.path,
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

/// Format an unqualified (selector-less) read. The output caps at
/// [`MAX_READ_LINES`] lines — a full-file dump of a 100k-line file would
/// flood the context; the selector syntax pages through the rest.
fn format_full_read(path_display: &str, text: &str, tag: &str, path_arg: &str) -> String {
    let line_count = text.lines().count();
    if line_count <= MAX_READ_LINES {
        return crate::hashline::format_numbered(path_display, text, tag);
    }
    let ranges = [crate::tools::path_selector::LineRange {
        start: 1,
        end: Some(MAX_READ_LINES),
    }];
    let mut out = crate::hashline::format_numbered_range(path_display, text, tag, &ranges);
    out.push_str(&format!(
        "\n[Showing lines 1-{MAX_READ_LINES} of {line_count}. \
         Use a line-range selector for more, e.g. `{path_arg}:{}-{}`]",
        MAX_READ_LINES + 1,
        MAX_READ_LINES * 2,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_file_is_not_capped() {
        let text = "a\nb\nc";
        let out = format_full_read("/tmp/f.txt", text, "AB12", "f.txt");
        assert!(out.contains("3:c"));
        assert!(!out.contains("Showing lines"));
    }

    #[test]
    fn large_file_caps_at_max_lines_with_selector_hint() {
        let text: String = (1..=5000).map(|i| format!("line {i}\n")).collect();
        let out = format_full_read("/tmp/big.txt", &text, "AB12", "big.txt");
        assert!(out.contains("1:line 1"));
        assert!(out.contains("2000:line 2000"));
        // format_numbered_range appends 3 trailing context lines; nothing
        // beyond those may appear.
        assert!(!out.contains("2004:line 2004"));
        assert!(out.contains("Showing lines 1-2000 of 5000"));
        assert!(out.contains("big.txt:2001-4000"), "selector hint: {out}");
    }
}
