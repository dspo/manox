//! `RichText` — a text element that paints inline-code spans in a foreground
//! color and participates in document-level selection.
//!
//! The element composes a `StyledText` for shaping/glyph painting. Inline-code
//! spans receive a foreground-color highlight so `StyledText` paints the glyphs
//! (including backticks) in the inline-code color. The element adds its own
//! overlay pass before the glyphs: a flat selection rect for the slice of the
//! document selection that falls inside this block, then the text on top.
//! Code-span geometry comes from `TextLayout::position_for_index`; the selection
//! slice is the intersection of the block's virtual-document range with the shared
//! `DocSelection`.
//!
//! Per-block mouse/keyboard listeners are gone: a drag that crosses block
//! boundaries is driven by the document container, which hit-tests this block's
//! registered layout. Each block only reports its geometry (via `register`) and
//! paints its slice of the shared selection.

use std::ops::Range;

use gpui::{
    App, Element, GlobalElementId, HighlightStyle, Hsla, InspectorElementId, IntoElement, LayoutId,
    Pixels, SharedString, StyledText, TextLayout, Window, fill, point, px, size,
};

use crate::markdown::ast::LinkSpan;
use crate::markdown::selection::{BlockHit, DocSelection};

/// One inline-code span: a byte range (including surrounding backticks) and
/// a foreground color. The renderer maps `InlineRuns::code_ranges` onto these
/// at render time so the color is caller-customizable (`Markdown::inline_code`)
/// rather than a fixed theme color baked into a run.
#[derive(Clone)]
pub struct CodeSpan {
    pub range: Range<usize>,
    pub fg: Hsla,
}

/// A text element that renders inline-code spans in a caller-customized
/// foreground color and participates in document-level selection. The block's
/// virtual-document start offset, the shared `DocSelection`, and the cross-leaf
/// join separator are supplied by the document renderer.
pub struct RichText {
    text: SharedString,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    code_spans: Vec<CodeSpan>,
    doc_start: usize,
    selection: DocSelection,
    selection_bg: Hsla,
    join_before: &'static str,
    /// Link spans detected in this block's text, registered in `BlockHit` so
    /// the document container can resolve Cmd+click to a link target.
    link_spans: Vec<LinkSpan>,
    /// Color of the underline painted under link spans.
    link_color: Hsla,
}

impl RichText {
    pub fn new(text: impl Into<SharedString>, doc_start: usize, selection: DocSelection) -> Self {
        Self {
            text: text.into(),
            highlights: Vec::new(),
            code_spans: Vec::new(),
            doc_start,
            selection,
            selection_bg: Hsla::default(),
            join_before: "\n\n",
            link_spans: Vec::new(),
            link_color: Hsla::default(),
        }
    }

    pub fn highlights(mut self, highlights: Vec<(Range<usize>, HighlightStyle)>) -> Self {
        self.highlights = highlights;
        self
    }

    pub fn code_spans(mut self, spans: Vec<CodeSpan>) -> Self {
        self.code_spans = spans;
        self
    }

    /// Flat wash painted behind the glyphs covered by the document selection.
    pub fn selection_bg(mut self, bg: Hsla) -> Self {
        self.selection_bg = bg;
        self
    }

    /// Separator prepended before this leaf when joining copied text across
    /// leaves — `"\n"` for a continuation line within one diff/conflict block,
    /// `"\n\n"` for a block boundary. Defaults to `"\n\n"`.
    pub fn join_before(mut self, sep: &'static str) -> Self {
        self.join_before = sep;
        self
    }

    /// Link spans to register for Cmd+click hit-testing and to paint as
    /// underlined text.
    pub fn link_spans(mut self, spans: Vec<LinkSpan>) -> Self {
        self.link_spans = spans;
        self
    }

    /// Color of the underline painted under link spans.
    pub fn link_color(mut self, color: Hsla) -> Self {
        self.link_color = color;
        self
    }
}

