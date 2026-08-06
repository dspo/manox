//! Path selector parsing — oh-my-pi style `path:selector` syntax.
//!
//! Selectors are appended to a file path after the last colon:
//!
//! - `:N` / `:N-` — from line N onward (1-indexed)
//! - `:N-M` — inclusive range
//! - `:N+K` — K lines starting from N
//! - `:N..M` — alias for `N-M`
//! - `:5-16,960-973` — comma-separated multi-range (sorted, merged)
//! - `:raw` — verbatim, no hashline header/line numbers
//! - `:raw:1-50` / `:1-50:raw` — compound

/// Inclusive line range (1-indexed). `end` is `None` for open-ended ranges
/// (`:N-` means "from N to EOF").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineRange {
    pub start: usize,
    pub end: Option<usize>,
}

/// A parsed selector from a `path:selector` string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// One or more line ranges (`:50-100`, `:5-16,960-973`).
    Lines(Vec<LineRange>),
    /// Verbatim output, no hashline header or line numbers (`:raw`).
    Raw,
    /// Verbatim output restricted to line ranges (`:raw:1-50` / `:1-50:raw`).
    RawLines(Vec<LineRange>),
}

/// Split a `path:selector` string into `(path, selector)`.
///
/// Returns `(path, None)` when no valid selector is found — the entire input
/// is treated as the path. Selector detection uses `rfind(':')` to peel the
/// trailing chunk, then validates it against the selector grammar. Invalid
/// selectors fall through gracefully (the colon is part of the path).
pub fn split_path_and_sel(raw: &str) -> (&str, Option<Selector>) {
    let Some(colon) = raw.rfind(':') else {
        return (raw, None);
    };
    // Colon at position 0 means no path component — treat entire string as path.
    if colon == 0 {
        return (raw, None);
    }

    let candidate = &raw[colon + 1..];
    if let Some(sel) = try_parse_selector(candidate) {
        let base = &raw[..colon];

        // Compound: check for `path:1-50:raw` or `path:raw:1-50`.
        if let Some(inner_colon) = base.rfind(':')
            && inner_colon > 0
        {
            let inner_candidate = &base[inner_colon + 1..];
            let outer_is_raw = is_raw(candidate);
            let inner_is_raw = is_raw(inner_candidate);
            let outer_is_range = try_parse_ranges(candidate).is_some();
            let inner_is_range = try_parse_ranges(inner_candidate).is_some();

            if (inner_is_raw && outer_is_range) || (inner_is_range && outer_is_raw) {
                let compound = format!("{inner_candidate}:{candidate}");
                if let Some(sel) = try_parse_selector(&compound) {
                    return (&base[..inner_colon], Some(sel));
                }
            }
        }

        return (base, Some(sel));
    }

    (raw, None)
}

fn is_raw(s: &str) -> bool {
    s.eq_ignore_ascii_case("raw")
}

/// Try to parse a selector string. Returns `None` if it doesn't match the
/// selector grammar at all.
fn try_parse_selector(sel: &str) -> Option<Selector> {
    let sel = sel.trim();
    if sel.is_empty() {
        return None;
    }

    // Split on `:` for compound selectors.
    let chunks: Vec<&str> = sel.split(':').collect();

    let mut has_raw = false;
    let mut ranges: Option<Vec<LineRange>> = None;

    for chunk in &chunks {
        let trimmed = chunk.trim();
        if is_raw(trimmed) {
            has_raw = true;
        } else if let Some(parsed) = try_parse_ranges(trimmed) {
            ranges = Some(parsed);
        } else {
            // Unknown chunk — not a valid selector.
            return None;
        }
    }

    match (has_raw, ranges) {
        (true, Some(r)) => Some(Selector::RawLines(r)),
        (true, None) => Some(Selector::Raw),
        (false, Some(r)) => Some(Selector::Lines(r)),
        (false, None) => None,
    }
}

/// Parse a comma-separated list of line range chunks. Returns `None` if any
/// chunk fails to parse.
fn try_parse_ranges(sel: &str) -> Option<Vec<LineRange>> {
    let chunks: Vec<&str> = sel.split(',').collect();
    if chunks.is_empty() {
        return None;
    }
    let mut parsed = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        parsed.push(parse_line_range_chunk(chunk.trim())?);
    }
    parsed.sort_by_key(|r| r.start);

    // Merge overlapping/adjacent ranges.
    let mut merged: Vec<LineRange> = vec![parsed[0].clone()];
    for current in parsed.into_iter().skip(1) {
        let last = merged.last_mut().unwrap();
        // Open-ended absorbs everything after.
        if last.end.is_none() {
            continue;
        }
        let last_end = last.end.unwrap();
        if current.start <= last_end + 1 {
            match current.end {
                None => last.end = None,
                Some(ce) if ce > last_end => last.end = Some(ce),
                _ => {}
            }
            continue;
        }
        merged.push(current);
    }
    Some(merged)
}

