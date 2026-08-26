//! Hashline patch parser.
//!
//! Parses a patch text into a list of `FilePatch` sections. The grammar is a
//! line-oriented state machine:
//!
//! ```text
//! *** Begin Patch            (optional envelope open)
//! [PATH#TAG]                 (file section header)
//! SWAP N.=M: / SWAP N:       (replace range, body follows)
//! SWAP.BLK N:                (replace bracket-block, body follows)
//! DEL N.=M / DEL N           (delete range, no body)
//! DEL.BLK N                  (delete bracket-block, no body)
//! CUT N.=M / CUT N           (delete range + capture to clipboard, no body)
//! CUT.BLK N                  (delete bracket-block + capture, no body)
//! PASTE.PRE N: / PASTE.POST N: / PASTE.HEAD: / PASTE.TAIL:
//!                            (paste clipboard at position, no body)
//! REM                        (delete the file named by the section header)
//! MV DEST                    (rename/move the section file to DEST)
//! INS.PRE N: / INS.POST N:   (insert before/after anchor, body follows)
//! INS.HEAD: / INS.TAIL:      (insert at start/end, body follows)
//! INS.BLK.POST N:            (insert after bracket-block, body follows)
//! +TEXT                      (body row; `+` alone = blank line;
//!                            `+-x`/`++x` escapes a literal `-`/`+` lead)
//! *** End Patch              (optional envelope close)
//! ```
//!
//! Line numbers are 1-indexed, non-zero, no leading zeros. Ranges are inclusive
//! on both ends. Only body-bearing headers end in `:`; `DEL`, `CUT`, `REM`,
//! `MV`, and `PASTE` have no body.
//!
//! ## Tolerance layer
//!
//! Models routinely drift from the strict grammar — forgetting the `+` body
//! prefix, pasting `read` output rows verbatim, mixing unified-diff `-` rows,
//! or writing range separators as `-`/`..`. Rather than rejecting the whole
//! patch, the parser recovers the author's intent and reports a model-facing
//! warning per correction. Recovery is applied only where the intent is
//! unambiguous; anything genuinely ambiguous still errors so the model fixes
//! the patch instead of landing corrupted content. Each correction is
//! announced in the edit result's `Warnings:` block.
use std::path::PathBuf;

/// Insertion position for `INS` / `PASTE` ops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsPos {
    Pre,
    Post,
    Head,
    Tail,
}

/// A single parsed operation against a file's line array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// `SWAP N.=M:` — replace inclusive lines `start..=end` with `body`.
    Swap {
        start: usize,
        end: usize,
        body: Vec<String>,
    },
    /// `DEL N.=M` / `DEL N` — delete inclusive lines `start..=end`.
    Del { start: usize, end: usize },
    /// `INS.PRE N:` / `INS.POST N:` / `INS.HEAD:` / `INS.TAIL:` — insert `body`.
    Ins {
        pos: InsPos,
        anchor: Option<usize>,
        body: Vec<String>,
    },
    /// `SWAP.BLK N:` — resolve bracket-block at `start`, replace its span.
    SwapBlk { start: usize, body: Vec<String> },
    /// `DEL.BLK N` — resolve bracket-block at `start`, delete its span.
    DelBlk { start: usize },
    /// `INS.BLK.POST N:` — insert after the bracket-block at `anchor`.
    InsBlkPost { anchor: usize, body: Vec<String> },
    /// `CUT N.=M` / `CUT N` — delete range and capture lines to clipboard.
    Cut { start: usize, end: usize },
    /// `CUT.BLK N` — resolve bracket-block at `start`, delete and capture it.
    CutBlk { start: usize },
    /// `PASTE.PRE N:` / `PASTE.POST N:` / `PASTE.HEAD:` / `PASTE.TAIL:` —
    /// paste clipboard at position (no body; clipboard supplies content).
    Paste { pos: InsPos, anchor: Option<usize> },
}

/// A file-level operation for the section (mutually exclusive with line ops).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileOp {
    /// `REM` — delete the file named by the section header.
    Rem,
    /// `MV DEST` — rename/move the section file to DEST.
    Move { dest: String },
}

/// One file section: a path, the snapshot tag it claims, its operations, and
/// an optional file-level operation (mutually exclusive with line ops).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    pub path: PathBuf,
    pub tag: String,
    pub ops: Vec<Op>,
    pub file_op: Option<FileOp>,
}

/// Parse failure carrying the 1-indexed line and a reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "patch line {} parse failed: {}", self.line, self.message)
    }
}

impl std::error::Error for ParseError {}

/// The parse result: file sections plus model-facing warnings describing every
/// tolerant correction the parser applied. Empty when the patch was clean.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ParsedPatch {
    pub files: Vec<FilePatch>,
    pub warnings: Vec<String>,
}

// ── Warning texts (model-facing, deduplicated per patch) ────────────────────

const WARN_BARE_BODY: &str = "Accepted body row(s) without the `+` prefix as literal content. Prefer explicit `+TEXT` rows so intent is unambiguous.";
const WARN_MINUS_BULLET: &str =
    "Kept `- `-prefixed Markdown bullet row(s) as literal content. The explicit form is `+- item`.";
const WARN_DIFF_OLD_ROWS: &str = "Discarded `-old` row(s) from a hunk body: the SWAP/DEL range already removes the old lines; the body carries only the NEW content as `+TEXT` rows.";
const WARN_SNAPSHOT_ROWS: &str = "Recovered top-level `N:TEXT` row(s) as single-line `SWAP N.=N:` replacements. Use explicit `SWAP` headers for reliable edits.";
const WARN_BARE_RANGE: &str =
    "Recovered a bare `N.=M:` header as `SWAP N.=M:`. Prefix replacement ranges with `SWAP`.";
const WARN_READ_METADATA: &str = "Ignored read-output metadata/elision line(s) (truncation notices, `...` gap markers) — they are display chrome, not source.";
const WARN_BARE_PREFIX_STRIP: &str = "Stripped uniform `N:` line-number prefixes from pasted body row(s). Author body rows as plain `+TEXT` instead of echoing `read` output.";
const WARN_EMPTY_SWAP: &str =
    "Interpreted an empty `SWAP` body as deletion. Use `DEL N.=M` for bodyless deletes.";
