//! `apply_patch` tool — apply a multi-file patch in codex's freeform
//! `*** Begin Patch` format.
//!
//! This is the bulk/multi-file replacement path; [`crate::tools::edit_file`] is
//! still the line-anchored small-edit path (TAG-validated, 3-way merge on
//! drift). `apply_patch` is the fallback when the model needs to add/delete
//! whole files or rewrite large regions in one shot.
//!
//! Matching is verbatim, not fuzzy: a hunk's context+removed lines must appear
//! as a contiguous block in the file, else the apply fails with a context-rich
//! error the model can correct. There is no Levenshtein/fuzzy search — a
//! stale read surfaces as an error, not a silent wrong edit.
//!
//! Format (clean-room implementation; the codex grammar is public, the code
//! here is original):
//!
//! ```text
//! *** Begin Patch
//! *** Add File: <path>
//! +<line>
//! +<line>
//! *** Delete File: <path>
//! *** Update File: <path>
//! @@ <optional anchor context line>
//!  <unchanged context line>
//! -<removed line>
//! +<added line>
//! *** End of File
//! *** End Patch
//! ```
//!
//! `@@ <text>` is an anchor hint: when the verbatim old block matches more than
//! one location, the match nearest a line equal to `<text>` wins. It does NOT
//! relax exact matching.

use std::path::PathBuf;
use std::sync::Arc;

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::sandbox::SandboxPolicy;
use crate::tool::AgentTool;

use super::{resolve_path_for_write, schema};

pub struct ApplyPatchTool {
    pub(crate) cwd: Arc<PathBuf>,
    pub(crate) sandbox: SandboxPolicy,
}

#[derive(Deserialize, JsonSchema)]
struct ApplyPatchInput {
    /// A codex-style `*** Begin Patch` document. Add/delete/update whole files
    /// or large regions in one call. Body line prefixes: `+` add, `-` remove,
    /// ` ` (space) unchanged context, `@@ <text>` anchor hint. A hunk's
    /// context+removed lines must match the file verbatim and contiguously
    /// (no fuzzy); on a stale read the apply fails with the offending block —
    /// re-`read_file` and retry. Build the patch from your latest `read_file`
    /// output, never from memory. Example:
    #[doc = r""]
    #[doc = r"```text"]
    #[doc = r"*** Begin Patch"]
    #[doc = r"*** Update File: /Users/me/proj/src/main.rs"]
    #[doc = r"@@ fn main"]
    #[doc = r" fn main() {"]
    #[doc = r#"-    println!("hello");"#]
    #[doc = r#"+    println!("hello, world");"#]
    #[doc = r" }"]
    #[doc = r"*** End of File"]
    #[doc = r"*** End Patch"]
    #[doc = r"```"]
    patch: String,
}

impl AgentTool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn description(&self) -> &str {
        "Apply a multi-file patch in codex `*** Begin Patch` freeform format. Add/delete whole \
         files or rewrite large regions in one shot; the line-anchored `edit_file` is still \
         preferred for small in-place edits (TAG-validated, 3-way merge on drift). Matching is \
         verbatim (no fuzzy): a hunk's context+removed lines must appear contiguously in the file, \
         else the apply fails with a context-rich error — re-read the file and retry. Build the \
         patch from your latest `read_file` output."
    }
    fn requires_approval(&self, _input: &serde_json::Value) -> bool {
        true
    }
    fn input_schema(&self) -> serde_json::Value {
        schema::<ApplyPatchInput>()
    }
    fn run(
        &self,
        input: serde_json::Value,
        _cancel: CancellationToken,
        ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<ApplyPatchInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let cwd = self.cwd.clone();
        let sandbox = self.sandbox.clone();
        let owner = ctx.agent_label().to_string();
        cx.background_spawn(async move {
            let ops = parse(&parsed.patch).map_err(|e| e.to_string())?;
            let mut results: Vec<String> = Vec::with_capacity(ops.len());
            for op in ops {
                results.push(apply_op(&op, &cwd, &sandbox, &owner)?);
            }
            Ok(results.join("\n"))
        })
    }
}

// ─── format model ─────────────────────────────────────────────────────────

/// One top-level `***` file operation.
#[derive(Debug)]
enum FileOp {
    Add { path: String, lines: Vec<String> },
    Delete { path: String },
    Update { path: String, hunks: Vec<Hunk> },
}

