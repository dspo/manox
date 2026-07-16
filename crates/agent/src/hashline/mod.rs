//! Hashline editing: line-anchored patches validated by content-hash tags.
//!
//! `read_file` mints a 4-hex tag from the file's normalized text and records a
//! snapshot; `edit_file` parses a patch of `SWAP`/`DEL`/`INS` ops anchored on
//! the ORIGINAL line numbers, validates the tag still matches the live file,
//! and applies the ops back-to-front. On a stale tag, a 3-way merge replays the
//! resolved changes onto the current content by content-anchoring the snapshot
//! ranges. The model never has to reproduce the original text verbatim — only
//! the line numbers and a fresh tag.
//!
//! Global state is a `OnceLock<Mutex<SnapshotStore>>` mirroring the
//! `runtime` / `thread_store` singleton pattern. Tools access it via
//! [`global`], which panics if [`init`] was not called at app startup.

pub mod apply;
pub mod block;
pub mod hash;
pub mod parser;
pub mod recovery;
pub mod snapshot;

#[cfg(test)]
mod integration_tests;

use std::sync::{Mutex, OnceLock};

pub use apply::{ApplyError, ApplyResult, apply};
pub use block::BlockError;
pub use hash::compute_tag;
pub use parser::{FilePatch, InsPos, Op, ParseError, parse_patch};
pub use recovery::{RecoverError, try_recover, try_recover_with_snapshot};
pub use snapshot::{Snapshot, SnapshotStore};

static GLOBAL: OnceLock<Mutex<SnapshotStore>> = OnceLock::new();

/// Initialize the global snapshot store. Call once at app startup from
/// [`crate::init`]. Idempotent: a second call is a no-op.
pub fn init() {
    let _ = GLOBAL.set(Mutex::new(SnapshotStore::new()));
}

/// The global snapshot store. Panics if [`init`] was not called.
pub fn global() -> &'static Mutex<SnapshotStore> {
    GLOBAL
        .get()
        .expect("hashline snapshot store not initialized; call agent::init first")
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

/// Format a file for `read_file` output: a `[path#TAG]` header followed by
/// `N:TEXT` numbered lines. The caller is responsible for recording the snapshot
/// before formatting so the tag is stable.
pub fn format_numbered(path: &str, text: &str, tag: &str) -> String {
    let mut out = String::with_capacity(text.len() + path.len() + 16);
    out.push('[');
    out.push_str(path);
    out.push('#');
    out.push_str(tag);
    out.push(']');
    out.push('\n');
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
    tag: &str,
    ranges: &[crate::tools::path_selector::LineRange],
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
    let _ = writeln!(out, "[{path}#{tag}]");

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
/// output for `:raw` selectors. When `ranges` is provided, only those lines
/// are included (with `...` gap markers between disjoint ranges).
pub fn format_raw(text: &str, ranges: Option<&[crate::tools::path_selector::LineRange]>) -> String {
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
    use crate::tools::path_selector::LineRange;

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
        let out = format_numbered("a.rs", "fn main() {\n}", "1A2B");
        assert_eq!(out, "[a.rs#1A2B]\n1:fn main() {\n2:}");
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
        let out = format_numbered_range("f.rs", &text, &tag, &[]);
        assert_eq!(out, format_numbered("f.rs", &text, &tag));
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
        let out = format_numbered_range("f.rs", &text, &tag, &ranges);
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
        let out = format_numbered_range("f.rs", &text, &tag, &ranges);
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
