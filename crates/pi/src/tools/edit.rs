// Edit tool — apply a hashline patch (line-anchored + TAG validation) to
// existing files, with 3-way merge recovery on a stale TAG.
//
// A patch holds one or more `[path#TAG]` sections of `SWAP`/`DEL`/`INS`/
// `CUT`/`PASTE` ops anchored on the ORIGINAL line numbers from `read`, plus
// optional file-level ops (`REM`/`MV`). The per-file mutation lock is held
// across read→patch→write so the TAG check and the write form a single
// critical section. On write, the file's original CRLF/BOM/trailing newline
// are restored so the edit is a minimal content delta.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

use crate::hashline;
use crate::hashline::parser::Op;
use crate::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use crate::tools::edit_diff;

pub struct EditTool;

/// The `patch` field description doubles as the hashline grammar reference
/// the model sees; keep it in sync with `hashline::parser`.
const PATCH_DOC: &str = "Hashline patch text. Each file section starts with a header \
`[<abs-path>#<tag>]` — paste the exact path and 6-hex tag returned by your latest `read` \
for that file; do NOT write the literal word `PATH`. Example: `[/Users/me/proj/CLAUDE.md#A55789]`. \
Operations: `SWAP N.=M:` replace lines N..=M (inclusive) with the `+TEXT` body rows; \
`DEL N.=M` delete lines N..=M (no body); `CUT N.=M` delete and capture to clipboard; \
`PASTE.PRE N:` / `PASTE.POST N:` / `PASTE.HEAD:` / `PASTE.TAIL:` paste clipboard content; \
`INS.PRE N:` / `INS.POST N:` / `INS.HEAD:` / `INS.TAIL:` insert body rows; \
`SWAP.BLK N:` / `DEL.BLK N` / `CUT.BLK N` / `INS.BLK.POST N:` operate on the \
bracket-block beginning at line N; `REM` deletes the file; `MV DEST` renames it. \
Body rows are `+TEXT` (`+` alone = blank line; \
`+-x`/`++x` escapes a literal leading `-`/`+`); a `-`-prefixed markdown list item is NOT a \
body row — rewrite it with a `+` prefix. Line numbers reference the ORIGINAL file from read \
and do not shift across hunks. Ranges cover only changed lines; pure additions use `INS`, \
never a widened `SWAP`. On a stale-TAG rejection, re-`read` before retrying.\n\
Format gotchas (common miswrites): the range separator is `.=` not `:` — write `SWAP 37.=48:` \
not `SWAP 37:=48:`. The body starts on the NEXT line as `+`-prefixed rows, never on the same \
line as the directive. Complete example:\n\
```text\n\
[/Users/me/proj/main.py#A55789]\n\
SWAP 37.=48:\n\
+    if args.command == \"add\":\n\
+        handler.add(args.title)\n\
+    else:\n\
+        parser.print_help()\n\
```";

