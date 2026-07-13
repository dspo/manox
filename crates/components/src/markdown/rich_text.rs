//! `RichText` — a text element that paints rounded washes behind inline-code
//! spans and supports mouse selection + Cmd/Ctrl+C copy, on top of the
//! shaping/painting machinery borrowed from `StyledText`/`TextLayout`.
//!
//! The element composes a `StyledText` for layout and glyph painting (its runs
//! carry no `background_color`, so `StyledText` paints no flat wash) and adds
//! its own overlay pass before the glyphs: rounded quads for code spans, a flat
//! selection rect, then the text on top. Code-span geometry and selection
//! hit-testing both come from `TextLayout::position_for_index` /
//! `index_for_position` — the two concerns collapse onto one layout, which is
//! why they live in the same element.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, BorderStyle, Bounds, ClipboardItem, Corners, CursorStyle, DispatchPhase, Edges, Element,
    ElementId, GlobalElementId, HighlightStyle, Hitbox, HitboxBehavior, Hsla, InspectorElementId,
    IntoElement, KeyDownEvent, LayoutId, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad,
    Pixels, Point, SharedString, StyledText, TextLayout, Window, fill, point, px, quad, size,
    transparent_black,
};

/// One rounded-wash span: a byte range, a fill color, and a corner radius.
/// The renderer maps `InlineRuns::code_ranges` onto these at render time so
/// the wash is caller-customizable (`Markdown::inline_code`) rather than a
/// fixed theme color baked into a run.
#[derive(Clone)]
pub struct CodeSpan {
    pub range: Range<usize>,
    pub bg: Hsla,
    pub radius: Pixels,
}

/// A text element with rounded inline-code washes and optional mouse
/// selection. Used for paragraphs (code wash only, not selectable) and for
/// code/diff blocks (selectable, no code wash — their per-line washes are
/// carried by the surrounding `div`, not inline spans).
pub struct RichText {
    /// `None` for non-interactive mounts (paragraphs, headings, table cells):
    /// no element state, no listener registration, no uniqueness requirement
    /// across siblings. `Some` only where selection state must persist.
    id: Option<ElementId>,
    text: SharedString,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    code_spans: Vec<CodeSpan>,
    selectable: bool,
    selection_bg: Hsla,
}

