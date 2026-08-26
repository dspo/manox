//! Hashline editing: line-anchored patches validated by content-hash tags.
//!
//! `read` mints a 6-hex tag from the file's normalized text and records a
//! snapshot; `edit` parses a patch of `SWAP`/`DEL`/`INS` ops anchored on
//! the ORIGINAL line numbers, validates the tag still matches the live file,
//! and applies the ops back-to-front. On a stale tag, a 3-way merge replays the
//! resolved changes onto the current content by content-anchoring the snapshot
//! ranges. The model never has to reproduce the original text verbatim — only
//! the line numbers and a fresh tag.
//!
//! Snapshots live in a caller-owned [`SnapshotStore`] (carried by
//! `tool::ToolState`); this module holds no global state.

pub mod apply;
pub mod block;
pub mod hash;
pub mod parser;
pub mod recovery;
pub mod snapshot;

#[cfg(test)]
mod integration_tests;

pub use apply::{ApplyError, ApplyResult, apply};
pub use block::BlockError;
pub use hash::compute_tag;
pub use parser::{FileOp, FilePatch, InsPos, Op, ParseError, ParsedPatch, parse_patch};
pub use recovery::{RecoverError, try_recover, try_recover_with_snapshot};
pub use snapshot::{Snapshot, SnapshotStore, canonical_path};

use std::collections::HashSet;

/// Upper bound on the normalized text a producer snapshots for hashline. A
/// section tag fingerprints the WHOLE file, so tagging means holding the full
/// text in the store. Files above this cap are served without a `[path#tag]`
/// header — line-anchored editing is out of scope for them (use `Write`).
pub const SNAPSHOT_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Record a read snapshot for `path` under the [`SNAPSHOT_MAX_BYTES`] cap.
/// Returns the recorded snapshot, or `None` when the file is too large to
/// snapshot — callers then omit the `[path#tag]` header so the model never
/// anchors against a tag the store cannot resolve.
pub fn record_read_snapshot(
    store: &mut snapshot::SnapshotStore,
    path: &std::path::Path,
    text: &str,
) -> Option<snapshot::Snapshot> {
    if text.len() > SNAPSHOT_MAX_BYTES {
        return None;
    }
    Some(store.record(path, text))
}

/// Parse the 1-indexed line numbers a numbered hashline body actually
/// displayed. Only rows whose `digits:`-prefixed prefix parse contribute —
/// headers, `...` gap markers, footers, and free text never match, and
/// optional leading marker characters (`>` match / ` ` context, as grep
/// emits) are tolerated. Parse the OUTPUT after truncation so the
/// provenance reflects exactly what reached the model.
pub fn parse_seen_lines_from_body(body: &str) -> HashSet<usize> {
    let mut seen = HashSet::new();
    for line in body.lines() {
        // Tolerate leading marker characters (grep's `>` / ` ` prefix).
        let stripped = line.trim_start_matches(['>', ' ']);
        let Some(colon) = stripped.find(':') else {
            continue;
        };
        let prefix = &stripped[..colon];
        if prefix.is_empty()
            || prefix.starts_with('0')
            || !prefix.bytes().all(|b| b.is_ascii_digit())
        {
            continue;
        }
        if let Ok(n) = prefix.parse::<usize>() {
            seen.insert(n);
        }
    }
    seen
}

/// Compress a line list into a sorted `1-4, 7, 10-12` range string.
pub fn format_line_ranges(lines: &[usize]) -> String {
    let mut sorted: Vec<usize> = lines.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut start = sorted[0];
    let mut prev = sorted[0];
    for &cur in sorted.iter().skip(1) {
        if cur == prev + 1 {
            prev = cur;
            continue;
        }
        parts.push(if start == prev {
            start.to_string()
        } else {
            format!("{start}-{prev}")
        });
        start = cur;
        prev = cur;
    }
    parts.push(if start == prev {
        start.to_string()
    } else {
        format!("{start}-{prev}")
    });
    parts.join(", ")
}

