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
    App, Bounds, Element, ElementId, Entity, FocusHandle, Font, FontFeatures, FontStyle,
    FontWeight, GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, SharedString,
    StrikethroughStyle, Style, TextAlign, TextRun, UnderlineStyle, Window, fill, point, px,
    relative, rgba, size,
};
use terminal::{Cell, Flags, Terminal};

use crate::grid_renderer::layout_grid;
use crate::terminal_view::{TerminalInputHandler, TerminalView};
use crate::theme::TerminalTheme;

/// The paint-only terminal element. Constructed by `TerminalView::render`.
pub struct TerminalElement {
    pub terminal: Entity<Terminal>,
    pub view: Entity<TerminalView>,
    pub focus_handle: FocusHandle,
    pub theme: TerminalTheme,
    pub font: Font,
    pub font_size: Pixels,
    pub line_height: f32,
    /// In-flight IME marked text, painted inline at the cursor.
    pub marked_text: SharedString,
    /// `/pattern` match ranges in grid coordinates, painted as highlights.
    pub search_matches: Vec<(terminal::Point, terminal::Point)>,
    /// Index of the active match (highlighted distinctly).
    pub active_match: Option<usize>,
}

/// Computed during prepaint, consumed during paint.
pub struct PrepaintState {
    plan: crate::grid_renderer::GridPlan,
    cell_width: Pixels,
    line_height_px: Pixels,
    bounds: Bounds<Pixels>,
    cursor: Option<(i32, i32)>,
    /// Pixel rects for search matches; `true` = the active match.
    search_rects: Vec<(Bounds<Pixels>, bool)>,
}

impl TerminalElement {
    pub fn new(
        terminal: Entity<Terminal>,
        view: Entity<TerminalView>,
        focus_handle: FocusHandle,
    ) -> Self {
        Self {
            terminal,
            view,
            focus_handle,
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
            marked_text: SharedString::default(),
            search_matches: Vec::new(),
            active_match: None,
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

    /// Convert grid-coordinate match ranges to pixel rects, keeping only the
    /// portion visible in the current display window. Multi-line matches are
    /// truncated to their first line (rare for `/pattern` search). The active
    /// match index is flagged so paint can color it distinctly.
    fn match_rects(
        matches: &[(terminal::Point, terminal::Point)],
        active: Option<usize>,
        offset: i32,
        rows: i32,
        bounds: Bounds<Pixels>,
        cell_w: Pixels,
        lh: Pixels,
    ) -> Vec<(Bounds<Pixels>, bool)> {
        let mut out = Vec::new();
        for (i, (start, end)) in matches.iter().enumerate() {
            // display_row = grid_line + (rows-1) + offset
            let display_row = start.line.0 + (rows - 1) + offset;
            if !(0..rows).contains(&display_row) {
                continue;
            }
            let start_col = start.column.0 as i32;
            let end_col = (end.column.0 as i32).max(start_col);
            let x = bounds.origin.x + start_col as f32 * cell_w;
            let y = bounds.origin.y + display_row as f32 * lh;
            let w = ((end_col - start_col + 1).max(1) as f32) * cell_w;
            out.push((Bounds::new(point(x, y), size(w, lh)), active == Some(i)));
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
        let (plan, cursor, offset) = self.terminal.read_with(cx, |t, _cx| {
            t.with_term(|term| {
                let content = term.renderable_content();
                let cursor_pt = content.cursor.point;
                let offset = term.grid().display_offset() as i32;
                let cells = Self::display_cells(content);
                let plan = layout_grid(cells.into_iter(), &self.theme);
                (
                    plan,
                    Some((cursor_pt.line.0, cursor_pt.column.0 as i32)),
                    offset,
                )
            })
        });
        let rows = self.terminal.read_with(cx, |t, _| t.rows as i32);
        let search_rects = Self::match_rects(
            &self.search_matches,
            self.active_match,
            offset,
            rows,
            bounds,
            cell_width,
            line_height_px,
        );

        PrepaintState {
            plan,
            cell_width,
            line_height_px,
            bounds,
            cursor,
            search_rects,
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

        // `/pattern` search highlights. The active match gets a stronger color.
        for (rect, is_active) in &prepaint.search_rects {
            let color = if *is_active {
                rgba(0xffa500cc)
            } else {
                rgba(0xffe06666)
            };
            window.paint_quad(fill(*rect, color));
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

        // Cursor block + inline IME marked (preedit) text.
        if let Some((line, col)) = prepaint.cursor {
            let x = origin.x + col as f32 * cell_w;
            let y = origin.y + line as f32 * lh;
            let pos = point(x, y);
            let sz = size(cell_w, lh);
            let cursor_bounds = Bounds::new(pos, sz);

            if !self.marked_text.is_empty() {
                // Paint the preedit text over the cursor with a highlight bg.
                let probe = TextRun {
                    len: self.marked_text.len(),
                    font: self.font.clone(),
                    color: self.theme.default_bg,
                    background_color: Some(self.theme.cursor),
                    underline: None,
                    strikethrough: None,
                };
                let shaped = window.text_system().shape_line(
                    self.marked_text.clone(),
                    self.font_size,
                    std::slice::from_ref(&probe),
                    Some(cell_w),
                );
                let bg = size(shaped.width().max(cell_w), lh);
                window.paint_quad(fill(Bounds::new(pos, bg), self.theme.cursor));
                let _ = shaped.paint(pos, lh, TextAlign::Left, None, window, cx);
            } else {
                window.paint_quad(fill(cursor_bounds, self.theme.cursor));
            }

            // Register the IME input handler for this frame so the platform
            // routes composition events here, with the candidate window placed
            // at the cursor.
            window.handle_input(
                &self.focus_handle,
                TerminalInputHandler {
                    view: self.view.clone(),
                    cursor_bounds: Some(cursor_bounds),
                },
                cx,
            );
        }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}