impl RichText {
    pub fn new(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Self {
        Self {
            id: Some(id.into()),
            text: text.into(),
            highlights: Vec::new(),
            code_spans: Vec::new(),
            selectable: false,
            selection_bg: Hsla::default(),
        }
    }

    /// Stateless mount — no element id, so no per-element state and no
    /// listener registration. For paragraphs/headings/table cells where the
    /// only job is to paint rounded code washes.
    pub fn anonymous(text: impl Into<SharedString>) -> Self {
        Self {
            id: None,
            text: text.into(),
            highlights: Vec::new(),
            code_spans: Vec::new(),
            selectable: false,
            selection_bg: Hsla::default(),
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

    /// Enable per-block mouse selection + Cmd/Ctrl+C copy. `selection_bg` is
    /// the flat wash painted behind the selected glyphs.
    pub fn selectable(mut self, selection_bg: Hsla) -> Self {
        self.selectable = true;
        self.selection_bg = selection_bg;
        self
    }
}

impl IntoElement for RichText {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Per-element selection state, persisted across renders via `with_element_state`
/// keyed by the element id. The `Rc` cells are cloned into the mouse/keyboard
/// listeners registered each paint, so an event callback mutates the same
/// selection the next frame reads.
#[derive(Clone, Default)]
struct RichTextState {
    /// Ordered anchor/caret pair (byte indices); `None` when no selection.
    selection: Rc<RefCell<Option<(usize, usize)>>>,
    /// True between MouseDown and MouseUp — while dragging, MouseMove extends
    /// the caret even beyond the hitbox (clamped by `index_for_position`).
    dragging: Rc<std::cell::Cell<bool>>,
}

impl Element for RichText {
    type RequestLayoutState = StyledText;
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        self.id.clone()
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
        // Runs carry no `background_color` for code spans — the wash is painted
        // separately from `code_spans` so it can be rounded. `StyledText`
        // inherits the base font/color from `window.text_style()` (the parent
        // div's `.text_sm()`/`.text_color()`/…), matching the old contract.
        let mut styled =
            StyledText::new(self.text.clone()).with_highlights(self.highlights.iter().cloned());
        let (layout_id, _) = styled.request_layout(None, inspector_id, window, cx);
        (layout_id, styled)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        styled: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        styled.prepaint(None, inspector_id, bounds, &mut (), window, cx);
        // Only selectable mounts need a hitbox — non-interactive paragraphs and
        // headings stay hitbox-free to match the old `StyledText` contract and
        // avoid interfering with ancestor hover regions.
        if self.selectable {
            Some(window.insert_hitbox(bounds, HitboxBehavior::Normal))
        } else {
            None
        }
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        styled: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let layout = styled.layout().clone();

        // 1. Rounded code-span washes, painted behind the glyphs.
        for span in &self.code_spans {
            for quad_bounds in span_quads(&layout, span.range.start, span.range.end, px(3.)) {
                window.paint_quad(rounded_quad(quad_bounds, span.bg, span.radius));
            }
        }

        // 2. Selection + interaction (only for selectable mounts with an id
        //    and a hitbox from prepaint).
        if self.selectable
            && let (Some(gid), Some(hitbox)) = (global_id, hitbox.as_ref())
        {
            if debug_selection() {
                eprintln!(
                    "[selection] paint id={:?} bounds=({:?}, {:?}, {:?}, {:?})",
                    self.id,
                    bounds.origin.x,
                    bounds.origin.y,
                    bounds.size.width,
                    bounds.size.height,
                );
            }
            let selection_bg = self.selection_bg;
            let text = self.text.clone();
            window.with_element_state::<RichTextState, _>(gid, |state, window| {
                let state = state.unwrap_or_default();

                // Paint the active selection behind the glyphs.
                if let Some((a, b)) = *state.selection.borrow() {
                    let (s, e) = (a.min(b), a.max(b));
                    if s < e {
                        for qb in span_quads(&layout, s, e, px(0.)) {
                            window.paint_quad(fill(qb, selection_bg));
                        }
                    }
                }

                window.set_cursor_style(CursorStyle::IBeam, hitbox);

                register_selection_listeners(
                    state.clone(),
                    layout.clone(),
                    text,
                    hitbox.clone(),
                    window,
                );

                ((), state)
            });
        }

        // 3. Text on top. `StyledText::paint` draws the glyph runs; since none
        //    carry a `background_color`, no flat wash is painted over the
        //    overlays above.
        styled.paint(None, inspector_id, bounds, &mut (), &mut (), window, cx);
    }
}

/// Wire MouseDown/Move/Up + Cmd/Ctrl+C for one selectable `RichText` mount.
/// Each listener closes over clones of the per-element state cells and the
/// layout, so it mutates the selection the next frame reads.
fn register_selection_listeners(
    state: RichTextState,
    layout: TextLayout,
    text: SharedString,
    hitbox: Hitbox,
    window: &mut Window,
) {
    // MouseDown: begin a selection at the clicked glyph. Hit-test against the
    // hitbox bounds directly rather than `is_hovered`, which can return false
    // when an occluding BlockMouse hitbox is painted above this element — the
    // text still sits under the cursor and should still be selectable.
    window.on_mouse_event({
        let selection = state.selection.clone();
        let dragging = state.dragging.clone();
        let layout = layout.clone();
        let hitbox = hitbox.clone();
        move |event: &MouseDownEvent, phase, window, _cx| {
            if debug_selection() {
                eprintln!(
                    "[selection] mousedown fired phase={:?} pos=({:?}, {:?}) hovered={} bounds_contains={}",
                    phase,
                    event.position.x,
                    event.position.y,
                    hitbox.is_hovered(window),
                    hitbox.bounds.contains(&event.position),
                );
            }
            if phase != DispatchPhase::Bubble || !hitbox.bounds.contains(&event.position) {
                return;
            }
            let ix = index_at(&layout, event.position);
            if debug_selection() {
                eprintln!("[selection] mousedown start ix={ix}");
            }
            dragging.set(true);
            selection.replace(Some((ix, ix)));
            window.refresh();
        }
    });

    // MouseMove: while dragging, extend the caret (clamped to text bounds).
    window.on_mouse_event({
        let selection = state.selection.clone();
        let dragging = state.dragging.clone();
        let layout = layout.clone();
        move |event: &MouseMoveEvent, phase, window, _cx| {
            if phase != DispatchPhase::Bubble || !dragging.get() {
                return;
            }
            let ix = index_at(&layout, event.position);
            if debug_selection() {
                eprintln!("[selection] mousemove extending ix={ix}");
            }
            if let Some(pair) = selection.borrow_mut().as_mut() {
                pair.1 = ix;
            }
            window.refresh();
        }
    });

    // MouseUp: stop dragging; a zero-width selection clears so the next click
    // starts fresh and no stray caret lingers for Cmd+C.
    window.on_mouse_event({
        let selection = state.selection.clone();
        let dragging = state.dragging.clone();
        move |_event: &MouseUpEvent, phase, window, _cx| {
            if phase != DispatchPhase::Bubble || !dragging.get() {
                return;
            }
            dragging.set(false);
            let clear = (*selection.borrow()).map(|(a, b)| a == b).unwrap_or(true);
            if debug_selection() {
                eprintln!("[selection] mouseup clear={clear}");
            }
            if clear {
                selection.replace(None);
            }
            window.refresh();
        }
    });

    // Cmd/Ctrl+C: copy the active (non-empty) selection. Fires only for blocks
    // with a live selection — so a selection that persists after mouse-up
    // doesn't steal Cmd+C from the composer input, whose `Copy` action travels
    // a separate dispatch channel and would otherwise be overwritten by a stray
    // clipboard write.
    //
    // No `is_hovered` gate here: a keyboard Cmd+C flips `last_input_was_keyboard`,
    // which makes `HitboxId::is_hovered` always return false and would silently
    // kill every copy. The `has_selection` gate above is the real scope limiter.
    let has_selection = (*state.selection.borrow())
        .map(|(a, b)| a != b)
        .unwrap_or(false);
    if has_selection {
        window.on_key_event::<KeyDownEvent>({
            let selection = state.selection.clone();
            move |event: &KeyDownEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let k = &event.keystroke;
                if !((k.modifiers.platform || k.modifiers.control) && k.key == "c") {
                    return;
                }
                if let Some((a, b)) = *selection.borrow() {
                    if debug_selection() {
                        eprintln!("[selection] copy a={a} b={b} text_len={}", text.len());
                    }
                    let (s, e) = (a.min(b), a.max(b));
                    if s < e && e <= text.len() {
                        let s = floor_char_boundary(&text, s);
                        let e = ceil_char_boundary(&text, e);
                        if s < e {
                            cx.write_to_clipboard(ClipboardItem::new_string(
                                text[s..e].to_string(),
                            ));
                        }
                    }
                }
            }
        });
    }
}

/// Whether to emit selection diagnostics to stderr. Set `MANOX_DEBUG_SELECTION`
/// to trace mouse/keyboard events through the per-block selection state machine
/// — intended for blind-fix verification, off by default in normal builds.
fn debug_selection() -> bool {
    static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLAG.get_or_init(|| std::env::var("MANOX_DEBUG_SELECTION").is_ok())
}

/// Byte index of the glyph under a window-coordinate point, clamped to the
/// text range. `index_for_position` returns `Ok` on a glyph, `Err` between
/// glyphs; both carry the closest index, so either branch yields a usable
/// caret position.
fn index_at(layout: &TextLayout, position: Point<Pixels>) -> usize {
    match layout.index_for_position(position) {
        Ok(ix) | Err(ix) => ix,
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
) -> Vec<Bounds<Pixels>> {
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
        out.push(Bounds::new(point(x0, start.y), size(x1 - x0, lh)));
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
            out.push(Bounds::new(point(x0, y), size(x1 - x0, lh)));
        }
    }
    out
}

/// A filled rounded quad — the inline-code pill.
fn rounded_quad(bounds: Bounds<Pixels>, bg: Hsla, radius: Pixels) -> PaintQuad {
    quad(
        bounds,
        Corners::all(radius),
        bg,
        Edges::default(),
        transparent_black(),
        BorderStyle::default(),
    )
}

/// Largest char boundary `<= i`, so a mid-codepoint byte index (from an
/// imprecise hit-test) slices the `SharedString` without panicking.
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

    #[test]
    fn code_span_is_clone_and_carries_style() {
        let span = CodeSpan {
            range: 0..5,
            bg: Hsla::default(),
            radius: px(4.),
        };
        let cloned = span.clone();
        assert_eq!(cloned.range, span.range);
        assert_eq!(cloned.radius, span.radius);
    }

    #[test]
    fn rich_text_builder_composes() {
        let rt = RichText::new("rt", "hello")
            .highlights(vec![(0..2, HighlightStyle::default())])
            .code_spans(vec![CodeSpan {
                range: 0..5,
                bg: Hsla::default(),
                radius: px(3.),
            }])
            .selectable(Hsla::default());
        assert!(rt.selectable);
        assert_eq!(rt.code_spans.len(), 1);
        assert_eq!(rt.highlights.len(), 1);
    }

    #[test]
    fn char_boundary_helpers_clamp() {
        // `é` is two bytes; byte 1 is mid-codepoint, floor/ceil snap to 0/2.
        let s = "é";
        assert_eq!(floor_char_boundary(s, 1), 0);
        assert_eq!(ceil_char_boundary(s, 1), 2);
    }
}
