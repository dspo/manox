//! Document-level selection for a `Markdown` body.
//!
//! A single `DocSelection` spans every text-bearing block of one markdown
//! document. Selection state lives in `Rc` cells shared between the document
//! container's mouse/keyboard listeners and each block element, so a drag that
//! crosses paragraph / code / list boundaries extends one continuous selection
//! rather than N independent per-block ones.
//!
//! Coordinate model: each selectable leaf block owns a sub-range of a virtual
//! document built by concatenating leaf texts in paint order. A block reports
//! its geometry (its `TextLayout` + painted bounds + doc-start offset) to the
//! per-frame registry during paint; the container's mouse listeners hit-test a
//! window point against that registry to resolve it to one document-wide byte
//! index for the anchor/caret. Copy walks the registry entries in order,
//! slicing each block's laid-out text by the intersected doc range and joining
//! adjacent blocks with a blank-line separator — so the copied text is exactly
//! what was painted, cursor and all.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::rc::Rc;

use gpui::{Bounds, ClipboardItem, Pixels, Point, TextLayout};

/// One selectable leaf block's geometry, recorded during paint so the document
/// mouse listeners can hit-test by window coordinate. Entries arrive in paint
/// (i.e. document) order; their painted bounds are non-overlapping vertical
/// bands.
pub(crate) struct BlockHit {
    /// Virtual-document byte offset where this block's text begins.
    pub doc_start: usize,
    /// Absolute-coordinate text layout (the block's `StyledText` layout). Carries
    /// the exact laid-out text via `.text()`, so copy slices what was painted.
    pub layout: TextLayout,
    /// Painted bounds of the block's text run, in window coordinates.
    pub bounds: Bounds<Pixels>,
    /// Separator prepended before this leaf when joining copied text across
    /// leaves: `"\n"` for a continuation line within one multi-line block (diff /
    /// conflict line index > 0), `"\n\n"` for a block boundary. The first
    /// selected leaf contributes no prefix.
    pub join_before: &'static str,
    /// Local byte ranges of inline-code spans within this block's text. A
    /// double-click landing inside one selects the whole span verbatim
    /// (`code in line`), so a code run reads as one selectable unit. Empty for
    /// plain-text blocks (e.g. the terminal panel's flat output).
    pub code_ranges: Vec<Range<usize>>,
}

/// Document-wide selection state, shared (via `Rc`) between the container
/// listeners and every block element so highlight + copy read one source of
/// truth.
#[derive(Clone, Default)]
pub struct DocSelection {
    /// Selection anchor (mouse-down point) as a virtual-doc byte index, or
    /// `None` when no selection is active.
    anchor: Rc<RefCell<Option<usize>>>,
    /// Caret (current drag endpoint) as a virtual-doc byte index.
    caret: Rc<Cell<usize>>,
    /// True between MouseDown and MouseUp; while set, MouseMove extends caret.
    dragging: Rc<Cell<bool>>,
    /// Per-frame layout registry: cleared by the sentinel at paint start, then
    /// filled by each block's paint. Read by the container's mouse/keyboard
    /// listeners on the events that follow, which always postdate the most
    /// recent paint.
    blocks: Rc<RefCell<Vec<BlockHit>>>,
}

impl DocSelection {
    pub fn new() -> Self {
        Self::default()
    }

    /// The active `[min, max]` selection, or `None` when collapsed/empty.
    pub fn range(&self) -> Option<(usize, usize)> {
        let anchor = *self.anchor.borrow();
        let caret = self.caret.get();
        anchor
            .map(|a| (a.min(caret), a.max(caret)))
            .filter(|(s, e)| s < e)
    }

    pub fn is_dragging(&self) -> bool {
        self.dragging.get()
    }

    /// Begin a selection at `doc_index` (mouse-down). Clears any prior selection
    /// so a new drag starts fresh.
    pub(crate) fn begin(&self, doc_index: usize) {
        self.dragging.set(true);
        self.anchor.replace(Some(doc_index));
        self.caret.set(doc_index);
    }

