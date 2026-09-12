//! Shared span types and overlap resolution for link detection.

use std::ops::Range;

/// How a detected span opens: URLs in a browser, paths in an editor / file
/// manager.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlKind {
    Url,
    Path,
}

/// A clickable span in text: the resolved target and its byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlaySpan {
    pub href: String,
    pub range: Range<usize>,
    pub kind: UrlKind,
}

/// Whether `range` overlaps any range in `covered`. `covered` is assumed
/// sorted by start and non-overlapping (as produced by [`consolidate`]).
pub fn is_covered(range: Range<usize>, covered: &[Range<usize>]) -> bool {
    for cov in covered {
        if cov.start < range.end && range.start < cov.end {
            return true;
        }
    }
    false
}

/// Resolve overlapping spans to a non-overlapping, sorted list. URLs take
/// precedence over paths; when two spans start at the same offset the longer
/// one wins. Kept spans are sorted by start then end.
pub fn consolidate(spans: Vec<OverlaySpan>) -> Vec<OverlaySpan> {
    let mut spans: Vec<_> = spans.into_iter().collect();
    spans.sort_by(|a, b| {
        a.range
            .start
            .cmp(&b.range.start)
            .then_with(|| b.range.end.cmp(&a.range.end))
            .then_with(|| kind_priority(a.kind).cmp(&kind_priority(b.kind)))
    });
    let mut out: Vec<OverlaySpan> = Vec::new();
    for span in spans {
        let overlaps = out.last().is_some_and(|last| {
            last.range.start < span.range.end && span.range.start < last.range.end
        });
        if !overlaps {
            out.push(span);
        }
    }
    out
}

fn kind_priority(kind: UrlKind) -> u8 {
    match kind {
        UrlKind::Url => 0,
        UrlKind::Path => 1,
    }
}