#[async_trait::async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Edit existing files via a hashline patch (line-anchored + TAG validation). \
         See the patch field docs for the grammar."
    }

    fn is_read_only(&self) -> bool {
        false
    }

    fn parameters_schema(&self) -> JsonValue {
        serde_json::json!({
            "type": "object",
            "properties": {
                "patch": {
                    "type": "string",
                    "description": PATCH_DOC
                }
            },
            "required": ["patch"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: JsonValue,
        _signal: CancellationToken,
        ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let patch = params["patch"]
            .as_str()
            .ok_or_else(|| ToolError::InvalidArguments("patch is required".into()))?;

        let patches =
            hashline::parse_patch(patch).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        // Clear the clipboard at the start of each edit call — the anonymous
        // register is batch-local, matching oh-my-pi's startClipboardBatch.
        ctx.tool_state()
            .clipboard
            .lock()
            .expect("hashline clipboard poisoned")
            .clear();

        let mut results: Vec<String> = Vec::new();
        for fp in patches {
            let path = resolve_path(ctx, &fp.path);
            let path_display = path.display().to_string();

            // Hold the mutation lock across all operations so concurrent
            // edits to the same file are serialized.
            let _guard = ctx.tool_state().mutation_queue.lock(&path).await;

            // File-level operations (REM/MV) still validate the tag — the
            // model must have a current view of the file it's deleting or
            // moving.
            if let Some(file_op) = &fp.file_op {
                let raw = ctx.env().read_file(&path, None, None).await.map_err(|e| {
                    ToolError::ExecutionFailed(format!("edit read failed {path_display}: {e}"))
                })?;
                let current = hashline::normalize_to_lf(&raw);
                let current_tag = hashline::compute_tag(&current);
                if current_tag != fp.tag {
                    return Err(ToolError::ExecutionFailed(format!(
                        "edit {path_display}: file changed between read and edit (tag mismatch)"
                    )));
                }
                match file_op {
                    hashline::FileOp::Rem => {
                        ctx.env().remove(&path).await.map_err(|e| {
                            ToolError::ExecutionFailed(format!(
                                "edit REM failed {path_display}: {e}"
                            ))
                        })?;
                        ctx.tool_state()
                            .snapshots
                            .lock()
                            .expect("hashline snapshot store poisoned")
                            .invalidate(&path);
                        results.push(format!("[{path_display}] deleted"));
                        continue;
                    }
                    hashline::FileOp::Move { dest } => {
                        let dest_path = resolve_path(ctx, std::path::Path::new(dest));
                        ctx.env()
                            .write_file(&dest_path, &raw)
                            .await
                            .map_err(|e| {
                                ToolError::ExecutionFailed(format!(
                                    "edit MV write failed {path_display}: {e}"
                                ))
                            })?;
                        ctx.env().remove(&path).await.map_err(|e| {
                            ToolError::ExecutionFailed(format!(
                                "edit MV source removal failed {path_display}: {e}"
                            ))
                        })?;
                        ctx.tool_state()
                            .snapshots
                            .lock()
                            .expect("hashline snapshot store poisoned")
                            .relocate(&path, &dest_path);
                        results.push(format!(
                            "[{path_display}] moved to {}",
                            dest_path.display()
                        ));
                        continue;
                    }
                }
            }

            let raw = ctx.env().read_file(&path, None, None).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("edit read failed {path_display}: {e}"))
            })?;
            let had_bom = hashline::has_bom(&raw);
            let is_crlf = hashline::detect_crlf(&raw);
            let had_trailing_nl = raw.ends_with('\n');
            let current = hashline::normalize_to_lf(&raw);
            let current_tag = hashline::compute_tag(&current);

            // Expand Cut/Paste ops before apply: Cut captures lines to clipboard,
            // Paste expands to Ins with clipboard content.
            let expanded_ops = expand_clipboard_ops(&fp.ops, &current, ctx)?;

            let new_text = if current_tag == fp.tag {
                // Seen-line gate — only on the no-drift path, where anchor line
                // numbers index the tagged content 1:1. On recovery the numbers
                // shift, so provenance does not apply.
                hashline::assert_seen_lines(
                    &mut ctx
                        .tool_state()
                        .snapshots
                        .lock()
                        .expect("hashline snapshot store poisoned"),
                    &path,
                    &fp.tag,
                    &expanded_ops,
                    &current,
                )
                .map_err(ToolError::ExecutionFailed)?;

                hashline::apply(&current, &expanded_ops)
                    .map_err(|e| {
                        ToolError::ExecutionFailed(format!("edit apply failed {path_display}: {e}"))
                    })?
                    .text
            } else {
                let store = ctx
                    .tool_state()
                    .snapshots
                    .lock()
                    .expect("hashline snapshot store poisoned");
                hashline::try_recover(&current, &fp.tag, &expanded_ops, &store, &path)
                    .map_err(|e| ToolError::ExecutionFailed(format!("edit {path_display}: {e}")))?
            };

            // Restore original line endings, trailing newline, and BOM so
            // the write is a minimal content delta, not a full-rewrite that
            // flattens formatting or drops the file's terminating newline.
            let persisted = persist(&new_text, is_crlf, had_bom, had_trailing_nl);
            ctx.env().write_file(&path, &persisted).await.map_err(|e| {
                ToolError::ExecutionFailed(format!("edit write failed {path_display}: {e}"))
            })?;

            // Record snapshot of LF-normalized text, consistent with Read tool.
            // The model authored the whole file through this patch (or the diff
            // it produced), so every line counts as seen — a follow-up edit on
            // the returned tag must not trip the gate.
            let snap_text = hashline::normalize_to_lf(&new_text);
            let new_snap = {
                let mut store = ctx
                    .tool_state()
                    .snapshots
                    .lock()
                    .expect("hashline snapshot store poisoned");
                let snap = store.record(&path, &snap_text);
                let all_lines: HashSet<usize> = (1..=snap_text.lines().count()).collect();
                store.record_seen_lines(&path, &snap.tag, &all_lines);
                snap
            };
            let diff = edit_diff::compute_unified_diff(&current, &new_text, &path);
            let diff = if edit_diff::is_diff_empty(&diff) {
                "(no changes)".to_string()
            } else {
                diff
            };
            results.push(format!("[{path_display}#{}]\n{diff}", new_snap.tag));
        }

        Ok(AgentToolResult::text(results.join("\n---\n")))
    }
}