/// A hunk inside an `*** Update File` section: a verbatim old block (context +
/// removed lines) to locate, replaced by the new block (context + added
/// lines), with an optional `@@` anchor to disambiguate multiple matches.
#[derive(Debug)]
struct Hunk {
    anchor: Option<String>,
    lines: Vec<HunkLine>,
}

#[derive(Debug)]
enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

impl Hunk {
    /// The exact original lines this hunk expects to find (context + removed,
    /// in document order). Matched verbatim — no fuzzy.
    fn old_block(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(s) | HunkLine::Remove(s) => Some(s.as_str()),
                HunkLine::Add(_) => None,
            })
            .collect()
    }

    /// The replacement lines (context + added, in document order).
    fn new_block(&self) -> Vec<&str> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Context(s) | HunkLine::Add(s) => Some(s.as_str()),
                HunkLine::Remove(_) => None,
            })
            .collect()
    }
}

// ─── parser ───────────────────────────────────────────────────────────────

/// Parse a `*** Begin Patch … *** End Patch` document into file operations.
/// Errors carry the offending line number so the model can locate the miswrite.
fn parse(patch: &str) -> Result<Vec<FileOp>, ParseError> {
    let mut ops = Vec::new();
    let mut lines = patch.lines().peekable();
    let mut line_no = 0usize;

    // Skip a leading blank lines (models sometimes prepend one); the first
    // non-empty line must be `*** Begin Patch`.
    while let Some(l) = lines.peek() {
        if l.trim().is_empty() {
            lines.next();
            line_no += 1;
        } else {
            break;
        }
    }
    match lines.next() {
        Some(l) if l.trim_start() == "*** Begin Patch" => line_no += 1,
        _ => {
            return Err(ParseError {
                line: line_no,
                msg: "expected `*** Begin Patch` as the first line".into(),
            });
        }
    }

    while let Some(l) = lines.next() {
        line_no += 1;
        let trimmed = l.trim_start();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "*** End Patch" {
            return Ok(ops);
        }
        if let Some(path) = strip_directive(trimmed, "*** Add File:") {
            let body = collect_add_body(&mut lines, &mut line_no)?;
            ops.push(FileOp::Add {
                path: path.to_string(),
                lines: body,
            });
        } else if let Some(path) = strip_directive(trimmed, "*** Delete File:") {
            ops.push(FileOp::Delete {
                path: path.to_string(),
            });
        } else if let Some(path) = strip_directive(trimmed, "*** Update File:") {
            let hunks = collect_update_hunks(&mut lines, &mut line_no)?;
            ops.push(FileOp::Update {
                path: path.to_string(),
                hunks,
            });
        } else {
            return Err(ParseError {
                line: line_no,
                msg: format!(
                    "expected a `***` directive, got {trimmed:?} (inside a section, every line must start with ` `, `-`, `+`, or `@@`)"
                ),
            });
        }
    }
    Err(ParseError {
        line: line_no,
        msg: "missing `*** End Patch`".into(),
    })
}

#[derive(Debug)]
struct ParseError {
    line: usize,
    msg: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "apply_patch parse error at line {}: {}",
            self.line, self.msg
        )
    }
}

/// Strip a `*** Directive:` prefix, returning the trimmed path remainder as
/// a borrow of `line` (no allocation; the caller moves it into a `String`).
fn strip_directive<'a>(line: &'a str, directive: &str) -> Option<&'a str> {
    line.strip_prefix(directive).map(|r| r.trim())
}

/// Collect `+`-prefixed body lines of an `*** Add File` section until the next
/// `***` directive. A non-`+`, non-`***` line is a malformed body.
fn collect_add_body(
    lines: &mut std::iter::Peekable<std::str::Lines<'_>>,
    line_no: &mut usize,
) -> Result<Vec<String>, ParseError> {
    let mut body = Vec::new();
    while let Some(&l) = lines.peek() {
        if l.trim_start().starts_with("***") || l.trim_start() == "*** End Patch" {
            break;
        }
        lines.next();
        *line_no += 1;
        let stripped = l.strip_prefix('+');
        let content = match stripped {
            Some(rest) => rest,
            None => {
                return Err(ParseError {
                    line: *line_no,
                    msg: format!("Add File body line must start with `+`, got {l:?}"),
                });
            }
        };
        body.push(content.to_string());
    }
    Ok(body)
}