/// Parse a single range chunk: `N`, `N-M`, `N-`, `N+K`, `N..M`, `N..`.
/// Returns `None` on invalid syntax.
fn parse_line_range_chunk(chunk: &str) -> Option<LineRange> {
    // Strip optional `L` prefix (oh-my-pi allows `L50-L100`).
    let chunk = chunk.strip_prefix(['L', 'l']).unwrap_or(chunk);
    if chunk.is_empty() {
        return None;
    }

    // Find the separator: `-`, `+`, or `..`.
    let (start_str, sep, end_str) = split_range_parts(chunk)?;

    let start: usize = start_str.parse().ok()?;
    if start < 1 {
        return None;
    }

    match sep {
        None => Some(LineRange { start, end: None }),
        Some('+') => {
            let count: usize = end_str?.parse().ok()?;
            if count < 1 {
                return None;
            }
            Some(LineRange {
                start,
                end: Some(start + count - 1),
            })
        }
        Some('-') => {
            if let Some(end_s) = end_str {
                let end: usize = end_s.parse().ok()?;
                if end < start {
                    return None;
                }
                Some(LineRange {
                    start,
                    end: Some(end),
                })
            } else {
                // `N-` means from N onward (same as bare `N`).
                Some(LineRange { start, end: None })
            }
        }
        _ => None,
    }
}

/// Split a range chunk into `(start_digits, separator, end_digits)`.
/// Separator is `'-'`, `'+'`, or the `..` pair is normalized to `'-'`.
/// Returns `None` if the chunk doesn't look like a range expression.
fn split_range_parts(s: &str) -> Option<(&str, Option<char>, Option<&str>)> {
    let bytes = s.as_bytes();
    // Find first digit run for start.
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return None;
    }
    let start_str = &s[..i];

    if i == bytes.len() {
        return Some((start_str, None, None));
    }

    // Check separator.
    let rest = &s[i..];
    if let Some(tail) = rest.strip_prefix("..") {
        // `..` normalized to `-`.
        let end_str = if tail.is_empty() {
            None
        } else {
            let tail = tail.strip_prefix(['L', 'l']).unwrap_or(tail);
            if tail.is_empty() { None } else { Some(tail) }
        };
        Some((start_str, Some('-'), end_str))
    } else if rest.starts_with('-') || rest.starts_with('+') {
        let sep = rest.as_bytes()[0] as char;
        let tail = &rest[1..];
        let end_str = if tail.is_empty() {
            None
        } else {
            let tail = tail.strip_prefix(['L', 'l']).unwrap_or(tail);
            if tail.is_empty() { None } else { Some(tail) }
        };
        Some((start_str, Some(sep), end_str))
    } else {
        None
    }
}