/// Merge code-span foreground highlights into an existing highlight list.
///
/// When emphasis (bold/italic/strikethrough) wraps inline code, both highlights
/// cover the same byte range. This function merges the `color` field into the
/// existing emphasis highlight rather than creating a duplicate range, preserving
/// the non-overlapping + sorted constraint required by `StyledText::compute_runs`.
///
/// Three branches:
/// - **Same-range merge**: code range matches an existing highlight → set `color`
/// - **Ordered insert**: code range falls between two existing highlights → insert
/// - **Tail append**: code range extends beyond all existing highlights → push
fn merge_code_highlights(
    highlights: &mut Vec<(Range<usize>, HighlightStyle)>,
    code_spans: &[CodeSpan],
) {
    for span in code_spans {
        let code_highlight = HighlightStyle {
            color: Some(span.fg),
            ..Default::default()
        };
        if let Some(existing) = highlights
            .iter_mut()
            .find(|(range, _)| range == &span.range)
        {
            existing.1.color = code_highlight.color;
        } else {
            let pos = highlights
                .iter()
                .position(|(range, _)| range.start > span.range.start)
                .unwrap_or(highlights.len());
            highlights.insert(pos, (span.range.clone(), code_highlight));
        }
    }
}

impl IntoElement for RichText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for RichText {
    type RequestLayoutState = StyledText;
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        // Merge code-span foreground colors into the emphasis highlight list,
        // preserving the non-overlapping + sorted constraint required by
        // `StyledText::compute_runs`.
        let mut all_highlights = self.highlights.clone();
        merge_code_highlights(&mut all_highlights, &self.code_spans);
        let mut styled = StyledText::new(self.text.clone()).with_highlights(all_highlights);
        let (layout_id, _) = styled.request_layout(None, inspector_id, window, cx);
        (layout_id, styled)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        styled: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        styled.prepaint(None, inspector_id, bounds, &mut (), window, cx);
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: gpui::Bounds<Pixels>,
        styled: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        let layout = styled.layout().clone();

        // Register this block's geometry so the document container can hit-test
        // a drag that sweeps across block boundaries.
        self.selection.register(BlockHit {
            doc_start: self.doc_start,
            layout: layout.clone(),
            bounds,
            join_before: self.join_before,
            code_ranges: self
                .code_spans
                .iter()
                .map(|span| span.range.clone())
                .collect(),
            link_spans: self.link_spans.clone(),
        });

        // 1. The slice of the document selection that falls inside this block,
        //    painted behind the glyphs. Local byte indices are the block's
        //    virtual-document range shifted by `doc_start`.
        let block_len = layout.len();
        if let Some((s, e)) = self.selection.range() {
            let lo = s.saturating_sub(self.doc_start).min(block_len);
            let hi = e.saturating_sub(self.doc_start).min(block_len);
            if lo < hi {
                for qb in span_quads(&layout, lo, hi, px(0.)) {
                    window.paint_quad(fill(qb, self.selection_bg));
                }
            }
        }

        // 2. Link underlines, painted behind the glyphs. Each link span gets a
        //    1px underline at the font baseline.
        if self.link_color.a > 0.0 {
            for link in &self.link_spans {
                for quad_bounds in span_quads(&layout, link.range.start, link.range.end, px(0.)) {
                    let y = quad_bounds.bottom() - px(2.);
                    let underline = gpui::Bounds::new(
                        point(quad_bounds.left(), y),
                        size(quad_bounds.size.width, px(1.)),
                    );
                    window.paint_quad(fill(underline, self.link_color));
                }
            }
        }

        // 3. Text on top. `StyledText::paint` draws the glyph runs with
        //    foreground-color highlights for inline-code spans.
        styled.paint(None, inspector_id, bounds, &mut (), &mut (), window, _cx);
    }
}