/// Collect hunks of an `*** Update File` section until `*** End of File`.
fn collect_update_hunks(
    lines: &mut std::iter::Peekable<std::str::Lines<'_>>,
    line_no: &mut usize,
) -> Result<Vec<Hunk>, ParseError> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut current: Option<Hunk> = None;
    while let Some(&l) = lines.peek() {
        let trimmed = l.trim_start();
        if trimmed.starts_with("***") {
            // *** End of File ends this section; another *** directive ends it
            // too (treat as implicit End of File so a missing marker is not
            // fatal — the next directive closes the update).
            if trimmed == "*** End of File" {
                lines.next();
                *line_no += 1;
                if let Some(h) = current.take() {
                    hunks.push(h);
                }
                return Ok(hunks);
            }
            break;
        }
        lines.next();
        *line_no += 1;
        // A hunk starts (or continues) at the first `@@`/` `/`-`/`+` line. A new
        // `@@` starts a fresh hunk. `@@ <text>` is a pure disambiguation hint,
        // NOT a context line: it is not part of the verbatim old block, so it
        // does not narrow matching — it only picks among multiple verbatim
        // matches by selecting the one nearest a file line equal to `<text>`.
        // This keeps matching exact (no fuzzy) while letting the model point at
        // which occurrence of a repeated block to edit.
        if let Some(rest) = l.strip_prefix("@@ ") {
            if let Some(h) = current.take() {
                hunks.push(h);
            }
            current = Some(Hunk {
                anchor: Some(rest.to_string()),
                lines: Vec::new(),
            });
        } else if let Some(rest) = l.strip_prefix(' ') {
            let h = current.get_or_insert_with(|| Hunk {
                anchor: None,
                lines: Vec::new(),
            });
            h.lines.push(HunkLine::Context(rest.to_string()));
        } else if let Some(rest) = l.strip_prefix('+') {
            let h = current.get_or_insert_with(|| Hunk {
                anchor: None,
                lines: Vec::new(),
            });
            h.lines.push(HunkLine::Add(rest.to_string()));
        } else if let Some(rest) = l.strip_prefix('-') {
            let h = current.get_or_insert_with(|| Hunk {
                anchor: None,
                lines: Vec::new(),
            });
            h.lines.push(HunkLine::Remove(rest.to_string()));
        } else if l.trim().is_empty() {
            // A bare blank line is ambiguous; codex represents an unchanged
            // blank context line as a single space. Reject so the model spells
            // it as ` ` for context or `+`/`-` for add/remove of a blank line.
            return Err(ParseError {
                line: *line_no,
                msg: "bare blank line in an Update section — use ` ` (space) for an unchanged blank line, or `+`/`-` for an added/removed blank line".into(),
            });
        } else {
            return Err(ParseError {
                line: *line_no,
                msg: format!("Update hunk line must start with ` `, `-`, `+`, or `@@ `, got {l:?}"),
            });
        }
    }
    if let Some(h) = current.take() {
        hunks.push(h);
    }
    Ok(hunks)
}

// ─── apply ─────────────────────────────────────────────────────────────────