    /// Extend the caret to `doc_index` while dragging.
    pub(crate) fn extend(&self, doc_index: usize) {
        if self.dragging.get() {
            self.caret.set(doc_index);
        }
    }

    /// End the drag. A zero-width selection (anchor == caret) clears so no stray
    /// caret lingers for Cmd+C.
    pub(crate) fn end(&self) {
        self.dragging.set(false);
        let collapsed = self.range().map(|(s, e)| s == e).unwrap_or(true);
        if collapsed {
            self.anchor.replace(None);
        }
    }

    /// Select the word (or inline-code span) covering `doc_index`. A double-click
    /// lands here: if the index is inside a registered code span, the whole span
    /// is selected; otherwise the maximal run of alphanumeric/underscore
    /// characters around the index (a non-word char selects just itself). Not a
    /// drag — `dragging` stays false so a later mouse-move does not extend it
    /// until the next mouse-down.
    pub(crate) fn select_word(&self, doc_index: usize) {
        if let Some((start, end)) = self.word_range(doc_index) {
            self.dragging.set(false);
            self.anchor.replace(Some(start));
            self.caret.set(end);
        } else {
            self.begin(doc_index);
        }
    }

    /// Select the line covering `doc_index` (between the surrounding newlines). A
    /// triple-click lands here. Like `select_word`, leaves `dragging` false.
    pub(crate) fn select_line(&self, doc_index: usize) {
        if let Some((start, end)) = self.line_range(doc_index) {
            self.dragging.set(false);
            self.anchor.replace(Some(start));
            self.caret.set(end);
        } else {
            self.begin(doc_index);
        }
    }

    /// Resolve the block that owns `doc_index`, returning its virtual-doc start,
    /// laid-out text, and local code-span ranges. The registry is rebuilt each
    /// frame, so this reads the most recent paint — the same registry the
    /// hit-test uses.
    fn block_at(&self, doc_index: usize) -> Option<(usize, String, Vec<Range<usize>>)> {
        let blocks = self.blocks.borrow();
        for hit in blocks.iter() {
            let len = layout_len(&hit.layout);
            if (hit.doc_start..hit.doc_start + len).contains(&doc_index) {
                return Some((hit.doc_start, hit.layout.text(), hit.code_ranges.clone()));
            }
        }
        None
    }

    /// The `[start, end]` byte range of the word / code span at `doc_index`, in
    /// virtual-doc coordinates.
    fn word_range(&self, doc_index: usize) -> Option<(usize, usize)> {
        let (doc_start, text, code_ranges) = self.block_at(doc_index)?;
        let local = doc_index - doc_start;
        // Inline-code span: the whole span is the selection unit.
        for r in &code_ranges {
            if r.contains(&local) {
                return Some((doc_start + r.start, doc_start + r.end));
            }
        }
        // Char-index map so multi-byte glyphs do not split a word.
        let byte_offsets: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        let chars: Vec<char> = text.chars().collect();
        let cur = byte_offsets.iter().rposition(|&b| b <= local).unwrap_or(0);
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let mut s = cur;
        while s > 0 && is_word(chars[s - 1]) {
            s -= 1;
        }
        let mut e = cur;
        if e < chars.len() && is_word(chars[e]) {
            while e + 1 < chars.len() && is_word(chars[e + 1]) {
                e += 1;
            }
            e += 1; // exclusive char end
        } else if e < chars.len() {
            // Non-word char: select just that one glyph.
            e += 1;
        }
        let bs = byte_offsets.get(s).copied().unwrap_or(text.len());
        let be = byte_offsets.get(e).copied().unwrap_or(text.len());
        Some((doc_start + bs, doc_start + be))
    }

    /// The `[start, end]` byte range of the line at `doc_index` (newline-bounded).
    fn line_range(&self, doc_index: usize) -> Option<(usize, usize)> {
        let (doc_start, text, _code_ranges) = self.block_at(doc_index)?;
        let local = (doc_index - doc_start).min(text.len());
        let bytes = text.as_bytes();
        let start = bytes[..local]
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let end = bytes[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| start + p)
            .unwrap_or(text.len());
        Some((doc_start + start, doc_start + end))
    }