/// Upper bound on unseen anchor lines whose content is inlined into a
/// seen-line rejection. Big enough for the common "edit a whole function
/// body" retry, small enough to keep the error human-readable.
pub const SEEN_LINE_REVEAL_CAP: usize = 40;
/// Per-revealed-line character cap so a minified megabyte line can never
/// dump into the error message. Lines over the cap are trimmed and the
/// whole reveal is flagged truncated.
pub const SEEN_LINE_REVEAL_MAX_COLUMNS: usize = 512;

/// The seen-line gate: reject anchored edits on lines the read that minted
/// the cited tag never displayed. When the reveal covers every unseen
/// anchor in full width, those lines merge into the snapshot's `seen_lines`
/// and the message invites a straight retry with the same `[path#tag]`
/// header; when truncated, the range re-read guidance stays intact. Only
/// runs on the no-drift path — on recovery the line numbers shift, so
/// provenance does not index the live content 1:1. Returns `Ok(())` when
/// no snapshot carries provenance or every anchor was displayed.
pub fn assert_seen_lines(
    store: &mut snapshot::SnapshotStore,
    path: &std::path::Path,
    tag: &str,
    ops: &[parser::Op],
    current: &str,
) -> Result<(), String> {
    let seen_count = store
        .get(path, tag)
        .and_then(|s| s.seen_lines.as_ref())
        .map(|seen| if seen.is_empty() { 0 } else { seen.len() })
        .unwrap_or(0);
    if seen_count == 0 {
        // Absent or empty provenance: the tag was externally minted or the
        // snapshot aged out. Apply as before.
        return Ok(());
    }

    let mut unseen: Vec<usize> = Vec::new();
    for op in ops {
        match op {
            parser::Op::Swap { start, end, .. }
            | parser::Op::Del { start, end }
            | parser::Op::Cut { start, end } => {
                for line in *start..=*end {
                    unseen.push(line);
                }
            }
            parser::Op::Ins {
                anchor: Some(a), ..
            }
            | parser::Op::Paste {
                anchor: Some(a), ..
            } => unseen.push(*a),
            parser::Op::SwapBlk { start, .. }
            | parser::Op::DelBlk { start }
            | parser::Op::InsBlkPost { anchor: start, .. }
            | parser::Op::CutBlk { start } => unseen.push(*start),
            parser::Op::Ins { anchor: None, .. } | parser::Op::Paste { anchor: None, .. } => {}
        }
    }
    let seen_ref = &store
        .get(path, tag)
        .and_then(|s| s.seen_lines.clone())
        .unwrap_or_default();
    unseen.retain(|line| !seen_ref.contains(line));
    if unseen.is_empty() {
        return Ok(());
    }
    unseen.sort_unstable();
    unseen.dedup();

    let path_display = path.display().to_string();
    let ranges = format_line_ranges(&unseen);
    let source_lines: Vec<&str> = current.lines().collect();

    let reveal_count = unseen.len().min(SEEN_LINE_REVEAL_CAP);
    let mut revealed: Vec<(usize, String)> = Vec::new();
    let mut column_truncated = false;
    for &line in &unseen[..reveal_count] {
        // Out-of-range anchors are caught by apply with a better message;
        // skip them so they never join the revealed set.
        if line < 1 || line > source_lines.len() {
            continue;
        }
        let source = source_lines[line - 1];
        if source.len() > SEEN_LINE_REVEAL_MAX_COLUMNS {
            let end = source
                .char_indices()
                .nth(SEEN_LINE_REVEAL_MAX_COLUMNS)
                .map(|(i, _)| i)
                .unwrap_or(source.len());
            revealed.push((line, format!("{}…", &source[..end])));
            column_truncated = true;
        } else {
            revealed.push((line, source.to_string()));
        }
    }
    let truncated = unseen.len() > reveal_count || column_truncated;
    // Only merge when the reveal covered every unseen anchor in full width:
    // a truncated reveal would let the model split a blind edit into
    // <=cap-line retries and land it without the required range re-read.
    if !truncated
        && let Some(snap) = store.get_mut(path, tag)
        && let Some(seen) = &mut snap.seen_lines
    {
        let revealed_lines: HashSet<usize> = revealed.iter().map(|(l, _)| *l).collect();
        seen.extend(&revealed_lines);
    }

    let header = format!(
        "This edit anchors to lines {ranges} of {path_display} that \
         [{path_display}#{tag}] never displayed (it showed a partial range, \
         a search hit, or a folded summary)."
    );
    let selector = ranges.replace(", ", ",");
    let preview: String = revealed
        .iter()
        .map(|(line, text)| format!("  {line}:{text}"))
        .collect::<Vec<_>>()
        .join("\n");
    if revealed.is_empty() {
        return Err(format!(
            "{header} Re-read them in full first with a ranged read like \
             `{path_display}:{selector}`, then re-issue the edit."
        ));
    }
    if truncated {
        return Err(format!(
            "{header} Preview of the actual file content at the first {} unseen line(s):\n{preview}\n\
             The range exceeds the inline preview cap — re-read the remainder with \
             `{path_display}:{selector}` before re-issuing the edit.",
            revealed.len()
        ));
    }
    Err(format!(
        "{header} Actual file content at those lines:\n{preview}\n\
         Verify the content matches what you intend to touch, then re-issue the edit with the \
         same [{path_display}#{tag}] header — a straight retry now succeeds without a re-read. \
         If the content does NOT match, fix your line numbers."
    ))
}

