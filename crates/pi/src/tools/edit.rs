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

#[derive(Default)]
pub struct EditTool {
    /// Reject no-drift edits whose anchor lines a prior `read`/`grep` never
    /// displayed. Off by default, matching upstream oh-my-pi's shipped
    /// `edit.enforceSeenLines: false`: the guard historically caused frequent
    /// edit rejections once truncation or summaries hid part of a file the
    /// model believed it had fully read. Hosts that want the anti-blind-edit
    /// discipline opt in explicitly.
    pub enforce_seen_lines: bool,
}

impl EditTool {
    /// Opt the host into the seen-line guard (see [`EditTool::enforce_seen_lines`]).
    pub fn with_enforce_seen_lines(mut self, enforce: bool) -> Self {
        self.enforce_seen_lines = enforce;
        self
    }
}

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
`+-x`/`++x` escapes a literal leading `-`/`+`). A `- `-prefixed Markdown bullet inside a \
body is accepted as literal content, but prefer the explicit `+- item`. A bare `-old` row \
is dropped when `+` rows are present — the range already deletes the old lines, the body is \
only the NEW content. Line numbers reference the ORIGINAL file from read and do not shift \
across hunks. Ranges cover only changed lines; pure additions use `INS`, never a widened \
`SWAP`. On a stale-TAG rejection, re-`read` before retrying.\n\
Format gotchas (common miswrites): the range separator is `.=` not `:` — write `SWAP 37.=48:` \
not `SWAP 37:=48:`. The body starts on the NEXT line as `+`-prefixed rows, never on the same \
line as the directive.\n\
Anti-patterns:\n\
- WRONG: empty `SWAP` to delete. RIGHT: `DEL N.=M`.\n\
- WRONG: range sized to the post-edit content. RIGHT: range covers only the ORIGINAL touched \
  lines; body length is irrelevant.\n\
- WRONG: pure insertion as a widened `SWAP` (retypes keepers, drops lines). RIGHT: `INS.POST N:` \
  touches nothing you keep.\n\