const WARN_BODYLESS_COLON: &str = "Ignored a trailing `:` on a bodyless `DEL`/`CUT`. Body-less directives take no colon (e.g. `DEL N.=M`, `CUT N`).";
const WARN_PAIR_COALESCED: &str = "Coalesced duplicate `SWAP` hunks targeting the same range (a before/after pair); only the last body was applied.";

/// One body row, classified while streaming.
#[derive(Debug, Clone)]
struct BodyRow {
    text: String,
    line_no: usize,
    /// `false` when the row was authored with the `+` prefix.
    bare: bool,
}

impl BodyRow {
    fn is_minus(&self) -> bool {
        self.bare && self.text.trim_start().starts_with('-')
    }
}

/// Pending body state for the body-bearing op currently open.
#[derive(Debug, Default)]
struct PendingBody {
    rows: Vec<BodyRow>,
    /// Blank lines held back until a later body row proves they were interior.
    deferred_blanks: Vec<BodyRow>,
}

/// Parse a patch text into file sections plus tolerant-recovery warnings.
pub fn parse_patch(text: &str) -> Result<ParsedPatch, ParseError> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut pending: Option<PendingBody> = None;
    // Snapshot rows recovered per current section, guarding repeated numbers.
    let mut recovered_lines: std::collections::HashSet<usize> = std::collections::HashSet::new();

    let mut warn = |warnings: &mut Vec<String>, msg: &'static str| {
        if !warnings.iter().any(|w| w == msg) {
            warnings.push(msg.to_string());
        }
    };

    for (idx, raw) in text.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim_end_matches('\r');

        // Optional envelope markers — accepted anywhere, consumed silently.
        if line.trim() == "*** Begin Patch" || line.trim() == "*** End Patch" {
            continue;
        }

        // apply_patch envelopes are a different grammar; name the drift.
        let trimmed = line.trim();
        if trimmed.starts_with("*** Update File:")
            || trimmed.starts_with("*** Add File:")
            || trimmed.starts_with("*** Delete File:")
            || trimmed.starts_with("*** Move to:")
        {
            return Err(ParseError {
                line: line_no,
                message: format!(
                    "{trimmed:?} is an apply_patch/Codex envelope; this is hashline grammar. \
                     Open each file with a `[path#TAG]` header (from your latest `read`) and \
                     edit with `SWAP N.=M:` / `DEL N.=M` / `INS.PRE N:`."
                ),
            });
        }

        // Blank lines: interior blanks belong to an open body (held back until
        // proven); outside a body they are pure layout.
        if trimmed.is_empty() {
            if let Some(pb) = pending.as_mut() {
                pb.deferred_blanks.push(BodyRow {
                    text: line.to_string(),
                    line_no,
                    bare: true,
                });
            }
            continue;
        }

        // A valid section header always terminates the open body.
        if let Some(section) = parse_section_header(trimmed) {
            flush_body(
                &mut files,
                &mut pending,
                &mut recovered_lines,
                &mut warnings,
            )?;
            recovered_lines.clear();
            files.push(section);
            continue;
        }

        // Display-only read metadata is never source content (checked before the
        // bracketed-header rule: `[Showing ...]` notices are bracketed too).
        if pending.is_none() && is_read_metadata(trimmed) {
            warn(&mut warnings, WARN_READ_METADATA);
            continue;
        }

        // A bracketed line that is NOT a valid header is a mistyped header —
        // fail closed rather than swallow it as body content.
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            return Err(ParseError {
                line: line_no,
                message: format!(
                    "section header {trimmed:?} is missing a valid 6-hex snapshot tag. Copy \
                     the exact `[path#TAG]` header from your latest `read` of this file. \
                     (Files over 4MB carry no tag — edit them with `Write` instead.)"
                ),
            });
        }

        // Op headers.
        match parse_op_header(trimmed) {
            Ok(ParsedOp::BodyBearer(op)) => {
                flush_body(
                    &mut files,
                    &mut pending,
                    &mut recovered_lines,
                    &mut warnings,
                )?;
                push_op(&mut files, op);
                pending = Some(PendingBody::default());
                continue;
            }
            Ok(ParsedOp::Bodyless(op)) => {
                flush_body(
                    &mut files,
                    &mut pending,
                    &mut recovered_lines,
                    &mut warnings,
                )?;
                // A bodyless directive that carried a stray trailing colon.
                if trimmed.ends_with(':') {
                    warn(&mut warnings, WARN_BODYLESS_COLON);
                }
                push_op(&mut files, op);
                continue;
            }
            Ok(ParsedOp::File(file_op)) => {
                flush_body(
                    &mut files,
                    &mut pending,
                    &mut recovered_lines,
                    &mut warnings,
                )?;
                if let Some(sec) = files.last_mut() {
                    if sec.file_op.is_some() {
                        return Err(ParseError {
                            line: line_no,
                            message: "only one file-level op (REM or MV) per section".to_string(),
                        });
                    }
                    sec.file_op = Some(file_op);
                }
                continue;
            }
            Err(msg) => {
                // An explicit `+` row continues an open body.
                if pending.is_some()
                    && let Some(rest) = strip_body_prefix(line)
                {
                    let pb = pending.as_mut().unwrap();
                    if !pb.deferred_blanks.is_empty() {
                        pb.rows.append(&mut pb.deferred_blanks);
                    }
                    pb.rows.push(BodyRow {
                        text: rest,
                        line_no,
                        bare: false,
                    });
                    continue;
                }
                // Inside a body an unrecognized line that looks like an op
                // directive is a malformed header (fail closed); anything else
                // is a bare body row recovered below.
                if pending.is_some() && !looks_like_op_header(trimmed) {
                    fall_bare(line, line_no, &mut pending, &mut warnings, &mut warn);
                    continue;
                }
                // Top level (no body) can still recover a bare range / snapshot row.
                if pending.is_none() {
                    if let Some((start, end)) = parse_bare_range_header(trimmed) {
                        flush_body(
                            &mut files,
                            &mut pending,
                            &mut recovered_lines,
                            &mut warnings,
                        )?;
                        push_op(
                            &mut files,
                            Op::Swap {
                                start,
                                end,
                                body: Vec::new(),
                            },
                        );
                        pending = Some(PendingBody::default());
                        warn(&mut warnings, WARN_BARE_RANGE);
                        continue;
                    }
                    if let Some((n, content)) = parse_snapshot_row(trimmed) {
                        if !recovered_lines.insert(n) {
                            return Err(ParseError {
                                line: line_no,
                                message: format!(
                                    "repeated snapshot row {n}: each line number may appear once \
                                     in recovered `N:TEXT` rows"
                                ),
                            });
                        }
                        flush_body(
                            &mut files,
                            &mut pending,
                            &mut recovered_lines,
                            &mut warnings,
                        )?;
                        push_op(
                            &mut files,
                            Op::Swap {
                                start: n,
                                end: n,
                                body: vec![content.to_string()],
                            },
                        );
                        warn(&mut warnings, WARN_SNAPSHOT_ROWS);
                        continue;
                    }
                }
                return Err(ParseError {
                    line: line_no,
                    message: msg,
                });
            }
        }
    }

    // Flush any trailing body.
    flush_body(
        &mut files,
        &mut pending,
        &mut recovered_lines,
        &mut warnings,
    )?;

    // A before/after pair written as two SWAP hunks on the same range keeps
    // only the final body.
    coalesce_duplicate_swaps(&mut files, &mut warnings);

    if files.is_empty() {
        return Err(ParseError {
            line: 0,
            message: "patch contains no [PATH#TAG] section".to_string(),
        });
    }
    Ok(ParsedPatch { files, warnings })
}

