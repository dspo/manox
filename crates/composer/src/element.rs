//! Text-rendering element for `ComposerInput`.
//!
//! Shapes the whole text with `shape_text` (multi-line + wrap), paints the
//! selection and caret, and registers the IME input handler during paint via
//! `window.handle_input` — the only registration point for `EntityInputHandler`.

use gpui::{
    App, Bounds, Element, ElementId, ElementInputHandler, Entity, GlobalElementId,
    InspectorElementId, LayoutId, PaintQuad, Pixels, Point, SharedString, Style, TextAlign,
    TextRun, UnderlineStyle, Window, fill, point, prelude::*, px, relative, rgba,
};

use crate::blink_cursor::CURSOR_WIDTH;
use crate::input::ComposerInput;

pub(crate) struct TextElement {
    pub input: Entity<ComposerInput>,
}

pub(crate) struct PrepaintState {
    cursor: Option<PaintQuad>,
    selection: Vec<PaintQuad>,
    line_height: Pixels,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let input = self.input.read(cx);
        let lh = window.line_height();
        let rows = input
            .computed_rows
            .max(input.auto_grow_min_rows)
            .min(input.auto_grow_max_rows);
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = (lh * rows as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> PrepaintState {
        let (text, selected_range, marked_range, placeholder, cursor_byte, is_empty) = {
            let i = self.input.read(cx);
            (
                i.text.clone(),
                i.selected_range.clone(),
                i.ime_marked_range.clone(),
                i.placeholder.clone(),
                i.cursor_offset(),
                i.text.is_empty() && i.ime_marked_range.is_none(),
            )
        };

        let style = window.text_style();
        let lh = window.line_height();

        let (display_text, text_color) = if is_empty {
            (placeholder, gpui::hsla(0., 0., 0.5, 0.5))
        } else {
            (SharedString::from(text), style.color)
        };

        let base_run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs: Vec<TextRun> = if let Some(m) = &marked_range {
            let mut v = vec![
                TextRun {
                    len: m.start,
                    ..base_run.clone()
                },
                TextRun {
                    len: m.end - m.start,
                    underline: Some(UnderlineStyle {
                        color: Some(base_run.color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..base_run.clone()
                },
                TextRun {
                    len: display_text.len().saturating_sub(m.end),
                    ..base_run.clone()
                },
            ];
            v.retain(|r| r.len > 0);
            v
        } else {
            vec![base_run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let lines = window
            .text_system()
            .shape_text(
                display_text,
                font_size,
                &runs,
                Some(bounds.size.width),
                None,
            )
            .unwrap_or_default()
            .into_vec();

        let rows: usize = lines.iter().map(|l| l.wrap_boundaries().len() + 1).sum();

        let (selection, cursor) = if is_empty {
            (Vec::new(), None)
        } else if selected_range.is_empty() {
            let p = point_for_byte(&lines, cursor_byte, bounds.origin, lh)
                .unwrap_or_else(|| point(bounds.left(), bounds.top()));
            let cursor = fill(
                Bounds::new(point(p.x - px(0.5), p.y), gpui::size(CURSOR_WIDTH, lh)),
                gpui::blue(),
            );
            (Vec::new(), Some(cursor))
        } else {
            let start = point_for_byte(&lines, selected_range.start, bounds.origin, lh)
                .unwrap_or_else(|| point(bounds.left(), bounds.top()));
            let end = point_for_byte(&lines, selected_range.end, bounds.origin, lh)
                .unwrap_or_else(|| point(bounds.left(), bounds.top()));
            // Simplified single rect; inaccurate when a selection spans a wrap
            // boundary, but correct for the common single-row case.
            (
                vec![fill(
                    Bounds::from_corners(start, point(end.x, end.y + lh)),
                    rgba(0x3311ff30),
                )],
                None,
            )
        };

        // Persist layout for IME bounds_for_range / hit-testing between frames.
        self.input.update(cx, |i, _| {
            i.last_lines = lines;
            i.last_bounds = Some(bounds);
            i.last_line_height = lh;
            i.computed_rows = rows;
        });

        PrepaintState {
            cursor,
            selection,
            line_height: lh,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        prepaint: &mut PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        for q in prepaint.selection.drain(..) {
            window.paint_quad(q);
        }

        let lh = prepaint.line_height;
        // Move the layout out for the duration of paint to avoid borrowing the
        // entity while `line.paint` borrows `cx`; put it back afterwards.
        let mut lines = self
            .input
            .update(cx, |i, _| std::mem::take(&mut i.last_lines));
        let mut y = bounds.top();
        for line in &lines {
            let _ = line.paint(
                point(bounds.left(), y),
                lh,
                TextAlign::Left,
                None,
                window,
                cx,
            );
            y += (line.wrap_boundaries().len() + 1) as f32 * lh;
        }

        let focused = focus_handle.is_focused(window);
        let blink_visible = self.input.read(cx).blink_cursor.read(cx).visible();
        if focused
            && blink_visible
            && let Some(c) = prepaint.cursor.take()
        {
            window.paint_quad(c);
        }

        // Put the layout back; IME may query it before the next paint.
        self.input
            .update(cx, |i, _| i.last_lines = std::mem::take(&mut lines));
    }
}

/// Pixel position of a byte offset within a shaped line list, relative to
/// `origin`. Walks logical lines, each contributing `wrap_boundaries + 1`
/// visual rows; uses `WrappedLine::position_for_index` for in-row placement.
fn point_for_byte(
    lines: &[gpui::WrappedLine],
    byte: usize,
    origin: Point<Pixels>,
    lh: Pixels,
) -> Option<Point<Pixels>> {
    let mut y = origin.y;
    let mut line_start = 0usize;
    for line in lines {
        let len = line.len();
        if byte <= line_start + len {
            let in_line = byte - line_start;
            let p = line.position_for_index(in_line, lh)?;
            return Some(point(origin.x + p.x, y + p.y));
        }
        y += (line.wrap_boundaries().len() + 1) as f32 * lh;
        line_start += len + 1; // +1 for the '\n'
    }
    None
}