/// Context lines rendered on each side of an anchor in rejection diagnostics.
const ANCHOR_CONTEXT_LINES: usize = 2;
/// Upper bound on total `N:TEXT` rows a single rejection diagnostic renders.
const ANCHOR_CONTEXT_MAX_ROWS: usize = 40;

/// Collect every 1-indexed anchor line the ops reference, in order. Shared by
/// the 3-way recovery path and the mismatch diagnostics so both report the
/// same positions.
pub fn collect_anchor_lines(ops: &[parser::Op]) -> Vec<usize> {
    let mut lines = Vec::new();
    for op in ops {
        match op {
            parser::Op::Swap { start, end, .. }
            | parser::Op::Del { start, end }
            | parser::Op::Cut { start, end } => {
                for l in *start..=*end {
                    lines.push(l);
                }
            }
            parser::Op::Ins {
                anchor: Some(a), ..
            }
            | parser::Op::Paste {
                anchor: Some(a), ..
            } => lines.push(*a),
            parser::Op::SwapBlk { start, .. }
            | parser::Op::DelBlk { start }
            | parser::Op::CutBlk { start }
            | parser::Op::InsBlkPost { anchor: start, .. } => lines.push(*start),
            parser::Op::Ins { anchor: None, .. } | parser::Op::Paste { anchor: None, .. } => {}
        }
    }
    lines
}

/// Render the live file's content around an edit's anchor lines as `N:TEXT`
/// rows — windows of ±2 lines, merged where they overlap, gaps marked `...` —
/// so a rejected edit shows the model what actually sits at the positions it
/// claimed. Returns an empty string when there are no anchors or no lines to
/// show. Anchors beyond the file are clamped into it.
pub fn anchored_context(ops: &[parser::Op], current: &str) -> String {
    let total = current.lines().count();
    if total == 0 {
        return String::new();
    }
    let anchors = collect_anchor_lines(ops);
    if anchors.is_empty() {
        return String::new();
    }
    let mut anchors = anchors;
    anchors.sort_unstable();
    anchors.dedup();
    // Merge each anchor's ±2 window into contiguous 1-indexed ranges. Anchors
    // past EOF clamp into the file so no window is empty.
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for a in anchors {
        let a = a.min(total);
        let start = a.saturating_sub(ANCHOR_CONTEXT_LINES).max(1);
        let end = (a + ANCHOR_CONTEXT_LINES).min(total);
        if let Some(last) = windows.last_mut()
            && start <= last.1 + 1
        {
            last.1 = last.1.max(end);
        } else {
            windows.push((start, end));
        }
    }
    let lines: Vec<&str> = current.lines().collect();
    let mut rows: Vec<String> = Vec::new();
    let mut truncated = false;
    'outer: for (wi, &(wstart, wend)) in windows.iter().enumerate() {
        if wi > 0 {
            rows.push("...".to_string());
        }
        for line_no in wstart..=wend {
            rows.push(format!("{line_no}:{}", lines[line_no - 1]));
            if rows.len() >= ANCHOR_CONTEXT_MAX_ROWS {
                truncated = true;
                break 'outer;
            }
        }
    }
    if truncated {
        rows.push("... (context truncated)".to_string());
    }
    rows.join("\n")
}