/// Per-line quads covering `[start_ix, end_ix)` with `pad_x` horizontal padding
/// on the first and last line. Single-line spans produce one quad; spans that
/// cross a soft-wrap boundary produce one quad per visual line (first line to
/// the right edge, last line from the left edge, middle lines full width).
fn span_quads(
    layout: &TextLayout,
    start_ix: usize,
    end_ix: usize,
    pad_x: Pixels,
) -> Vec<gpui::Bounds<Pixels>> {
    let mut out = Vec::new();
    let Some(start) = layout.position_for_index(start_ix) else {
        return out;
    };
    let Some(end) = layout.position_for_index(end_ix) else {
        return out;
    };
    let lh = layout.line_height();
    let bounds = layout.bounds();

    // Same visual line when the y delta is a sub-pixel fraction of the line
    // height (Pixels / Pixels → f32, no private-field access).
    let dy_lines = (start.y - end.y) / lh;
    if dy_lines.abs() < 0.01 {
        let x0 = start.x - pad_x;
        let x1 = end.x + pad_x;
        out.push(gpui::Bounds::new(point(x0, start.y), size(x1 - x0, lh)));
    } else {
        // Visual line numbers relative to the layout's top; rounding absorbs
        // the sub-pixel snap `position_for_index` carries.
        let top = bounds.top();
        let start_line = (((start.y - top) / lh).round()) as i32;
        let end_line = (((end.y - top) / lh).round()) as i32;
        for line in start_line..=end_line {
            let y = top + line as f32 * lh;
            let x0 = if line == start_line {
                start.x - pad_x
            } else {
                bounds.left()
            };
            let x1 = if line == end_line {
                end.x + pad_x
            } else {
                bounds.right()
            };
            out.push(gpui::Bounds::new(point(x0, y), size(x1 - x0, lh)));
        }
    }
    out
}

/// Largest char boundary `<= i`, so a mid-codepoint byte index slices the
/// `SharedString` without panicking.
#[allow(dead_code)]
fn floor_char_boundary(s: &str, i: usize) -> usize {
    s.floor_char_boundary(i)
}

/// Smallest char boundary `>= i`.
#[allow(dead_code)]
fn ceil_char_boundary(s: &str, i: usize) -> usize {
    s.ceil_char_boundary(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_span_is_clone_and_carries_style() {
        let span = CodeSpan {
            range: 0..5,
            fg: Hsla::default(),
        };
        let cloned = span.clone();
        assert_eq!(cloned.range, span.range);
        assert_eq!(cloned.fg, span.fg);
    }

    fn blue() -> Hsla {
        gpui::hsla(0.6, 0.8, 0.5, 1.0)
    }

    #[test]
    fn merge_code_same_range_sets_color() {
        // **`code`** → emphasis highlight (bold) at 0..6, code span at 0..6
        let mut highlights = vec![(
            0..6,
            HighlightStyle {
                font_weight: Some(gpui::FontWeight::BOLD),
                ..Default::default()
            },
        )];
        let code_spans = vec![CodeSpan {
            range: 0..6,
            fg: blue(),
        }];
        merge_code_highlights(&mut highlights, &code_spans);
        assert_eq!(highlights.len(), 1);
        assert_eq!(highlights[0].0, 0..6);
        assert_eq!(highlights[0].1.color, Some(blue()));
        // bold weight preserved
        assert_eq!(highlights[0].1.font_weight, Some(gpui::FontWeight::BOLD));
    }

    #[test]
    fn merge_code_inserts_in_sorted_order() {
        // `a` **`code`** `b` → emphasis at 8..14, code spans at 0..3, 8..14, 19..22
        let mut highlights = vec![(
            8..14,
            HighlightStyle {
                font_weight: Some(gpui::FontWeight::BOLD),
                ..Default::default()
            },
        )];
        let code_spans = vec![
            CodeSpan {
                range: 0..3,
                fg: blue(),
            },
            CodeSpan {
                range: 8..14,
                fg: blue(),
            },
            CodeSpan {
                range: 19..22,
                fg: blue(),
            },
        ];
        merge_code_highlights(&mut highlights, &code_spans);
        assert_eq!(highlights.len(), 3);
        // sorted by range.start
        assert_eq!(highlights[0].0, 0..3);
        assert_eq!(highlights[1].0, 8..14);
        assert_eq!(highlights[2].0, 19..22);
        // all carry blue color
        for h in &highlights {
            assert_eq!(h.1.color, Some(blue()));
        }
        // bold preserved on the middle one
        assert_eq!(highlights[1].1.font_weight, Some(gpui::FontWeight::BOLD));
    }

    #[test]
    fn merge_code_appends_at_tail() {
        // no existing highlights, just a code span
        let mut highlights: Vec<(Range<usize>, HighlightStyle)> = vec![];
        let code_spans = vec![CodeSpan {
            range: 5..10,
            fg: blue(),
        }];
        merge_code_highlights(&mut highlights, &code_spans);
        assert_eq!(highlights.len(), 1);
        assert_eq!(highlights[0].0, 5..10);
        assert_eq!(highlights[0].1.color, Some(blue()));
    }
}