    /// Reset the per-frame registry, called once at paint start by the sentinel
    /// element so block paints repopulate a fresh list each frame.
    pub(crate) fn clear_registry(&self) {
        self.blocks.borrow_mut().clear();
    }

    /// Record one block's geometry during its paint.
    pub(crate) fn register(&self, hit: BlockHit) {
        self.blocks.borrow_mut().push(hit);
    }

    /// Resolve a window-coordinate point to a virtual-doc byte index by
    /// hit-testing the per-frame block registry. A point in the gap between
    /// blocks snaps to the boundary of the nearest block, so a drag that sweeps
    /// across a margin still advances the caret rather than stalling.
    pub(crate) fn hit(&self, point: Point<Pixels>) -> Option<usize> {
        let blocks = self.blocks.borrow();
        let mut best: Option<(usize, Pixels)> = None;
        for hit in blocks.iter() {
            // Inside a block: index_for_position yields the glyph byte index.
            if hit.bounds.contains(&point) {
                let ix = index_in_layout(&hit.layout, point);
                return Some(hit.doc_start + ix.min(layout_len(&hit.layout)));
            }
            // Otherwise track the nearest block boundary for snap-to behavior.
            let dy = (hit.bounds.center().y - point.y).abs();
            if best.map(|(_, d)| dy < d).unwrap_or(true) {
                // Snap toward the block whose start/end the point is nearest: if
                // the point is above this block, snap to its start; if below, to
                // its end (clamped at copy time).
                let snap = if point.y < hit.bounds.center().y {
                    hit.doc_start
                } else {
                    hit.doc_start + layout_len(&hit.layout)
                };
                best = Some((snap, dy));
            }
        }
        best.map(|(ix, _)| ix)
    }

    /// Copy the active selection's text to the clipboard. Builds the doc ranges
    /// from the per-frame registry — each block's laid-out text is exactly what
    /// was painted — so the copied text matches the visible content.
    pub(crate) fn copy_to_clipboard(&self, cx: &mut gpui::App) -> bool {
        let Some((start, end)) = self.range() else {
            return false;
        };
        let ranges: Vec<DocRange> = self
            .blocks
            .borrow()
            .iter()
            .map(|hit| {
                let text = hit.layout.text();
                let len = text.len();
                DocRange {
                    range: hit.doc_start..hit.doc_start + len,
                    text,
                    join_before: hit.join_before,
                }
            })
            .collect();
        let Some(text) = selected_text(&ranges, start, end) else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }
}

/// One leaf block's selectable text, its virtual-document byte range, and the
/// separator prepended before it when joining copied text across leaves.
pub struct DocRange {
    pub range: Range<usize>,
    pub text: String,
    pub join_before: &'static str,
}

/// Build the selected substring across leaf ranges. Slices the partial leaves
/// at the selection endpoints and full leaves in between, joining each leaf
/// (after the first) with its own `join_before` separator — `"\n"` for a
/// continuation line within one diff/conflict block, `"\n\n"` for a block
/// boundary — so copied text reads as it renders.
fn selected_text(ranges: &[DocRange], start: usize, end: usize) -> Option<String> {
    let mut out = String::new();
    let mut seen = false;
    for r in ranges {
        // Skip leaves entirely before/after the selection.
        if r.range.end <= start || r.range.start >= end {
            continue;
        }
        let lo = start.saturating_sub(r.range.start);
        let hi = end.min(r.range.end) - r.range.start;
        let lo = floor_char_boundary(&r.text, lo);
        let hi = ceil_char_boundary(&r.text, hi);
        if lo < hi && hi <= r.text.len() {
            if seen {
                out.push_str(r.join_before);
            }
            out.push_str(&r.text[lo..hi]);
            seen = true;
        }
    }
    if !seen {
        return None;
    }
    Some(out)
}

/// Byte index of the glyph under a window-coordinate point within one layout,
/// clamped to the layout's text range. `index_for_position` returns `Ok` on a
/// glyph and `Err` between glyphs; both carry the closest index.
fn index_in_layout(layout: &TextLayout, point: Point<Pixels>) -> usize {
    match layout.index_for_position(point) {
        Ok(ix) | Err(ix) => ix,
    }
}