/// Drop every earlier `SWAP` whose range exactly matches a later `SWAP` in the
/// same section — the model wrote a before/after pair; the last body wins.
fn coalesce_duplicate_swaps(files: &mut [FilePatch], warnings: &mut Vec<String>) {
    let mut coalesced = false;
    for sec in files.iter_mut() {
        let n = sec.ops.len();
        let mut drop: std::collections::HashSet<usize> = std::collections::HashSet::new();
        for i in 0..n {
            let Op::Swap { start, end, .. } = &sec.ops[i] else {
                continue;
            };
            let (s, e) = (*start, *end);
            let has_later = sec.ops[(i + 1)..]
                .iter()
                .any(|op| matches!(op, Op::Swap { start, end, .. } if *start == s && *end == e));
            if has_later {
                drop.insert(i);
            }
        }
        if drop.is_empty() {
            continue;
        }
        coalesced = true;
        let kept: Vec<Op> = sec
            .ops
            .iter()
            .enumerate()
            .filter(|(i, _)| !drop.contains(i))
            .map(|(_, op)| op.clone())
            .collect();
        sec.ops = kept;
    }
    if coalesced && !warnings.iter().any(|w| w == WARN_PAIR_COALESCED) {
        warnings.push(WARN_PAIR_COALESCED.to_string());
    }
}

/// Append a bare row to the open body, committing any deferred blanks first.
fn fall_bare(
    line: &str,
    line_no: usize,
    pending: &mut Option<PendingBody>,
    warnings: &mut Vec<String>,
    warn: &mut impl FnMut(&mut Vec<String>, &'static str),
) {
    let pb = match pending.as_mut() {
        Some(pb) => pb,
        None => return,
    };
    // A later row proves the deferred blanks were interior body lines.
    if !pb.deferred_blanks.is_empty() {
        pb.rows.append(&mut pb.deferred_blanks);
        warn(warnings, WARN_BARE_BODY);
    }
    let minus = line.trim_start().starts_with('-');
    pb.rows.push(BodyRow {
        text: line.to_string(),
        line_no,
        bare: true,
    });
    if !minus {
        warn(warnings, WARN_BARE_BODY);
    }
}

/// A markdown bullet: optional indent, `-`, exactly one space, then content.
/// Unified-diff `-` rows almost never match (code deletions glue the `-` on
/// with no space, or indent with multiple spaces).
fn is_md_bullet(text: &str) -> bool {
    let t = text.trim_start();
    let Some(rest) = t.strip_prefix('-') else {
        return false;
    };
    let Some(rest) = rest.strip_prefix(' ') else {
        return false;
    };
    !rest.is_empty() && !rest.starts_with(' ')
}

/// Judge bare `-` rows once the whole body is known. Bullet rows stand alone
/// as content; non-bullet `-` rows paired with explicit `+` rows are diff
/// contamination and are dropped; anything else stays rejected.
fn resolve_minus_rows(
    rows: &mut Vec<BodyRow>,
    warnings: &mut Vec<String>,
) -> Result<(), ParseError> {
    let Some(first_minus) = rows.iter().find(|r| r.is_minus()) else {
        return Ok(());
    };
    let all_bullet = rows
        .iter()
        .filter(|r| r.is_minus())
        .all(|r| is_md_bullet(&r.text));
    let has_explicit = rows.iter().any(|r| !r.bare);
    let has_explicit_bullet = rows.iter().any(|r| !r.bare && is_md_bullet(&r.text));
    if all_bullet && (!has_explicit || has_explicit_bullet) {
        if !warnings.iter().any(|w| w == WARN_MINUS_BULLET) {
            warnings.push(WARN_MINUS_BULLET.to_string());
        }
        return Ok(());
    }
    if has_explicit && !all_bullet {
        rows.retain(|r| !r.is_minus());
        if !warnings.iter().any(|w| w == WARN_DIFF_OLD_ROWS) {
            warnings.push(WARN_DIFF_OLD_ROWS.to_string());
        }
        return Ok(());
    }
    Err(ParseError {
        line: first_minus.line_no,
        message: "`-` rows are unified-diff deletions, not hashline body content. The SWAP/DEL \
                  range already removes the old lines; the body carries only the NEW content as \
                  `+TEXT` rows. For a literal Markdown bullet write `+- item`."
            .to_string(),
    })
}

/// A lone quoted/numeric literal, optionally comma-terminated — the shape of a
/// numeric-keyed dict value. Uniform `N:` prefixes over such a body are real
/// content and must stay.
fn is_bare_literal_value(text: &str) -> bool {
    let t = text.trim().trim_end_matches(',').trim();
    if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
        return true;
    }
    if t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2 {
        return true;
    }
    let body = t.strip_prefix(['-', '+']).unwrap_or(t);
    if body.is_empty() {
        return false;
    }
    let mut saw_digit = false;
    let mut saw_dot = false;
    for c in body.chars() {
        if c.is_ascii_digit() {
            saw_digit = true;
        } else if c == '.' {
            if saw_dot {
                return false;
            }
            saw_dot = true;
        } else {
            return false;
        }
    }
    saw_digit
}