- WRONG: pasting `read` output rows (`N:TEXT`) as the body. RIGHT: plain `+TEXT` rows.\n\
- WRONG: `+`-less body rows / `-old` diff rows. RIGHT: every body row starts with `+`.\n\
Critical:\n\
1. RE-GROUND AFTER EVERY EDIT: each edit renumbers lines and changes `#TAG`. Take the next \
   line numbers and tag from the edit's own `[path#newtag]` response, or re-`read`. Never \
   reuse a tag from before that edit.\n\
2. RANGES TIGHT: changed lines only. Whole construct: `SWAP.BLK N:`.\n\
3. BODY FINAL CONTENT: every row starts `+`.\n\
Complete example:\n\
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

        let parsed =
            hashline::parse_patch(patch).map_err(|e| ToolError::ExecutionFailed(e.to_string()))?;

        // Clear the clipboard at the start of each edit call — the anonymous
        // register is batch-local, matching oh-my-pi's startClipboardBatch.
        ctx.tool_state()
            .clipboard
            .lock()
            .expect("hashline clipboard poisoned")
            .clear();

        // All-or-nothing execution: every section is fully prepared (read,
        // tag check, gate, apply/recover, byte transforms) BEFORE any byte is
        // written, so a later section's rejection cannot leave earlier ones
        // half-applied on disk. Per-file mutation locks are all taken up
        // front, in canonical path order, so two concurrent multi-file edits
        // can never deadlock on overlapping lock sets.
        let mut lock_paths: Vec<PathBuf> = parsed
            .files
            .iter()
            .map(|fp| resolve_path(ctx, &fp.path))
            .collect();
        lock_paths.sort_by_key(|p| hashline::canonical_path(p));
        lock_paths.dedup_by(|a, b| hashline::canonical_path(a) == hashline::canonical_path(b));
        let mut guards = Vec::with_capacity(lock_paths.len());
        for path in &lock_paths {
            guards.push(ctx.tool_state().mutation_queue.lock(path).await);
        }

        /// One prepared, not-yet-written file effect.
        enum Pending {
            Write {
                path: PathBuf,
                path_display: String,
                persisted: String,
                snap_text: String,
                current: String,
                old_tag: String,
                recovered: bool,
                payload_hash: u64,
            },
            Remove {
                path: PathBuf,
                path_display: String,
            },
            Move {
                from: PathBuf,
                to: PathBuf,
                persisted: String,
                path_display: String,
            },
        }

        let mut prepared: Vec<Pending> = Vec::with_capacity(parsed.files.len());
        for fp in parsed.files {
            let path = resolve_path(ctx, &fp.path);
            let path_display = path.display().to_string();

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
                    // Distinguish "file drifted" from "tag never minted this session".
                    let known = ctx
                        .tool_state()
                        .snapshots
                        .lock()
                        .expect("hashline snapshot store poisoned")
                        .get(&path, &fp.tag)
                        .is_some();
                    let reason = if known {
                        format!(
                            "file changed between read and edit (tag #{old} now #{new}). If a \
                             prior edit this session changed it, copy that edit's [path#newtag] \
                             header; otherwise re-read.",
                            old = fp.tag,
                            new = current_tag
                        )
                    } else {
                        format!(
                            "tag #{old} is not from this session (fabricated or carried over \
                             from a prior session / app restart). The current file hashes to \
                             #{new}; re-read to copy a fresh [path#tag] header — never invent a tag.",
                            old = fp.tag,
                            new = current_tag
                        )
                    };
                    return Err(ToolError::ExecutionFailed(format!(
                        "edit {path_display}: {reason}"
                    )));
                }
                match file_op {
                    hashline::FileOp::Rem => {
                        prepared.push(Pending::Remove { path, path_display });
                        continue;
                    }
                    hashline::FileOp::Move { dest } => {
                        let dest_path = resolve_path(ctx, std::path::Path::new(dest));
                        if hashline::canonical_path(&dest_path) == hashline::canonical_path(&path) {
                            return Err(ToolError::ExecutionFailed(format!(
                                "edit MV {path_display}: destination is the source file itself"
                            )));
                        }
                        // The section's line ops ride the move: apply them to
                        // the current content first (a bare header + `MV` moves
                        // the file unchanged), then persist the result at the
                        // destination with the source's line-ending/BOM shape.
                        let expanded = expand_clipboard_ops(&fp.ops, &current, ctx)?;
                        let effective = if fp.ops.is_empty() {
                            current.clone()
                        } else {
                            hashline::apply(&current, &expanded)
                                .map_err(|e| {
                                    ToolError::ExecutionFailed(format!(
                                        "edit MV apply failed {path_display}: {e}"
                                    ))
                                })?
                                .text
                        };
                        let persisted = persist(
                            &effective,
                            hashline::detect_crlf(&raw),
                            hashline::has_bom(&raw),
                            raw.ends_with('\n'),
                        );
                        prepared.push(Pending::Move {
                            path_display,
                            from: path,
                            to: dest_path,
                            persisted,
                        });
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

            // Position-free ops (pure INS.HEAD/INS.TAIL) land identically no
            // matter how the file drifted around them: applying to live
            // content directly beats failing an edit whose anchors carry no
            // line numbers to go stale. `expand_clipboard_ops` above has
            // already rewritten every PASTE (HEAD/TAIL included, which parse
            // anchor-less) into its INS equivalent, so the Paste arm below is
            // defensive — it holds only if a future change lets Paste ops
            // survive expansion.
            let position_free = expanded_ops.iter().all(|op| {
                matches!(
                    op,
                    Op::Ins { anchor: None, .. } | Op::Paste { anchor: None, .. }
                )
            });

            let new_text = if current_tag == fp.tag {
                // Seen-line gate — only on the no-drift path, where anchor line
                // numbers index the tagged content 1:1. On recovery the numbers
                // shift, so provenance does not apply. Host-opt-in: upstream
                // ships it off because blind anchors on a drifted view are
                // caught by the tag check, and the gate's rejections cost a
                // full model round-trip each.
                if self.enforce_seen_lines {
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
                }

                hashline::apply(&current, &expanded_ops)
                    .map_err(|e| {
                        ToolError::ExecutionFailed(format!("edit apply failed {path_display}: {e}"))
                    })?
                    .text
            } else if position_free {
                hashline::apply(&current, &expanded_ops)
                    .map_err(|e| {
                        ToolError::ExecutionFailed(format!("edit apply failed {path_display}: {e}"))
                    })?
                    .text
            } else {
                let mut store = ctx
                    .tool_state()
                    .snapshots
                    .lock()
                    .expect("hashline snapshot store poisoned");
                hashline::try_recover(&current, &fp.tag, &expanded_ops, &mut store, &path)
                    .map_err(|e| ToolError::ExecutionFailed(format!("edit {path_display}: {e}")))?
            };
            let recovered = current_tag != fp.tag;

            // Restore original line endings, trailing newline, and BOM so
            // the write is a minimal content delta, not a full-rewrite that
            // flattens formatting or drops the file's terminating newline.
            let persisted = persist(&new_text, is_crlf, had_bom, had_trailing_nl);
            let snap_text = hashline::normalize_to_lf(&new_text);
            prepared.push(Pending::Write {
                path,
                path_display,
                persisted,
                snap_text,
                current,
                old_tag: fp.tag,
                recovered,
                payload_hash: hash_ops(&fp.ops),
            });
        }

        // Commit: every section validated — write them all. A write-phase I/O
        // failure still names which sections landed and which did not, so the
        // model never re-guesses the on-disk state.
        let mut results: Vec<String> = Vec::with_capacity(prepared.len());
        let mut failure_manifest: Vec<String> = Vec::new();
        for pending in prepared {
            match pending {
                Pending::Remove { path, path_display } => {
                    ctx.env().remove(&path).await.map_err(|e| {
                        ToolError::ExecutionFailed(format!(
                            "edit REM failed {path_display}: {e}{}",
                            commit_manifest(&failure_manifest)
                        ))
                    })?;
                    ctx.tool_state()
                        .snapshots
                        .lock()
                        .expect("hashline snapshot store poisoned")
                        .invalidate(&path);
                    failure_manifest.push(format!("{path_display} deleted"));
                    results.push(format!("[{path_display}] deleted"));
                }
                Pending::Move {
                    from,
                    to,
                    persisted,
                    path_display,
                } => {
                    ctx.env().write_file(&to, &persisted).await.map_err(|e| {
                        ToolError::ExecutionFailed(format!(
                            "edit MV write failed {path_display}: {e}{}",
                            commit_manifest(&failure_manifest)
                        ))
                    })?;
                    ctx.env().remove(&from).await.map_err(|e| {
                        ToolError::ExecutionFailed(format!(
                            "edit MV source removal failed {path_display}: {e}{}",
                            commit_manifest(&failure_manifest)
                        ))
                    })?;
                    ctx.tool_state()
                        .snapshots
                        .lock()
                        .expect("hashline snapshot store poisoned")
                        .relocate(&from, &to);
                    failure_manifest.push(format!("{path_display} moved"));
                    results.push(format!("[{path_display}] moved to {}", to.display()));
                }
                Pending::Write {
                    path,
                    path_display,
                    persisted,
                    snap_text,
                    current,
                    old_tag,
                    recovered,
                    payload_hash,
                } => {
                    ctx.env().write_file(&path, &persisted).await.map_err(|e| {
                        ToolError::ExecutionFailed(format!(
                            "edit write failed {path_display}: {e}{}",
                            commit_manifest(&failure_manifest)
                        ))
                    })?;
                    failure_manifest.push(path_display.clone());

                    // Record snapshot of LF-normalized text, consistent with
                    // Read tool. The model authored the whole file through this
                    // patch (or the diff it produced), so every line counts as
                    // seen — a follow-up edit on the returned tag must not trip
                    // the gate. Files over the snapshot cap mint no tag.
                    let new_snap = if snap_text.len() > hashline::SNAPSHOT_MAX_BYTES {
                        None
                    } else {
                        let mut store = ctx
                            .tool_state()
                            .snapshots
                            .lock()
                            .expect("hashline snapshot store poisoned");
                        let snap = store.record(&path, &snap_text);
                        let all_lines: HashSet<usize> = (1..=snap_text.lines().count()).collect();
                        store.record_seen_lines(&path, &snap.tag, &all_lines);
                        Some(snap)
                    };
                    let diff = edit_diff::compute_unified_diff(&current, &snap_text, &path);
                    let changed = !edit_diff::is_diff_empty(&diff);

                    // No-op loop guard: the same payload producing no change
                    // repeatedly means the model is spinning; escalate after a
                    // few identical no-ops. Keyed by canonical path so `/tmp`
                    // vs `/private/tmp` spellings of one file cannot dodge it.
                    let noop_key = hashline::canonical_path(&path);
                    if changed {
                        ctx.tool_state()
                            .noop_edits
                            .lock()
                            .expect("noop guard poisoned")
                            .remove(&noop_key);
                        let fresh_header = new_snap
                            .as_ref()
                            .map(|s| format!("[{path_display}#{}]", s.tag))
                            .unwrap_or_else(|| "a fresh Read".to_string());
                        let mut entry = match new_snap {
                            Some(s) => format!("[{path_display}#{}]\n{diff}", s.tag),
                            None => {
                                format!("[{path_display}]\n{diff}\n(no snapshot: file exceeds 4MB)")
                            }
                        };
                        if recovered {
                            // The edit landed through drift recovery: every
                            // hashline header still in the model's context for
                            // this file is now stale, and the next edit citing
                            // one re-enters the slow (or failing) recovery
                            // path. Say so while the new header is right here.
                            entry.push_str(&format!(
                                "\n[drift recovery: the file changed since tag #{old_tag} — \
                                 older headers for this file are stale; anchor the next edit on \
                                 {fresh_header}]",
                            ));
                        }
                        results.push(entry);
                    } else {
                        let mut guard = ctx
                            .tool_state()
                            .noop_edits
                            .lock()
                            .expect("noop guard poisoned");
                        let entry = guard.entry(noop_key).or_insert((payload_hash, 0));
                        if entry.0 == payload_hash {
                            entry.1 += 1;
                        } else {
                            entry.0 = payload_hash;
                            entry.1 = 1;
                        }
                        let count = entry.1;
                        drop(guard);
                        if count >= NOOP_HARD_LIMIT {
                            return Err(ToolError::ExecutionFailed(format!(
                                "edit {path_display}: this patch made no changes {count} times in \
                                 a row with an identical payload — the file already matches your \
                                 intent. Stop re-issuing it; re-read the file if you expected a \
                                 difference."
                            )));
                        }
                        let hint = "(no changes — the targeted lines already match the body; \
                                    re-read before another edit)";
                        results.push(match new_snap {
                            Some(s) => format!("[{path_display}#{}]\n{hint}", s.tag),
                            None => {
                                format!("[{path_display}]\n{hint}\n(no snapshot: file exceeds 4MB)")
                            }
                        });
                    }
                }
            }
        }

        let mut out = results.join("\n---\n");
        if !parsed.warnings.is_empty() {
            out.push_str("\n\nWarnings:\n");
            out.push_str(&parsed.warnings.join("\n"));
        }
        Ok(AgentToolResult::text(out))
    }
}

/// The tail of a commit-phase failure message: which sections already landed
/// on disk, so the model knows exactly what remains to re-issue. Empty when
/// nothing was written yet (the common, fully-atomic case).
fn commit_manifest(written: &[String]) -> String {
    if written.is_empty() {
        return String::new();
    }
    format!(
        " — sections already applied: {}; the remaining sections were NOT applied, re-issue them",
        written.join(", ")
    )
}

/// Resolve a patch section path against the tool cwd when it is relative.
fn resolve_path(ctx: &dyn ToolContext, path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.cwd().join(path)
    }
}

/// Consecutive identical no-op edits on one path before the guard escalates.
const NOOP_HARD_LIMIT: u32 = 3;

/// Fingerprint an edit's ops so the noop guard can tell a repeated payload
/// from progress. Debug rendering is stable enough for in-session comparison.
fn hash_ops(ops: &[Op]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    format!("{ops:?}").hash(&mut hasher);
    hasher.finish()
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
                    let captured: Vec<String> = lines[s..e].iter().map(|s| s.to_string()).collect();
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
                let captured: Vec<String> = lines[s - 1..e].iter().map(|s| s.to_string()).collect();
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