/// Apply one file operation: resolve the path under the sandbox write
/// confinement, take the per-file write lock, then add/delete/update.
fn apply_op(
    op: &FileOp,
    cwd: &std::path::Path,
    sandbox: &SandboxPolicy,
    owner: &str,
) -> Result<String, String> {
    let raw_path = match op {
        FileOp::Add { path, .. } | FileOp::Delete { path } | FileOp::Update { path, .. } => path,
    };
    let path = resolve_path_for_write(raw_path, cwd, sandbox)?;
    let path_display = path.display().to_string();

    let _lock = match crate::tools::file_lock::try_acquire(&path, owner) {
        Ok(g) => g,
        Err(held) => {
            return Err(format!(
                "apply_patch blocked: {path_display} is being written by {}; re-read and retry",
                held.owner
            ));
        }
    };

    match op {
        FileOp::Add { lines, .. } => {
            if path.exists() {
                return Err(format!(
                    "apply_patch Add File: {path_display} already exists; use Update File to edit it"
                ));
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("apply_patch create dir {path_display}: {e}"))?;
            }
            let mut content = lines.join("\n");
            if !content.is_empty() {
                content.push('\n');
            }
            std::fs::write(&path, content.as_bytes())
                .map_err(|e| format!("apply_patch write {path_display}: {e}"))?;
            Ok(format!("added {path_display} ({} lines)", lines.len()))
        }
        FileOp::Delete { .. } => {
            if !path.exists() {
                return Err(format!(
                    "apply_patch Delete File: {path_display} does not exist"
                ));
            }
            std::fs::remove_file(&path)
                .map_err(|e| format!("apply_patch delete {path_display}: {e}"))?;
            Ok(format!("deleted {path_display}"))
        }
        FileOp::Update { hunks, .. } => {
            let raw = std::fs::read_to_string(&path)
                .map_err(|e| format!("apply_patch read {path_display}: {e}"))?;
            let had_trailing_nl = raw.ends_with('\n');
            let mut file_lines: Vec<String> = raw.lines().map(String::from).collect();

            for hunk in hunks {
                apply_hunk(&mut file_lines, hunk, &path_display)?;
            }

            let mut out = file_lines.join("\n");
            if had_trailing_nl && !out.is_empty() {
                out.push('\n');
            }
            std::fs::write(&path, out.as_bytes())
                .map_err(|e| format!("apply_patch write {path_display}: {e}"))?;
            Ok(format!("updated {path_display} ({} hunks)", hunks.len()))
        }
    }
}

/// Locate a hunk's verbatim old block in `file_lines` and replace it with the
/// new block. No fuzzy: an exact contiguous match is required. When the old
/// block matches more than one location, the `@@` anchor picks the match
/// nearest a line equal to the anchor text; absent an anchor, multiple matches
/// are an error (the model must add disambiguating context).
fn apply_hunk(file_lines: &mut Vec<String>, hunk: &Hunk, path_display: &str) -> Result<(), String> {
    let old_block: Vec<&str> = hunk.old_block();
    let new_block: Vec<&str> = hunk.new_block();

    if old_block.is_empty() {
        return Err(format!(
            "apply_patch {path_display}: hunk has no context/remove lines — nothing to locate; add context lines or use Add File"
        ));
    }

    let matches = find_matches(file_lines, &old_block);
    let idx = match matches.len() {
        0 => {
            return Err(no_match_error(file_lines, &old_block, path_display));
        }
        1 => matches[0],
        _ => {
            // Disambiguate via the `@@` anchor: pick the match whose window is
            // nearest a file line equal to the anchor text.
            let Some(anchor) = &hunk.anchor else {
                return Err(format!(
                    "apply_patch {path_display}: old block matches {} locations and no `@@` anchor — add disambiguating context lines",
                    matches.len()
                ));
            };
            let anchor_positions: Vec<usize> = file_lines
                .iter()
                .enumerate()
                .filter(|(_, l)| l.as_str() == anchor.as_str())
                .map(|(i, _)| i)
                .collect();
            if anchor_positions.is_empty() {
                return Err(format!(
                    "apply_patch {path_display}: `@@` anchor {anchor:?} not found in file; the patch is stale — re-read the file"
                ));
            }
            pick_nearest_match(&matches, &old_block.len(), &anchor_positions).ok_or_else(|| {
                format!(
                    "apply_patch {path_display}: `@@` anchor did not disambiguate {} matches",
                    matches.len()
                )
            })?
        }
    };

    // Splice: replace [idx, idx+old_block.len()) with new_block.
    let end = idx + old_block.len();
    let replacement: Vec<String> = new_block.iter().map(|s| s.to_string()).collect();
    file_lines.splice(idx..end, replacement);
    Ok(())
}

/// All start indices where `haystack[i..i+needle.len()] == needle`.
fn find_matches(haystack: &[String], needle: &[&str]) -> Vec<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return Vec::new();
    }
    (0..=haystack.len() - needle.len())
        .filter(|i| {
            haystack[*i..*i + needle.len()]
                .iter()
                .map(|s| s.as_str())
                .eq(needle.iter().copied())
        })
        .collect()
}