/// Strip one read-output line-number prefix (`N:` / `N|`, optionally preceded
/// by a grep `>` or context marker) from `text`; returns `text` unchanged when
/// no prefix is present.
fn strip_one_line_prefix(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'>') {
        i += 1;
    }
    let digit_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == digit_start {
        return text;
    }
    if i < bytes.len() && (bytes[i] == b':' || bytes[i] == b'|') {
        return &text[i + 1..];
    }
    text
}

/// When every non-blank bare row carries a read-output `N:` prefix, strip the
/// prefixes (the body was pasted straight from `read`). A body whose stripped
/// remainders are all lone literals is a numeric-keyed dict and is left alone.
fn strip_uniform_prefixes(rows: &mut [BodyRow], warnings: &mut Vec<String>) {
    let mut saw_bare = false;
    let mut all_literal = true;
    for r in rows.iter() {
        if !r.bare || r.text.trim().is_empty() {
            continue;
        }
        saw_bare = true;
        let stripped = strip_one_line_prefix(&r.text);
        if stripped.len() == r.text.len() {
            return;
        }
        if !is_bare_literal_value(stripped) {
            all_literal = false;
        }
    }
    if !saw_bare || all_literal {
        return;
    }
    for r in rows.iter_mut() {
        if r.bare && !r.text.trim().is_empty() {
            let stripped = strip_one_line_prefix(&r.text);
            r.text = stripped.to_string();
        }
    }
    if !warnings.iter().any(|w| w == WARN_BARE_PREFIX_STRIP) {
        warnings.push(WARN_BARE_PREFIX_STRIP.to_string());
    }
}

fn push_op(files: &mut [FilePatch], op: Op) {
    if let Some(sec) = files.last_mut() {
        sec.ops.push(op);
    }
}

/// Flush the pending body into the body-bearing op it belongs to, applying the
/// minus-row / prefix-strip / empty-body recoveries along the way.
fn flush_body(
    files: &mut [FilePatch],
    pending: &mut Option<PendingBody>,
    recovered_lines: &mut std::collections::HashSet<usize>,
    warnings: &mut Vec<String>,
) -> Result<(), ParseError> {
    let mut pb = match pending.take() {
        Some(pb) => pb,
        None => return Ok(()),
    };
    // Trailing deferred blanks are layout; interior ones were already committed.
    pb.deferred_blanks.clear();

    resolve_minus_rows(&mut pb.rows, warnings)?;
    strip_uniform_prefixes(&mut pb.rows, warnings);

    let body: Vec<String> = pb.rows.into_iter().map(|r| r.text).collect();

    let Some(sec) = files.last_mut() else {
        return Ok(());
    };
    let Some(last) = sec.ops.last_mut() else {
        return Ok(());
    };

    match last {
        Op::Swap {
            start,
            end,
            body: b,
        } => {
            if body.is_empty() {
                if !warnings.iter().any(|w| w == WARN_EMPTY_SWAP) {
                    warnings.push(WARN_EMPTY_SWAP.to_string());
                }
                let (s, e) = (*start, *end);
                *last = Op::Del { start: s, end: e };
            } else {
                *b = body;
            }
        }
        Op::SwapBlk { start, body: b } => {
            if body.is_empty() {
                if !warnings.iter().any(|w| w == WARN_EMPTY_SWAP) {
                    warnings.push(WARN_EMPTY_SWAP.to_string());
                }
                let s = *start;
                *last = Op::DelBlk { start: s };
            } else {
                *b = body;
            }
        }
        Op::Ins {
            pos: _,
            anchor: _,
            body: b,
        } => {
            if body.is_empty() {
                return Err(ParseError {
                    line: 0,
                    message: "INS header has no body rows — `INS` takes at least one `+TEXT` \
                              row; use `DEL` for bodyless deletes"
                        .to_string(),
                });
            }
            *b = body;
        }
        Op::InsBlkPost { anchor: _, body: b } => {
            if body.is_empty() {
                return Err(ParseError {
                    line: 0,
                    message: "INS.BLK.POST header has no body rows — add at least one `+TEXT` row"
                        .to_string(),
                });
            }
            *b = body;
        }
        // Bodyless ops never open a pending body; their body stays empty.
        Op::Del { .. }
        | Op::DelBlk { .. }
        | Op::Cut { .. }
        | Op::CutBlk { .. }
        | Op::Paste { .. } => {}
    }

    let _ = recovered_lines;
    Ok(())
}

/// Display-only metadata emitted by `read` — never source content.
fn is_read_metadata(trimmed: &str) -> bool {
    if trimmed == "..." {
        return true;
    }
    if trimmed.starts_with("[Showing lines ")
        && (trimmed.contains(" of ") || trimmed.ends_with(']'))
    {
        return true;
    }
    if trimmed.starts_with("[read: ") && trimmed.contains("output truncated]") {
        return true;
    }
    if trimmed.starts_with("[continue with ") && trimmed.ends_with(']') {
        return true;
    }
    false
}

/// `+TEXT` → `TEXT`; `+` alone → `""`; `+-x`/`++x` → `-x`/`+x`. Non-`+` lines
/// return `None` (not a body row).
fn strip_body_prefix(line: &str) -> Option<String> {
    let rest = line.strip_prefix('+')?;
    if rest.starts_with('-') || rest.starts_with('+') {
        return Some(rest.to_string());
    }
    Some(rest.to_string())
}

