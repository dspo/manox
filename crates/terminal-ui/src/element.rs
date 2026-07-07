//! `TerminalElement` — manox's first gpui `Element`.
//!
//! Three phases mirror zed's `terminal_element`:
//!   - `request_layout`: fill the parent (width/height = relative 1).
//!   - `prepaint`: measure cell size from the font, derive cols/rows from
//!     bounds, resize the Terminal, run `layout_grid` over
//!     `renderable_content().display_iter`, stash the plan.
//!   - `paint`: fill the default background, paint merged background regions,
//!     shape+paint each batched text run, then the cursor block.
//!
//! No `InteractiveElement`/hitbox here — mouse and keyboard are routed by
//! `TerminalView`'s wrapping `div`, keeping this element paint-only.

use gpui::{
    App, Bounds, Element, ElementId, Entity, Font, FontFeatures, FontStyle, FontWeight,
    GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, SharedString,
    StrikethroughStyle, Style, TextAlign, TextRun, UnderlineStyle, Window, fill, point, px,
    relative, size,
};
use terminal::{Cell, Flags, Terminal};

use crate::grid_renderer::layout_grid;
use crate::theme::TerminalTheme;

/// The paint-only terminal element. Constructed by `TerminalView::render`.
pub struct TerminalElement {
    pub terminal: Entity<Terminal>,
    pub theme: TerminalTheme,
    pub font: Font,
    pub font_size: Pixels,
    pub line_height: f32,
}

/// Computed during prepaint, consumed during paint.
pub struct PrepaintState {
    plan: crate::grid_renderer::GridPlan,
    cell_width: Pixels,
    line_height_px: Pixels,
    bounds: Bounds<Pixels>,
    cursor: Option<(i32, i32)>,
}

impl TerminalElement {
    pub fn new(terminal: Entity<Terminal>) -> Self {
        Self {
            terminal,
            theme: TerminalTheme::default(),
            font: Font {
                family: "Menlo".into(),
                features: FontFeatures::default(),
                fallbacks: None,
                weight: FontWeight::default(),
                style: FontStyle::Normal,
            },
            font_size: px(14.),
            line_height: 1.2,
        }
    }

    /// Map alacritty's display iterator to `(display_line, column, &Cell)`,
    /// assigning a 0-based display line by detecting line changes (the raw
    /// `point.line` is a grid coordinate that does not start at 0). Consumes
    /// the `RenderableContent` (GridIterator is not Clone).
    fn display_cells<'a>(
        mut content: terminal::RenderableContent<'a>,
    ) -> Vec<(i32, usize, &'a Cell)> {
        let mut out: Vec<(i32, usize, &Cell)> = Vec::new();
        let mut display_line = -1i32;
        let mut prev: Option<i32> = None;
        for idx in content.display_iter.by_ref() {
            let line = idx.point.line.0;
            if prev != Some(line) {
                display_line += 1;
                prev = Some(line);
            }
            out.push((display_line, idx.point.column.0, idx.cell));
        }
        out
    }
}

impl Element for TerminalElement {
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
        _window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        let layout_id = _window.request_layout(style, std::iter::empty(), _cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let line_height_px = px(f32::from(self.font_size) * self.line_height);

        // Measure cell width from a single glyph of the monospace font.
        let probe = TextRun {
            len: 1,
            font: self.font.clone(),
            color: self.theme.default_fg,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let shaped = window.text_system().shape_line(
            SharedString::from("m"),
            self.font_size,
            std::slice::from_ref(&probe),
            None,
        );
        let cell_width = shaped.width().max(px(1.));

        let cols = (bounds.size.width / cell_width).floor() as usize;
        let rows = (bounds.size.height / line_height_px).floor() as usize;
        if cols > 0 && rows > 0 {
            self.terminal.update(cx, |t, cx| t.resize(cols, rows, cx));
        }

        // Build the paint plan from the terminal's renderable snapshot.
        let (plan, cursor) = self.terminal.read_with(cx, |t, _cx| {
            t.with_term(|term| {
                let content = term.renderable_content();
                let cursor_pt = content.cursor.point;
                let cells = Self::display_cells(content);
                let plan = layout_grid(cells.into_iter(), &self.theme);
                (plan, Some((cursor_pt.line.0, cursor_pt.column.0 as i32)))
            })
        });

        PrepaintState {
            plan,
            cell_width,
            line_height_px,
            bounds,
            cursor,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let origin = prepaint.bounds.origin;
        let cell_w = prepaint.cell_width;
        let lh = prepaint.line_height_px;

        // Default background fills the whole bounds.
        window.paint_quad(fill(prepaint.bounds, self.theme.default_bg));

        // Merged non-default background regions.
        for region in &prepaint.plan.background {
            let x = origin.x + region.start_col as f32 * cell_w;
            let y = origin.y + region.start_line as f32 * lh;
            let w = (region.end_col - region.start_col + 1) as f32 * cell_w;
            let h = (region.end_line - region.start_line + 1) as f32 * lh;
            let pos = point(x, y);
            let sz = size(w, h);
            window.paint_quad(fill(Bounds::new(pos, sz), region.color));
        }

        // Text runs.
        for run in &prepaint.plan.runs {
            let x = origin.x + run.start_col as f32 * cell_w;
            let y = origin.y + run.start_line as f32 * lh;
            let pos = point(x, y);
            let text_run = TextRun {
                len: run.text.len(),
                font: self.font.clone(),
                color: run.fg,
                background_color: None,
                underline: if run.flags.contains(Flags::UNDERLINE) {
                    Some(UnderlineStyle::default())
                } else {
                    None
                },
                strikethrough: if run.flags.contains(Flags::STRIKEOUT) {
                    Some(StrikethroughStyle::default())
                } else {
                    None
                },
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(run.text.clone()),
                self.font_size,
                std::slice::from_ref(&text_run),
                Some(cell_w),
            );
            let _ = shaped.paint(pos, lh, TextAlign::Left, None, window, cx);
        }

        // Cursor block.
        if let Some((line, col)) = prepaint.cursor {
            let x = origin.x + col as f32 * cell_w;
            let y = origin.y + line as f32 * lh;
            let pos = point(x, y);
            let sz = size(cell_w, lh);
            window.paint_quad(fill(Bounds::new(pos, sz), self.theme.cursor));
        }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}