/// Resolve a patch section path against the tool cwd when it is relative.
fn resolve_path(ctx: &dyn ToolContext, path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.cwd().join(path)
    }
}

/// Expand Cut/Paste ops before apply. Cut captures the deleted lines to the
/// clipboard and converts to Del; Paste expands to Ins with clipboard content.
/// CutBlk resolves the block range and captures those lines before converting
/// to DelBlk.
fn expand_clipboard_ops(
    ops: &[Op],
    current: &str,
    ctx: &dyn ToolContext,
) -> Result<Vec<Op>, ToolError> {
    let lines: Vec<&str> = current.lines().collect();
    let line_refs: Vec<&str> = lines.to_vec();
    let mut expanded: Vec<Op> = Vec::new();
    let mut clipboard = ctx
        .tool_state()
        .clipboard
        .lock()
        .expect("hashline clipboard poisoned");

    for op in ops {
        match op {
            Op::Cut { start, end } => {
                let s = start.saturating_sub(1);
                let e = (*end).min(lines.len());
                if s < e {
                    let captured: Vec<String> =
                        lines[s..e].iter().map(|s| s.to_string()).collect();
                    clipboard.clear();
                    clipboard.extend(captured);
                }
                expanded.push(Op::Del {
                    start: *start,
                    end: *end,
                });
            }
            Op::CutBlk { start } => {
                let (s, e) = crate::hashline::block::resolve_block_range(&line_refs, *start)
                    .map_err(|err| {
                        ToolError::ExecutionFailed(format!("CUT.BLK resolve failed: {err}"))
                    })?;
                let captured: Vec<String> = lines[s - 1..e]
                    .iter()
                    .map(|s| s.to_string())
                    .collect();
                clipboard.clear();
                clipboard.extend(captured);
                expanded.push(Op::DelBlk { start: *start });
            }
            Op::Paste { pos, anchor } => {
                let body = clipboard.clone();
                if body.is_empty() {
                    return Err(ToolError::ExecutionFailed(
                        "PASTE failed: clipboard is empty (no preceding CUT in this patch)"
                            .to_string(),
                    ));
                }
                expanded.push(Op::Ins {
                    pos: *pos,
                    anchor: *anchor,
                    body,
                });
            }
            other => expanded.push(other.clone()),
        }
    }
    Ok(expanded)
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
        assert_eq!(persist("a\nb", false, false, true), "a\nb\n");
        assert_eq!(persist("a\nb", true, false, true), "a\r\nb\r\n");
        assert_eq!(persist("a\nb", false, false, false), "a\nb");
        assert_eq!(persist("", false, false, true), "");
        assert_eq!(persist("x", false, true, false), "\u{feff}x");
    }
}