/// A parsed op header: either body-bearing (needs `+` rows), bodyless, or a
/// file-level operation.
#[derive(Debug)]
enum ParsedOp {
    BodyBearer(Op),
    Bodyless(Op),
    File(FileOp),
}

/// True when a line begins with a hashline directive keyword, so a malformed
/// occurrence inside a body is reported as an op-header error rather than
/// recovered as literal content.
fn looks_like_op_header(trimmed: &str) -> bool {
    trimmed.starts_with("SWAP ")
        || trimmed.starts_with("SWAP.BLK ")
        || trimmed.starts_with("DEL ")
        || trimmed.starts_with("DEL.BLK ")
        || trimmed.starts_with("CUT ")
        || trimmed.starts_with("CUT.BLK ")
        || trimmed.starts_with("INS.")
        || trimmed.starts_with("PASTE.")
        || trimmed.starts_with("MV ")
        || trimmed == "REM"
}

fn parse_op_header(line: &str) -> Result<ParsedOp, String> {
    // SWAP.BLK N:
    if let Some(rest) = line.strip_prefix("SWAP.BLK ") {
        let (n, tail) = parse_lid(rest)?;
        expect_colon(tail)?;
        return Ok(ParsedOp::BodyBearer(Op::SwapBlk {
            start: n,
            body: Vec::new(),
        }));
    }
    // DEL.BLK N
    if let Some(rest) = line.strip_prefix("DEL.BLK ") {
        let (n, tail) = parse_lid(rest)?;
        expect_bodyless(tail)?;
        return Ok(ParsedOp::Bodyless(Op::DelBlk { start: n }));
    }
    // INS.BLK.POST N:
    if let Some(rest) = line.strip_prefix("INS.BLK.POST ") {
        let (n, tail) = parse_lid(rest)?;
        expect_colon(tail)?;
        return Ok(ParsedOp::BodyBearer(Op::InsBlkPost {
            anchor: n,
            body: Vec::new(),
        }));
    }
    // SWAP N.=M: / SWAP N:
    if let Some(rest) = line.strip_prefix("SWAP ") {
        let (start, end, tail) = parse_range(rest)?;
        expect_colon(tail)?;
        return Ok(ParsedOp::BodyBearer(Op::Swap {
            start,
            end,
            body: Vec::new(),
        }));
    }
    // DEL N.=M / DEL N
    if let Some(rest) = line.strip_prefix("DEL ") {
        let (start, end, tail) = parse_range(rest)?;
        expect_bodyless(tail)?;
        return Ok(ParsedOp::Bodyless(Op::Del { start, end }));
    }
    // CUT.BLK N
    if let Some(rest) = line.strip_prefix("CUT.BLK ") {
        let (n, tail) = parse_lid(rest)?;
        expect_bodyless(tail)?;
        return Ok(ParsedOp::Bodyless(Op::CutBlk { start: n }));
    }
    // CUT N.=M / CUT N
    if let Some(rest) = line.strip_prefix("CUT ") {
        let (start, end, tail) = parse_range(rest)?;
        expect_bodyless(tail)?;
        return Ok(ParsedOp::Bodyless(Op::Cut { start, end }));
    }
    // PASTE.PRE N: / PASTE.POST N: / PASTE.HEAD: / PASTE.TAIL:
    if let Some(rest) = line.strip_prefix("PASTE.") {
        if let Some(rest) = rest.strip_prefix("PRE ") {
            let (n, tail) = parse_lid(rest)?;
            expect_colon(tail)?;
            return Ok(ParsedOp::Bodyless(Op::Paste {
                pos: InsPos::Pre,
                anchor: Some(n),
            }));
        }
        if let Some(rest) = rest.strip_prefix("POST ") {
            let (n, tail) = parse_lid(rest)?;
            expect_colon(tail)?;
            return Ok(ParsedOp::Bodyless(Op::Paste {
                pos: InsPos::Post,
                anchor: Some(n),
            }));
        }
        if let Some(rest) = rest.strip_prefix("HEAD") {
            expect_colon(rest)?;
            return Ok(ParsedOp::Bodyless(Op::Paste {
                pos: InsPos::Head,
                anchor: None,
            }));
        }
        if let Some(rest) = rest.strip_prefix("TAIL") {
            expect_colon(rest)?;
            return Ok(ParsedOp::Bodyless(Op::Paste {
                pos: InsPos::Tail,
                anchor: None,
            }));
        }
        return Err("unknown PASTE position (expected PRE/POST/HEAD/TAIL)".to_string());
    }
    // REM — delete the file
    if line.trim() == "REM" {
        return Ok(ParsedOp::File(FileOp::Rem));
    }
    // MV DEST — move the file
    if let Some(rest) = line.strip_prefix("MV ") {
        let dest = rest.trim();
        if dest.is_empty() {
            return Err("MV requires a destination path".to_string());
        }
        return Ok(ParsedOp::File(FileOp::Move {
            dest: dest.to_string(),
        }));
    }
    if let Some(rest) = line.strip_prefix("INS.") {
        if let Some(rest) = rest.strip_prefix("PRE ") {
            let (n, tail) = parse_lid(rest)?;
            expect_colon(tail)?;
            return Ok(ParsedOp::BodyBearer(Op::Ins {
                pos: InsPos::Pre,
                anchor: Some(n),
                body: Vec::new(),
            }));
        }
        if let Some(rest) = rest.strip_prefix("POST ") {
            let (n, tail) = parse_lid(rest)?;
            expect_colon(tail)?;
            return Ok(ParsedOp::BodyBearer(Op::Ins {
                pos: InsPos::Post,
                anchor: Some(n),
                body: Vec::new(),
            }));
        }
        if let Some(rest) = rest.strip_prefix("HEAD") {
            expect_colon(rest)?;
            return Ok(ParsedOp::BodyBearer(Op::Ins {
                pos: InsPos::Head,
                anchor: None,
                body: Vec::new(),
            }));
        }
        if let Some(rest) = rest.strip_prefix("TAIL") {
            expect_colon(rest)?;
            return Ok(ParsedOp::BodyBearer(Op::Ins {
                pos: InsPos::Tail,
                anchor: None,
                body: Vec::new(),
            }));
        }
        return Err("unknown INS position (expected PRE/POST/HEAD/TAIL)".to_string());
    }
    Err(format!(
        "unrecognized op header: {line:?}. \
         Hint: body rows (the content to insert/replace) must start with `+` (e.g. `+new content`); \
         a lone `+` on a line denotes an empty line, and `+-x`/`++x` escape literal `-`/`+`. \
         A `- `-prefixed Markdown bullet inside a body is kept as `+- item`; a bare `-old` row \
         is discarded when `+` rows are present. \
         If you did mean to write an op header, valid directives are: `SWAP N.=M:` / `DEL N.=M` / \
         `CUT N.=M` / `PASTE.PRE N:` / `PASTE.POST N:` / `PASTE.HEAD:` / `PASTE.TAIL:` / \
         `INS.PRE N:` / `INS.POST N:` / `INS.HEAD:` / `INS.TAIL:` / `SWAP.BLK N:` / `DEL.BLK N` / \
         `CUT.BLK N` / `INS.BLK.POST N:` / `REM` / `MV DEST`."
    ))
}