/// A 1-indexed inclusive line range for partial display. `end: None` extends
/// to the end of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub start: usize,
    pub end: Option<usize>,
}

/// Normalize raw file bytes for hashing and line-number display: strip a leading
/// BOM and convert CRLF to LF. Trailing-whitespace normalization for the hash
/// happens inside [`hash::compute_tag`].
pub fn normalize_to_lf(raw: &str) -> String {
    raw.strip_prefix('\u{feff}')
        .unwrap_or(raw)
        .replace("\r\n", "\n")
        .replace('\r', "\n")
}

/// Detect whether `raw` used CRLF line endings, so an edit can restore them on
/// write instead of silently flattening to LF.
pub fn detect_crlf(raw: &str) -> bool {
    raw.contains("\r\n")
}

/// Detect whether `raw` began with a UTF-8 BOM.
pub fn has_bom(raw: &str) -> bool {
    raw.starts_with('\u{feff}')
}

/// Format a file for `read` output: an optional `[path#TAG]` header followed by
/// `N:TEXT` numbered lines. The caller is responsible for recording the snapshot
/// before formatting so the tag is stable. Pass `tag: None` to omit the header
/// (files over [`SNAPSHOT_MAX_BYTES`] carry no tag, so no header).
pub fn format_numbered(path: &str, text: &str, tag: Option<&str>) -> String {
    let mut out = String::with_capacity(text.len() + path.len() + 16);
    if let Some(tag) = tag {
        out.push('[');
        out.push_str(path);
        out.push('#');
        out.push_str(tag);
        out.push(']');
        out.push('\n');
    }
    for (i, line) in text.lines().enumerate() {
        use std::fmt::Write as _;
        let _ = write!(out, "{}:{}", i + 1, line);
        out.push('\n');
    }
    // Trim the trailing newline so the output matches the conventional shape.
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Leading context lines added before an explicit range start.
const RANGE_LEADING_CONTEXT: usize = 1;
/// Trailing context lines added after an explicit range end.
const RANGE_TRAILING_CONTEXT: usize = 3;

/// Format a subset of a file with a `[path#TAG]` header and `N:TEXT` numbered
/// lines. Lines outside the requested ranges are elided, with `...` markers
/// between gaps. Context lines (1 leading + 3 trailing) surround explicit
/// ranges so the model sees surrounding structure.
///
/// Snapshot is always computed from the full file text — this function only
/// controls display. An empty `ranges` slice falls back to [`format_numbered`].
pub fn format_numbered_range(
    path: &str,
    text: &str,
    tag: Option<&str>,
    ranges: &[LineRange],
) -> String {
    if ranges.is_empty() {
        return format_numbered(path, text, tag);
    }

    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();

    // Expand each range with context lines, then merge overlapping display
    // windows into contiguous segments.
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for r in ranges {
        if r.start == 0 || r.start > total {
            continue;
        }
        let ctx_start = if r.start > 1 {
            r.start.saturating_sub(RANGE_LEADING_CONTEXT)
        } else {
            1
        };
        let ctx_end = match r.end {
            Some(e) => (e + RANGE_TRAILING_CONTEXT).min(total),
            None => total,
        };
        if let Some(last) = windows.last_mut()
            && ctx_start <= last.1 + 1
        {
            last.1 = last.1.max(ctx_end);
        } else {
            windows.push((ctx_start, ctx_end));
        }
    }

    let mut out = String::with_capacity(text.len() / 2 + path.len() + 16);
    use std::fmt::Write as _;
    if let Some(tag) = tag {
        let _ = writeln!(out, "[{path}#{tag}]");
    }

    for (wi, &(wstart, wend)) in windows.iter().enumerate() {
        if wi > 0 {
            out.push_str("...\n");
        }
        for line_no in wstart..=wend {
            if line_no == 0 || line_no > total {
                continue;
            }
            let _ = write!(out, "{}:{}", line_no, lines[line_no - 1]);
            out.push('\n');
        }
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Format file content without hashline headers or line numbers — verbatim
/// output. When `ranges` is provided, only those lines are included (with
/// `...` gap markers between disjoint ranges).
pub fn format_raw(text: &str, ranges: Option<&[LineRange]>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();

    let Some(ranges) = ranges else {
        return text.to_string();
    };
    if ranges.is_empty() {
        return text.to_string();
    }

    let mut windows: Vec<(usize, usize)> = Vec::new();
    for r in ranges {
        if r.start == 0 || r.start > total {
            continue;
        }
        let end = r.end.unwrap_or(total).min(total);
        if let Some(last) = windows.last_mut()
            && r.start <= last.1 + 1
        {
            last.1 = last.1.max(end);
        } else {
            windows.push((r.start, end));
        }
    }

    let mut out = String::new();
    for (wi, &(wstart, wend)) in windows.iter().enumerate() {
        if wi > 0 {
            out.push_str("...\n");
        }
        for line_no in wstart..=wend {
            if line_no == 0 || line_no > total {
                continue;
            }
            out.push_str(lines[line_no - 1]);
            out.push('\n');
        }
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_bom_and_crlf() {
        let raw = "\u{feff}a\r\nb\r\n";
        assert_eq!(normalize_to_lf(raw), "a\nb\n");
    }

    #[test]
    fn detect_crlf_and_bom() {
        assert!(detect_crlf("a\r\nb\r\n"));
        assert!(!detect_crlf("a\nb\n"));
        assert!(has_bom("\u{feff}a"));
        assert!(!has_bom("a"));
    }

    #[test]
    fn format_numbered_shapes_header_and_lines() {
        let out = format_numbered("a.rs", "fn main() {\n}", Some("1A2B3C"));
        assert_eq!(out, "[a.rs#1A2B3C]\n1:fn main() {\n2:}");
    }

    fn ten_line_file() -> String {
        (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn format_numbered_range_empty_ranges_falls_back() {
        let text = ten_line_file();
        let tag = compute_tag(&text);
        let out = format_numbered_range("f.rs", &text, Some(&tag), &[]);
        assert_eq!(out, format_numbered("f.rs", &text, Some(&tag)));
    }

    #[test]
    fn format_numbered_range_subset_with_context() {
        let text = ten_line_file();
        let tag = compute_tag(&text);
        // Range 5-7 → context: line 4 (1 leading) + lines 8,9,10 (3 trailing).
        let ranges = [LineRange {
            start: 5,
            end: Some(7),
        }];
        let out = format_numbered_range("f.rs", &text, Some(&tag), &ranges);
        assert!(out.contains("4:line4"), "leading context: {out}");
        assert!(out.contains("5:line5"));
        assert!(out.contains("6:line6"));
        assert!(out.contains("7:line7"));
        assert!(out.contains("8:line8"), "trailing context: {out}");
        assert!(out.contains("9:line9"), "trailing context: {out}");
        assert!(out.contains("10:line10"), "trailing context: {out}");
        assert!(!out.contains("1:line1"), "out-of-range excluded: {out}");
    }

    #[test]
    fn format_numbered_range_gap_marker() {
        let text = (1..=100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tag = compute_tag(&text);
        let ranges = [
            LineRange {
                start: 5,
                end: Some(5),
            },
            LineRange {
                start: 90,
                end: Some(90),
            },
        ];
        let out = format_numbered_range("f.rs", &text, Some(&tag), &ranges);
        assert!(
            out.contains("...\n"),
            "gap marker between disjoint ranges: {out}"
        );
    }

    #[test]
    fn format_raw_full() {
        let text = "hello\nworld";
        let out = format_raw(text, None);
        assert_eq!(out, text);
    }

    #[test]
    fn format_raw_with_ranges() {
        let text = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let ranges = [LineRange {
            start: 3,
            end: Some(5),
        }];
        let out = format_raw(&text, Some(&ranges));
        assert_eq!(out, "line3\nline4\nline5");
    }

    #[test]
    fn format_raw_gap_marker() {
        let text = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let ranges = [
            LineRange {
                start: 2,
                end: Some(3),
            },
            LineRange {
                start: 18,
                end: Some(19),
            },
        ];
        let out = format_raw(&text, Some(&ranges));
        assert!(out.contains("...\n"));
        assert!(out.starts_with("line2\nline3"));
        assert!(out.ends_with("line18\nline19"));
    }
}