/// Return `true` when `line_number` (1-indexed) falls within any of the ranges.
pub fn is_line_in_ranges(line_number: usize, ranges: &[LineRange]) -> bool {
    for r in ranges {
        if line_number < r.start {
            continue;
        }
        if r.end.is_none() || line_number <= r.end.unwrap() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_selector() {
        assert_eq!(split_path_and_sel("src/foo.rs"), ("src/foo.rs", None));
        assert_eq!(
            split_path_and_sel("/abs/path/file.rs"),
            ("/abs/path/file.rs", None)
        );
    }

    #[test]
    fn single_line() {
        let (path, sel) = split_path_and_sel("src/foo.rs:50");
        assert_eq!(path, "src/foo.rs");
        assert_eq!(
            sel,
            Some(Selector::Lines(vec![LineRange {
                start: 50,
                end: None
            }]))
        );
    }

    #[test]
    fn inclusive_range() {
        let (path, sel) = split_path_and_sel("src/foo.rs:50-100");
        assert_eq!(path, "src/foo.rs");
        assert_eq!(
            sel,
            Some(Selector::Lines(vec![LineRange {
                start: 50,
                end: Some(100)
            }]))
        );
    }

    #[test]
    fn open_ended_range() {
        let (path, sel) = split_path_and_sel("src/foo.rs:50-");
        assert_eq!(path, "src/foo.rs");
        assert_eq!(
            sel,
            Some(Selector::Lines(vec![LineRange {
                start: 50,
                end: None
            }]))
        );
    }

    #[test]
    fn count_from() {
        let (path, sel) = split_path_and_sel("src/foo.rs:50+150");
        assert_eq!(path, "src/foo.rs");
        assert_eq!(
            sel,
            Some(Selector::Lines(vec![LineRange {
                start: 50,
                end: Some(199)
            }]))
        );
    }

    #[test]
    fn dotdot_alias() {
        let (path, sel) = split_path_and_sel("src/foo.rs:50..100");
        assert_eq!(path, "src/foo.rs");
        assert_eq!(
            sel,
            Some(Selector::Lines(vec![LineRange {
                start: 50,
                end: Some(100)
            }]))
        );
    }

    #[test]
    fn multi_range() {
        let (path, sel) = split_path_and_sel("src/foo.rs:5-16,960-973");
        assert_eq!(path, "src/foo.rs");
        assert_eq!(
            sel,
            Some(Selector::Lines(vec![
                LineRange {
                    start: 5,
                    end: Some(16)
                },
                LineRange {
                    start: 960,
                    end: Some(973)
                },
            ]))
        );
    }

    #[test]
    fn multi_range_merges_overlapping() {
        let (_, sel) = split_path_and_sel("f:5-10,8-15");
        assert_eq!(
            sel,
            Some(Selector::Lines(vec![LineRange {
                start: 5,
                end: Some(15)
            }]))
        );
    }

    #[test]
    fn multi_range_merges_adjacent() {
        let (_, sel) = split_path_and_sel("f:5-10,11-15");
        assert_eq!(
            sel,
            Some(Selector::Lines(vec![LineRange {
                start: 5,
                end: Some(15)
            }]))
        );
    }

    #[test]
    fn raw_selector() {
        let (path, sel) = split_path_and_sel("src/foo.rs:raw");
        assert_eq!(path, "src/foo.rs");
        assert_eq!(sel, Some(Selector::Raw));
    }

    #[test]
    fn raw_case_insensitive() {
        let (_, sel) = split_path_and_sel("src/foo.rs:RAW");
        assert_eq!(sel, Some(Selector::Raw));
    }

    #[test]
    fn compound_raw_range() {
        let (path, sel) = split_path_and_sel("src/foo.rs:raw:1-50");
        assert_eq!(path, "src/foo.rs");
        assert_eq!(
            sel,
            Some(Selector::RawLines(vec![LineRange {
                start: 1,
                end: Some(50)
            }]))
        );
    }

    #[test]
    fn compound_range_raw() {
        let (path, sel) = split_path_and_sel("src/foo.rs:1-50:raw");
        assert_eq!(path, "src/foo.rs");
        assert_eq!(
            sel,
            Some(Selector::RawLines(vec![LineRange {
                start: 1,
                end: Some(50)
            }]))
        );
    }

    #[test]
    fn colon_at_start_is_not_selector() {
        assert_eq!(split_path_and_sel(":weird"), (":weird", None));
    }

    #[test]
    fn invalid_range_rejected() {
        // End < start → not a valid selector → treated as path.
        assert_eq!(
            split_path_and_sel("src/foo.rs:100-50"),
            ("src/foo.rs:100-50", None)
        );
    }

    #[test]
    fn zero_line_rejected() {
        assert_eq!(split_path_and_sel("f:0"), ("f:0", None));
    }

    #[test]
    fn l_prefix_accepted() {
        let (_, sel) = split_path_and_sel("f:L50-L100");
        assert_eq!(
            sel,
            Some(Selector::Lines(vec![LineRange {
                start: 50,
                end: Some(100)
            }]))
        );
    }

    #[test]
    fn is_line_in_ranges_check() {
        let ranges = vec![
            LineRange {
                start: 5,
                end: Some(10),
            },
            LineRange {
                start: 20,
                end: None,
            },
        ];
        assert!(!is_line_in_ranges(4, &ranges));
        assert!(is_line_in_ranges(5, &ranges));
        assert!(is_line_in_ranges(10, &ranges));
        assert!(!is_line_in_ranges(11, &ranges));
        assert!(!is_line_in_ranges(19, &ranges));
        assert!(is_line_in_ranges(20, &ranges));
        assert!(is_line_in_ranges(9999, &ranges));
    }
}