/// Parse `[PATH#TAG]` → `FilePatch` with empty ops. Returns `None` if not a
/// section header.
fn parse_section_header(line: &str) -> Option<FilePatch> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    let hash_sep = inner.rfind('#')?;
    let (path_part, tag) = inner.split_at(hash_sep);
    let tag = &tag[1..]; // skip '#'
    if !is_valid_tag(tag) {
        return None;
    }
    let path = unquote_path(path_part);
    Some(FilePatch {
        path: PathBuf::from(path),
        tag: tag.to_string(),
        ops: Vec::new(),
        file_op: None,
    })
}

fn is_valid_tag(tag: &str) -> bool {
    tag.len() == 6 && tag.chars().all(|c| matches!(c, '0'..='9' | 'A'..='F'))
}

fn unquote_path(s: &str) -> &str {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// A bare range header at top level: `N- M:` / `N.=M:` / `N M:` with a
/// trailing colon. Returns `(start, end)` when the whole trimmed line is a
/// range terminator.
fn parse_bare_range_header(trimmed: &str) -> Option<(usize, usize)> {
    let t = trimmed.strip_suffix(':')?.trim();
    let bytes = t.as_bytes();
    let mut i = 0;
    let start = read_uint(bytes, &mut i)?;
    let sep_start = i;
    while i < bytes.len()
        && (bytes[i] == b' ' || bytes[i] == b'-' || bytes[i] == b'.' || bytes[i] == b'=')
    {
        i += 1;
    }
    // Accept an ellipsis separator too.
    if i == sep_start && t[i..].starts_with('…') {
        i += '…'.len_utf8();
    }
    if i == sep_start {
        return None;
    }
    let end = read_uint(bytes, &mut i)?;
    if i != bytes.len() {
        return None;
    }
    if end < start {
        return None;
    }
    Some((start, end))
}

/// A top-level pasted read row: `N:TEXT`. Returns `(line, text)`.
fn parse_snapshot_row(trimmed: &str) -> Option<(usize, &str)> {
    let bytes = trimmed.as_bytes();
    let mut i = 0;
    let n = read_uint(bytes, &mut i)?;
    if i < bytes.len() && bytes[i] == b':' {
        return Some((n, &trimmed[i + 1..]));
    }
    None
}

/// Read an unsigned integer from `bytes` starting at `*i`, advancing `*i`.
fn read_uint(bytes: &[u8], i: &mut usize) -> Option<usize> {
    let start = *i;
    while *i < bytes.len() && bytes[*i].is_ascii_digit() {
        *i += 1;
    }
    if start == *i {
        return None;
    }
    let text = std::str::from_utf8(&bytes[start..*i]).ok()?;
    text.parse().ok()
}

/// Parse a `N.=M` range (or bare `N`), returning `(start, end, remaining)`.
///
/// The range separator is deliberately lenient for model output: `.=`, `-`,
/// `=`, `.`, `..`, `…`, mixed runs, and whitespace-only separators all recover
/// to the same range. A dangling separator (no end number before the `:` or end
/// of line) collapses to `N.=N`.
fn parse_range(s: &str) -> Result<(usize, usize, &str), String> {
    let (start, rest) = parse_lid(s)?;
    let bytes = rest.as_bytes();
    let mut i = 0;
    let mut saw_sep_char = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b' ' || b == b'\t' || b == b'-' || b == b'.' || b == b'=' {
            if b != b' ' && b != b'\t' {
                saw_sep_char = true;
            }
            i += 1;
            continue;
        }
        if rest[i..].starts_with('…') {
            saw_sep_char = true;
            i += '…'.len_utf8();
            continue;
        }
        break;
    }
    // An end number follows the separator run → real range.
    if i < bytes.len() && bytes[i].is_ascii_digit() && bytes[i] != b'0' {
        let (end, tail) = parse_lid(&rest[i..])?;
        if end < start {
            return Err(format!("range end {end} is less than start {start}"));
        }
        return Ok((start, end, tail));
    }
    // Dangling separator before `:` or end-of-line: the model meant `N.=N`.
    if saw_sep_char && (i == bytes.len() || bytes[i] == b':') {
        return Ok((start, start, &rest[i..]));
    }
    // No separator: single line.
    Ok((start, start, rest))
}

/// Parse a 1-indexed line id (`[1-9]\d*`), returning `(n, remaining)`.
fn parse_lid(s: &str) -> Result<(usize, &str), String> {
    let s = s.strip_prefix(' ').unwrap_or(s);
    let mut end = 0;
    let bytes = s.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_digit() || bytes[0] == b'0' {
        return Err(format!(
            "expected a line number (non-zero digit prefix): {s:?}"
        ));
    }
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let n: usize = s[..end]
        .parse()
        .map_err(|_| format!("line number overflow: {s:?}"))?;
    Ok((n, &s[end..]))
}