/// Character count of a layout's text, used to clamp hit indices so a drag past
/// the last glyph does not overshoot the document.
fn layout_len(layout: &TextLayout) -> usize {
    layout.len()
}

/// Largest char boundary `<= i`, so a mid-codepoint byte index slices without
/// panicking.
fn floor_char_boundary(s: &str, i: usize) -> usize {
    s.floor_char_boundary(i)
}

/// Smallest char boundary `>= i`.
fn ceil_char_boundary(s: &str, i: usize) -> usize {
    s.ceil_char_boundary(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: usize, text: &str) -> DocRange {
        DocRange {
            range: start..start + text.len(),
            text: text.to_string(),
            join_before: "\n\n",
        }
    }

    fn line(start: usize, text: &str) -> DocRange {
        DocRange {
            range: start..start + text.len(),
            text: text.to_string(),
            join_before: "\n",
        }
    }

    #[test]
    fn copy_full_range_returns_all_text_joined() {
        let ranges = [range(0, "alpha"), range(5, "beta")];
        let text = selected_text(&ranges, 0, 9).unwrap();
        assert_eq!(text, "alpha\n\nbeta");
    }

    #[test]
    fn copy_partial_range_slices_one_block() {
        let ranges = [range(0, "hello world")];
        // bytes [3, 9) of "hello world" → "lo wor".
        let text = selected_text(&ranges, 3, 9).unwrap();
        assert_eq!(text, "lo wor");
    }

    #[test]
    fn copy_across_two_blocks_inserts_separator() {
        let ranges = [range(0, "one"), range(3, "two")];
        // "ne" + "tw"
        let text = selected_text(&ranges, 1, 5).unwrap();
        assert_eq!(text, "ne\n\ntw");
    }

    #[test]
    fn copy_skips_blocks_outside_selection() {
        let ranges = [range(0, "aaa"), range(3, "bbb"), range(6, "ccc")];
        // Select only the middle block.
        let text = selected_text(&ranges, 3, 6).unwrap();
        assert_eq!(text, "bbb");
    }

    #[test]
    fn copy_across_diff_lines_joins_with_single_newline() {
        // Three diff lines (leaves) of one code block, joined with "\n".
        let ranges = [line(0, "added"), line(5, "removed"), line(13, "ctx")];
        // Full span across all three lines.
        let text = selected_text(&ranges, 0, 16).unwrap();
        assert_eq!(text, "added\nremoved\nctx");
    }

    #[test]
    fn copy_from_paragraph_into_diff_uses_block_boundary_then_lines() {
        // A paragraph leaf, then a diff block: its first line is a block
        // boundary ("\n\n"), its second line a continuation ("\n").
        let ranges = [range(0, "intro"), range(5, "l1"), line(7, "l2")];
        // Span the paragraph tail + both lines.
        let text = selected_text(&ranges, 2, 9).unwrap();
        assert_eq!(text, "tro\n\nl1\nl2");
    }

    #[test]
    fn selection_range_is_none_when_collapsed() {
        let sel = DocSelection::new();
        assert!(sel.range().is_none());
        sel.begin(5);
        // anchor == caret → collapsed → range is None.
        assert!(sel.range().is_none());
        sel.extend(9);
        assert_eq!(sel.range(), Some((5, 9)));
        sel.end();
        assert_eq!(sel.range(), Some((5, 9)));
    }

    #[test]
    fn end_clears_collapsed_selection() {
        let sel = DocSelection::new();
        sel.begin(7);
        sel.end();
        // After end with a zero-width selection, the anchor clears so Cmd+C is
        // a no-op.
        assert!(sel.range().is_none());
    }

    #[test]
    fn floor_ceil_clamp_mid_codepoint() {
        // `é` is two bytes; byte 1 is mid-codepoint.
        assert_eq!(floor_char_boundary("é", 1), 0);
        assert_eq!(ceil_char_boundary("é", 1), 2);
    }
}
