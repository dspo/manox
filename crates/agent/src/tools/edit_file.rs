//! `edit_file` tool: apply a hashline patch (line-anchored + TAG validation)
//! to an existing file, with 3-way merge recovery on a stale TAG.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::sandbox::SandboxPolicy;
use crate::tool::AgentTool;

use super::{resolve_path_for_write, schema};

pub struct EditFileTool {
    pub(crate) cwd: Arc<PathBuf>,
    pub(crate) sandbox: SandboxPolicy,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EditFileInput {
    /// Hashline patch text. Each file section starts with a header
    /// `[<abs-path>#<tag>]` — paste the exact absolute path and 4-hex tag
    /// returned by your latest `read_file` for that file; do NOT write the
    /// literal word `PATH`. Example: `[/Users/me/proj/CLAUDE.md#A557]`.
    /// Operations: `SWAP N.=M:` replace lines N..=M (inclusive) with the
    /// `+TEXT` body rows; `DEL N.=M` delete lines N..=M (no body);
    /// `INS.PRE N:` / `INS.POST N:` / `INS.HEAD:` / `INS.TAIL:` insert body
    /// rows; `SWAP.BLK N:` / `DEL.BLK N` / `INS.BLK.POST N:` operate on the
    /// bracket-block beginning at line N. Body rows are `+TEXT` (`+` alone =
    /// blank line; `+-x`/`++x` escapes a literal leading `-`/`+`); a `-`-prefixed
    /// markdown list item is NOT a body row — rewrite it with a `+` prefix.
    /// Line numbers reference the ORIGINAL file from read_file and do not shift
    /// across hunks. Ranges cover only changed lines; pure additions use
    /// `INS`, never a widened `SWAP`. On a stale-TAG rejection, re-`read_file`
    /// before retrying.
    ///
    /// Format gotchas (common miswrites): the range separator is `.=` not `:`
    /// — write `SWAP 37.=48:` not `SWAP 37:=48:`. The body starts on the NEXT
    /// line as `+`-prefixed rows, never on the same line as the directive.
    /// Complete example:
    #[doc = r""]
    #[doc = r"```text"]
    #[doc = r"[/Users/me/proj/main.py#A557]"]
    #[doc = r"SWAP 37.=48:"]
    #[doc = r#"+    if args.command == "add":"#]
    #[doc = r"+        handler.add(args.title)"]
    #[doc = r"+    else:"]
    #[doc = r"+        parser.print_help()"]
    #[doc = r"```"]
    patch: String,
}

impl AgentTool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Edit an existing file via a hashline patch (line-anchored + TAG validation). See the input.patch field docs."
    }
    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<EditFileInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let parsed = match serde_json::from_value::<EditFileInput>(input) {
            Ok(p) => p,
            Err(e) => {
                return cx.background_spawn(async move { Err(format!("input parse failed: {e}")) });
            }
        };
        let cwd = self.cwd.clone();
        let sandbox = self.sandbox.clone();
        let owner = ctx.agent_label().to_string();
        cx.background_spawn(async move {
            let patches = crate::hashline::parse_patch(&parsed.patch).map_err(|e| e.to_string())?;
            let mut results: Vec<String> = Vec::new();
            for fp in patches {
                let path = resolve_path_for_write(&fp.path, &cwd, &sandbox)?;
                let path_display = path.display().to_string();
                // Hold the write lock across read+patch+write so the TAG check
                // and the write are a single critical section — a concurrent
                // writer between read and write would stale the TAG and clobber.
                let _lock = match crate::tools::file_lock::try_acquire(&path, &owner) {
                    Ok(g) => g,
                    Err(held) => {
                        return Err(format!(
                            "edit_file blocked: {} is being written by {}; re-read and retry",
                            path_display, held.owner
                        ));
                    }
                };
                let raw = std::fs::read_to_string(&path)
                    .map_err(|e| format!("edit_file read failed {path_display}: {e}"))?;
                let had_bom = crate::hashline::has_bom(&raw);
                let is_crlf = crate::hashline::detect_crlf(&raw);
                let had_trailing_nl = raw.ends_with('\n');
                let current = crate::hashline::normalize_to_lf(&raw);
                let current_tag = crate::hashline::compute_tag(&current);

                let new_text = if current_tag == fp.tag {
                    crate::hashline::apply(&current, &fp.ops)
                        .map_err(|e| format!("edit_file apply failed {path_display}: {e}"))?
                        .text
                } else {
                    let store = crate::hashline::global()
                        .lock()
                        .expect("hashline store poisoned");
                    crate::hashline::try_recover(&current, &fp.tag, &fp.ops, &store, &path)
                        .map_err(|e| format!("edit_file {path_display}: {e}"))?
                };

                // Restore original line endings, trailing newline, and BOM so
                // the write is a minimal content delta, not a full-rewrite that
                // flattens formatting or drops the file's terminating newline.
                let persisted = persist(&new_text, is_crlf, had_bom, had_trailing_nl);
                std::fs::write(&path, persisted.as_bytes())
                    .map_err(|e| format!("edit_file write failed {path_display}: {e}"))?;

                let new_snap = crate::hashline::global()
                    .lock()
                    .expect("hashline store poisoned")
                    .record(&path, &new_text);
                let diff = unified_diff(&current, &new_text);
                results.push(format!("[{}#{}]\n{}", path_display, new_snap.tag, diff));
            }
            Ok(results.join("\n---\n"))
        })
    }
}

/// Render a minimal unified diff for the edit result preview.
fn unified_diff(old: &str, new: &str) -> String {
    use similar::TextDiff;
    let diff = TextDiff::from_lines(old, new);
    let rendered = diff.unified_diff().to_string();
    if rendered.is_empty() {
        "(no changes)".to_string()
    } else {
        rendered
    }
}

/// Restore the file's original line-ending style, trailing newline, and optional
/// BOM on write. `apply`/`recover` model files as content lines without
/// terminators, so the trailing newline the source file carried is restored
/// here rather than dropped.
fn persist(text: &str, crlf: bool, bom: bool, trailing_nl: bool) -> String {
    let mut out = String::with_capacity(text.len() + 3);
    if bom {
        out.push('\u{feff}');
    }
    if crlf {
        let mut iter = text.split('\n').peekable();
        while let Some(line) = iter.next() {
            out.push_str(line);
            if iter.peek().is_some() {
                out.push_str("\r\n");
            }
        }
    } else {
        out.push_str(text);
    }
    // Re-attach a trailing terminator if the original file had one and the
    // edited content is non-empty (an emptied file stays empty).
    if trailing_nl && !text.is_empty() {
        if crlf {
            out.push_str("\r\n");
        } else {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::persist;

    #[test]
    fn persist_restores_trailing_newline_and_crlf() {
        // apply yields content without a terminator; persist re-attaches the
        // file's original trailing newline (LF or CRLF) and BOM.
        assert_eq!(persist("a\nb", false, false, true), "a\nb\n");
        assert_eq!(persist("a\nb", true, false, true), "a\r\nb\r\n");
        // No trailing newline originally → none added.
        assert_eq!(persist("a\nb", false, false, false), "a\nb");
        // Emptied content stays empty even if the original had a newline.
        assert_eq!(persist("", false, false, true), "");
        // BOM is re-prepended.
        assert_eq!(persist("x", false, true, false), "\u{feff}x");
    }
}