fn expect_colon(s: &str) -> Result<(), String> {
    let s = s.trim();
    if s == ":" {
        Ok(())
    } else {
        Err(format!(
            "expected `:` terminator, got {s:?}. Body-bearing directives end with `:`: `SWAP N.=M:` / `SWAP.BLK N:` / `INS.PRE N:` / `INS.POST N:` / `INS.HEAD:` / `INS.TAIL:` / `INS.BLK.POST N:`; body-less `DEL`/`DEL.BLK` have no colon (e.g. `DEL N.=M` / `DEL.BLK N`). If you wrote `N:M` you most likely mistyped `N.=M:`."
        ))
    }
}

/// Body-less directives end at end-of-line; a stray trailing `:` is tolerated.
fn expect_bodyless(s: &str) -> Result<(), String> {
    let s = s.trim();
    if s.is_empty() || s == ":" {
        if s == ":" {
            // Caller appends WARN_BODYLESS_COLON where warnings are visible.
        }
        Ok(())
    } else {
        Err(format!("expected end of line, got {:?}", s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op_at(files: &[FilePatch], i: usize) -> &Op {
        &files[0].ops[i]
    }

    fn parse_ok(patch: &str) -> ParsedPatch {
        parse_patch(patch).unwrap()
    }

    #[test]
    fn parses_swap_range() {
        let p = parse_ok("[a.rs#1A2B3C]\nSWAP 2.=3:\n+x\n+y");
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 2,
                end: 3,
                body: vec!["x".into(), "y".into()]
            }
        );
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    #[test]
    fn parses_swap_single_as_range() {
        let p = parse_ok("[a.rs#1A2B3C]\nSWAP 5:\n+z");
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 5,
                end: 5,
                body: vec!["z".into()]
            }
        );
    }

    #[test]
    fn parses_del_range_and_single() {
        let p = parse_ok("[a.rs#1A2B3C]\nDEL 2.=4\nDEL 7");
        assert_eq!(op_at(&p.files, 0), &Op::Del { start: 2, end: 4 });
        assert_eq!(op_at(&p.files, 1), &Op::Del { start: 7, end: 7 });
    }

    #[test]
    fn parses_ins_variants() {
        let p = parse_ok(
            "[a.rs#1A2B3C]\nINS.PRE 2:\n+x\nINS.POST 3:\n+y\nINS.HEAD:\n+h\nINS.TAIL:\n+t",
        );
        assert!(matches!(
            op_at(&p.files, 0),
            Op::Ins {
                pos: InsPos::Pre,
                anchor: Some(2),
                ..
            }
        ));
        assert!(matches!(
            op_at(&p.files, 1),
            Op::Ins {
                pos: InsPos::Post,
                anchor: Some(3),
                ..
            }
        ));
        assert!(matches!(
            op_at(&p.files, 2),
            Op::Ins {
                pos: InsPos::Head,
                anchor: None,
                ..
            }
        ));
        assert!(matches!(
            op_at(&p.files, 3),
            Op::Ins {
                pos: InsPos::Tail,
                anchor: None,
                ..
            }
        ));
    }

    #[test]
    fn parses_block_ops() {
        let p = parse_ok("[a.rs#1A2B3C]\nSWAP.BLK 1:\n+x\nDEL.BLK 2\nINS.BLK.POST 3:\n+y");
        assert!(matches!(op_at(&p.files, 0), Op::SwapBlk { start: 1, .. }));
        assert!(matches!(op_at(&p.files, 1), Op::DelBlk { start: 2 }));
        assert!(matches!(
            op_at(&p.files, 2),
            Op::InsBlkPost { anchor: 3, .. }
        ));
    }

    #[test]
    fn body_blank_and_escape() {
        let p = parse_ok("[a.rs#1A2B3C]\nSWAP 1.=1:\n+\n+-x\n++y");
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 1,
                end: 1,
                body: vec!["".into(), "-x".into(), "+y".into()]
            }
        );
    }

    #[test]
    fn body_preserves_leading_whitespace() {
        let p = parse_ok("[a.rs#1A2B3C]\nSWAP 1.=1:\n+    code");
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 1,
                end: 1,
                body: vec!["    code".into()]
            }
        );
    }

    #[test]
    fn envelope_optional() {
        let with = parse_ok("*** Begin Patch\n[a.rs#1A2B3C]\nDEL 1\n*** End Patch");
        let without = parse_ok("[a.rs#1A2B3C]\nDEL 1");
        assert_eq!(with.files, without.files);
    }

    #[test]
    fn multiple_sections() {
        let p = parse_ok("[a.rs#1A2B3C]\nDEL 1\n[b.rs#3C4D5E]\nSWAP 1.=1:\n+x");
        assert_eq!(p.files.len(), 2);
        assert_eq!(p.files[0].path, PathBuf::from("a.rs"));
        assert_eq!(p.files[1].path, PathBuf::from("b.rs"));
    }

    #[test]
    fn error_on_bad_tag() {
        let e = parse_patch("[a.rs#bad]\nDEL 1").unwrap_err();
        assert!(e.message.contains("snapshot tag"), "{}", e.message);
    }

    #[test]
    fn error_on_zero_line() {
        assert!(parse_patch("[a.rs#1A2B3C]\nDEL 0").is_err());
    }

    #[test]
    fn error_on_unrecognized_header_top_level() {
        let e = parse_patch("[a.rs#1A2B3C]\nFROB 1").unwrap_err();
        assert!(e.message.contains("unrecognized"), "{}", e.message);
    }

    // ── Tolerance layer ────────────────────────────────────────────────────

    #[test]
    fn bare_body_row_auto_piped() {
        // `}` without `+` inside a body is recovered as literal content.
        let p = parse_ok("[a.rs#1A2B3C]\nSWAP 1.=1:\nfn f() {\n}");
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 1,
                end: 1,
                body: vec!["fn f() {".into(), "}".into()]
            }
        );
        assert!(
            p.warnings.iter().any(|w| w == WARN_BARE_BODY),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn minus_bullet_kept_as_content() {
        let p = parse_ok("[a.md#1A2B3C]\nSWAP 1.=1:\n- task\n  - nested");
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 1,
                end: 1,
                body: vec!["- task".into(), "  - nested".into()]
            }
        );
        assert!(
            p.warnings.iter().any(|w| w == WARN_MINUS_BULLET),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn diff_old_rows_dropped_when_plus_present() {
        let p = parse_ok("[a.rs#1A2B3C]\nSWAP 2.=2:\n-old();\n+new();");
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 2,
                end: 2,
                body: vec!["new();".into()]
            }
        );
        assert!(
            p.warnings.iter().any(|w| w == WARN_DIFF_OLD_ROWS),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn minus_only_non_bullet_rejected() {
        assert!(parse_patch("[a.rs#1A2B3C]\nSWAP 2.=2:\n-    old_code();").is_err());
    }

    #[test]
    fn top_level_snapshot_rows_become_swaps() {
        let p = parse_ok("[a.rs#1A2B3C]\n1:fn main() {}\n2:fn other() {}");
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 1,
                end: 1,
                body: vec!["fn main() {}".into()]
            }
        );
        assert_eq!(
            op_at(&p.files, 1),
            &Op::Swap {
                start: 2,
                end: 2,
                body: vec!["fn other() {}".into()]
            }
        );
        assert!(
            p.warnings.iter().any(|w| w == WARN_SNAPSHOT_ROWS),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn repeated_snapshot_row_rejected() {
        assert!(parse_patch("[a.rs#1A2B3C]\n5:a\n5:b").is_err());
    }

    #[test]
    fn bare_range_header_recovered() {
        let p = parse_ok("[a.rs#1A2B3C]\n2.=4:\n+x\n+y");
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 2,
                end: 4,
                body: vec!["x".into(), "y".into()]
            }
        );
        assert!(
            p.warnings.iter().any(|w| w == WARN_BARE_RANGE),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn uniform_prefixes_stripped() {
        let p = parse_ok("[a.rs#1A2B3C]\nSWAP 1.=2:\n1:fn a() {}\n2:fn b() {}");
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 1,
                end: 2,
                body: vec!["fn a() {}".into(), "fn b() {}".into()]
            }
        );
        assert!(
            p.warnings.iter().any(|w| w == WARN_BARE_PREFIX_STRIP),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn dict_shaped_prefixes_not_stripped() {
        // Every remainder is a lone numeric literal → the prefixes are content.
        let p = parse_ok("[a.yaml#1A2B3C]\nSWAP 1.=2:\n8080: 8080\n8081: 8081");
        // Both rows carry `NNN:` but the remainders are numeric, so stripping
        // would corrupt a port dict; the prefixes stay.
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 1,
                end: 2,
                body: vec!["8080: 8080".into(), "8081: 8081".into()]
            }
        );
    }

    #[test]
    fn empty_swap_becomes_del() {
        let p = parse_ok("[a.rs#1A2B3C]\nSWAP 3.=5:");
        assert_eq!(op_at(&p.files, 0), &Op::Del { start: 3, end: 5 });
        assert!(
            p.warnings.iter().any(|w| w == WARN_EMPTY_SWAP),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn del_trailing_colon_tolerated() {
        let p = parse_ok("[a.rs#1A2B3C]\nDEL 2.=4:");
        assert_eq!(op_at(&p.files, 0), &Op::Del { start: 2, end: 4 });
    }

    #[test]
    fn lenient_range_separators() {
        for patch in [
            "[a.rs#1A2B3C]\nSWAP 2-4:\n+x",
            "[a.rs#1A2B3C]\nSWAP 2..4:\n+x",
            "[a.rs#1A2B3C]\nSWAP 2 4:\n+x",
        ] {
            let p = parse_ok(patch);
            assert_eq!(
                op_at(&p.files, 0),
                &Op::Swap {
                    start: 2,
                    end: 4,
                    body: vec!["x".into()]
                },
                "patch: {patch}"
            );
        }
    }

    #[test]
    fn dangling_separator_collapses_to_single_line() {
        let p = parse_ok("[a.rs#1A2B3C]\nSWAP 5-:\n+x");
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 5,
                end: 5,
                body: vec!["x".into()]
            }
        );
    }

    #[test]
    fn blank_lines_inside_body_preserved() {
        let p = parse_ok("[a.rs#1A2B3C]\nSWAP 1.=1:\n+a\n\n+b");
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 1,
                end: 1,
                body: vec!["a".into(), "".into(), "b".into()]
            }
        );
    }

    #[test]
    fn read_metadata_ignored() {
        let p =
            parse_ok("[a.rs#1A2B3C]\nDEL 1\n[Showing lines 1-2000 of 3000. Page through the rest]");
        assert_eq!(op_at(&p.files, 0), &Op::Del { start: 1, end: 1 });
        assert!(
            p.warnings.iter().any(|w| w == WARN_READ_METADATA),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn gap_marker_ignored() {
        let p = parse_ok("[a.rs#1A2B3C]\nDEL 1\n...");
        assert_eq!(op_at(&p.files, 0), &Op::Del { start: 1, end: 1 });
    }

    #[test]
    fn duplicate_swap_range_coalesced() {
        let p = parse_ok("[a.rs#1A2B3C]\nSWAP 2.=2:\n+old\nSWAP 2.=2:\n+new");
        // Only the LAST body survives; the earlier one is dropped.
        assert_eq!(
            op_at(&p.files, 0),
            &Op::Swap {
                start: 2,
                end: 2,
                body: vec!["new".into()]
            }
        );
        assert!(
            p.warnings.iter().any(|w| w == WARN_PAIR_COALESCED),
            "{:?}",
            p.warnings
        );
    }

    #[test]
    fn apply_patch_contamination_rejected() {
        let e = parse_patch("[a.rs#1A2B3C]\n*** Update File: b.rs").unwrap_err();
        assert!(e.message.contains("apply_patch"), "{}", e.message);
    }

    #[test]
    fn empty_ins_body_rejected() {
        assert!(parse_patch("[a.rs#1A2B3C]\nINS.PRE 2:").is_err());
    }

    #[test]
    fn malformed_op_header_inside_body_still_errors() {
        // A `SWAP` typo inside a body is an attempted header, not bare content.
        assert!(parse_patch("[a.rs#1A2B3C]\nSWAP 1.=1:\n+x\nSWAP 2:=3:\n+y").is_err());
    }
}