/// Among `match_starts`, pick the one whose window is closest (min line
/// distance) to any anchor position.
fn pick_nearest_match(
    match_starts: &[usize],
    block_len: &usize,
    anchor_positions: &[usize],
) -> Option<usize> {
    match_starts.iter().copied().min_by_key(|&start| {
        // Distance from each anchor to the [start, end) window, collapsed to
        // zero when the anchor sits inside (or adjacent to) the window.
        let end = start + *block_len;
        anchor_positions
            .iter()
            .map(|&a| a.saturating_sub(end).max(start.saturating_sub(a)))
            .min()
            .unwrap_or(usize::MAX)
    })
}

/// Build a context-rich error for a hunk whose old block is absent from the
/// file: show the expected block and the file's nearest line (by simple
/// overlap), so the model can correct a stale read.
fn no_match_error(file_lines: &[String], old_block: &[&str], path_display: &str) -> String {
    let expected_str = old_block
        .iter()
        .map(|l| format!("  | {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    // Surface a window of the file around the first old-block line that does
    // appear, so the model sees where the patch drifted.
    let probe = old_block.first().copied().unwrap_or("");
    let hint = if !probe.is_empty() {
        if let Some(idx) = file_lines.iter().position(|l| l.as_str() == probe) {
            let lo = idx.saturating_sub(2);
            let hi = (idx + 3).min(file_lines.len());
            let window: Vec<String> = file_lines[lo..hi]
                .iter()
                .enumerate()
                .map(|(i, l)| format!("  {:>4}: {l}", lo + i + 1))
                .collect();
            format!("\nfile around line {}:\n{}", idx + 1, window.join("\n"))
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    format!(
        "apply_patch {path_display}: old block not found verbatim — the file does not match your read. Expected:\n{expected_str}{hint}\n\nRe-read the file and rebuild the patch from its current content."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk_ctx(lines: &[(&str, &str)]) -> Hunk {
        // (prefix, content) → HunkLine.
        let mut hl = Vec::new();
        for &(p, c) in lines {
            match p {
                " " => hl.push(HunkLine::Context(c.to_string())),
                "-" => hl.push(HunkLine::Remove(c.to_string())),
                "+" => hl.push(HunkLine::Add(c.to_string())),
                _ => panic!("bad prefix {p}"),
            }
        }
        Hunk {
            anchor: None,
            lines: hl,
        }
    }

    #[test]
    fn parse_add_delete_update_round_trip() {
        let doc = "\
*** Begin Patch
*** Add File: /tmp/new.txt
+first
+second
*** Delete File: /tmp/old.txt
*** Update File: /tmp/up.txt
 fn a() {
-    println!(\"a\");
+    println!(\"b\");
 }
*** End of File
*** End Patch
";
        let ops = parse(doc).expect("parse");
        assert_eq!(ops.len(), 3);
        match &ops[0] {
            FileOp::Add { path, lines } => {
                assert_eq!(path, "/tmp/new.txt");
                assert_eq!(lines, &vec!["first".to_string(), "second".to_string()]);
            }
            _ => panic!("expected Add"),
        }
        assert!(matches!(&ops[1], FileOp::Delete { path } if path == "/tmp/old.txt"));
        match &ops[2] {
            FileOp::Update { path, hunks } => {
                assert_eq!(path, "/tmp/up.txt");
                assert_eq!(hunks.len(), 1);
                assert_eq!(hunks[0].lines.len(), 4);
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn parse_anchor_splits_hunks() {
        let doc = "\
*** Begin Patch
*** Update File: /tmp/f.rs
@@ fn a
 fn a() {
-x
+y
@@ fn b
 fn b() {
-z
+w
*** End of File
*** End Patch
";
        let ops = parse(doc).expect("parse");
        match &ops[0] {
            FileOp::Update { hunks, .. } => {
                assert_eq!(hunks.len(), 2, "two @@ anchors → two hunks");
                assert_eq!(hunks[0].anchor.as_deref(), Some("fn a"));
                assert_eq!(hunks[1].anchor.as_deref(), Some("fn b"));
                // `@@` is a pure hint, not a context line: the hunk body is the
                // three ` `/`-`/`+` lines that follow, with no synthesized context.
                assert_eq!(hunks[0].lines.len(), 3);
                assert!(matches!(hunks[0].lines[0], HunkLine::Context(_)));
            }
            _ => panic!("expected Update"),
        }
    }

    #[test]
    fn parse_rejects_missing_begin_patch() {
        let err = parse("*** Add File: /tmp/x\n+y\n").unwrap_err();
        assert!(err.to_string().contains("Begin Patch"), "{}", err);
    }

    #[test]
    fn parse_rejects_bare_blank_line_in_update() {
        let doc = "\
*** Begin Patch
*** Update File: /tmp/f
 a

+b
*** End of File
*** End Patch
";
        let err = parse(doc).unwrap_err();
        assert!(err.to_string().contains("bare blank line"), "{}", err);
    }

    #[test]
    fn apply_hunk_replaces_verbatim_block() {
        let mut lines: Vec<String> = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let hunk = hunk_ctx(&[(" ", "b"), ("-", "c"), ("+", "C")]);
        apply_hunk(&mut lines, &hunk, "/tmp/f").unwrap();
        assert_eq!(lines, vec!["a", "b", "C", "d"]);
    }

    #[test]
    fn apply_hunk_no_match_is_error_with_context() {
        let mut lines: Vec<String> = vec!["a".into(), "b".into()];
        let hunk = hunk_ctx(&[(" ", "x"), ("-", "y"), ("+", "z")]);
        let err = apply_hunk(&mut lines, &hunk, "/tmp/f").unwrap_err();
        assert!(err.contains("not found verbatim"), "{}", err);
        assert!(err.contains("/tmp/f"), "{}", err);
    }

    #[test]
    fn apply_hunk_anchor_disambiguates_multiple_matches() {
        // old block [a,b] matches at 0 and 2; the `@@` landmark sits just past
        // the second occurrence, so the anchor picks match 2 (distance 0) over
        // match 0 (distance 2). The anchor is a pure hint — not part of the old
        // block — so it disambiguates without narrowing the verbatim match.
        let mut lines: Vec<String> = ["a", "b", "a", "b", "LANDMARK"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut hunk = hunk_ctx(&[(" ", "a"), ("-", "b"), ("+", "B")]);
        hunk.anchor = Some("LANDMARK".to_string());
        apply_hunk(&mut lines, &hunk, "/tmp/f").unwrap();
        assert_eq!(lines, vec!["a", "b", "a", "B", "LANDMARK"]);
    }

    #[test]
    fn apply_hunk_anchor_not_found_is_error() {
        // A landmark absent from the file signals a stale read; the apply
        // refuses rather than guessing which match to edit.
        let mut lines: Vec<String> = ["a", "b", "a", "b"].iter().map(|s| s.to_string()).collect();
        let mut hunk = hunk_ctx(&[(" ", "a"), ("-", "b")]);
        hunk.anchor = Some("NOPE".to_string());
        let err = apply_hunk(&mut lines, &hunk, "/tmp/f").unwrap_err();
        assert!(err.contains("`@@` anchor"), "{}", err);
        assert!(err.contains("not found"), "{}", err);
    }

    #[test]
    fn apply_hunk_multiple_matches_without_anchor_is_error() {
        let mut lines: Vec<String> = vec!["a".into(), "b".into(), "a".into(), "b".into()];
        let hunk = hunk_ctx(&[(" ", "a"), (" ", "b")]);
        let err = apply_hunk(&mut lines, &hunk, "/tmp/f").unwrap_err();
        assert!(err.contains("matches 2 locations"), "{}", err);
        assert!(err.contains("no `@@` anchor"), "{}", err);
    }

    #[test]
    fn apply_hunk_blank_add_and_remove_lines() {
        // `+` alone adds a blank line; `-` alone removes one.
        let mut lines: Vec<String> = vec!["x".into(), "y".into()];
        let hunk = hunk_ctx(&[(" ", "x"), ("+", "")]);
        apply_hunk(&mut lines, &hunk, "/tmp/f").unwrap();
        assert_eq!(lines, vec!["x", "", "y"]);

        let mut lines: Vec<String> = vec!["x".into(), "".into(), "y".into()];
        let hunk = hunk_ctx(&[(" ", "x"), ("-", ""), (" ", "y")]);
        apply_hunk(&mut lines, &hunk, "/tmp/f").unwrap();
        assert_eq!(lines, vec!["x", "y"]);
    }

    #[test]
    fn find_matches_counts_contiguous_occurrences() {
        let h: Vec<String> = ["a", "b", "a", "b"].iter().map(|s| s.to_string()).collect();
        let needle: Vec<&str> = vec!["a", "b"];
        let m = find_matches(&h, &needle);
        assert_eq!(m, vec![0, 2]);
    }
}
