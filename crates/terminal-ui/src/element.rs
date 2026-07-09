//! `TerminalElement` — manox's first gpui `Element`.
//!
//! Three phases:
//!   - `request_layout`: fill the parent (width/height = relative 1).
//!   - `prepaint`: measure cell size from the font, derive cols/rows from
//!     bounds, resize the Terminal, run `layout_grid` over
//!     `renderable_content().display_iter`, then shape every text run (and
//!     the IME preedit line) so paint stays allocation-free.
//!   - `paint`: fill the default background, paint merged background regions,
//!     paint the pre-shaped text runs, then the cursor block.
//!
//! No `InteractiveElement`/hitbox here — mouse and keyboard are routed by
//! `TerminalView`'s wrapping `div`, keeping this element paint-only.

use gpui::{
    App, Bounds, Element, ElementId, Entity, FocusHandle, Font, FontFeatures, FontStyle,
    FontWeight, GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, Point,
    ShapedLine, SharedString, Size, StrikethroughStyle, Style, TextAlign, TextRun, UnderlineStyle,
    Window, fill, point, px, relative, rgba, size,
};
use terminal::{Cell, Flags, Terminal};

use crate::grid_renderer::{BackgroundRegion, GridPlan, layout_grid};
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
///
/// All `ShapedLine`s are shaped in prepaint so `paint` only emits quads and
/// painted lines — no per-frame shaping or string allocation in the paint
/// phase.
pub struct PrepaintState {
    bounds: Bounds<Pixels>,
    cell_width: Pixels,
    line_height_px: Pixels,
    background: Vec<BackgroundRegion>,
    /// Pixel rects for search matches; `true` = the active match.
    search_rects: Vec<(Bounds<Pixels>, bool)>,
    /// Pre-shaped text runs with their paint origin.
    shaped_runs: Vec<(Point<Pixels>, ShapedLine)>,
    /// Cursor block, plus a pre-shaped preedit line when IME marked text is
    /// active. `None` when the terminal reports no cursor.
    cursor: Option<CursorPrepaint>,
}

/// Cursor paint data: the block bounds, and a pre-shaped preedit line when
/// IME marked text is non-empty (`None` paints a plain cursor block).
pub struct CursorPrepaint {
    bounds: Bounds<Pixels>,
    marked: Option<MarkedPrepaint>,
}

/// Pre-shaped IME preedit (marked) text painted over the cursor block.
pub struct MarkedPrepaint {
    shaped: ShapedLine,
    bg_size: Size<Pixels>,
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
            // alacritty numbers grid lines top-down (line 0 = topmost visible
            // line when display_offset is 0), so display_row = grid_line + offset.
            let display_row = start.line.0 + offset;
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
            // Resize is a same-frame mutation so the renderable snapshot below
            // reflects the new grid size; TerminalView holds no `observe` on
            // the Terminal, so the inner `cx.notify()` cannot re-enter this
            // render pass.
            self.terminal.update(cx, |t, cx| t.resize(cols, rows, cx));
        }

        // Build the paint plan from the terminal's renderable snapshot, then
        // shape every text run here so paint stays allocation-free.
        let origin = bounds.origin;
        let (background, runs, cursor_grid, offset, term_rows) =
            self.terminal.read_with(cx, |t, _cx| {
                t.with_term(|term| {
                    let content = term.renderable_content();
                    let cursor_pt = content.cursor.point;
                    let offset = term.grid().display_offset() as i32;
                    let cells = Self::display_cells(content);
                    let GridPlan { background, runs } = layout_grid(cells.into_iter(), &self.theme);
                    let cursor_display_line = cursor_pt.line.0 + offset;
                    (
                        background,
                        runs,
                        Some((cursor_display_line, cursor_pt.column.0 as i32)),
                        offset,
                        t.rows as i32,
                    )
                })
            });

        let mut shaped_runs: Vec<(Point<Pixels>, ShapedLine)> = Vec::with_capacity(runs.len());
        for run in &runs {
            let pos = point(
                origin.x + run.start_col as f32 * cell_width,
                origin.y + run.start_line as f32 * line_height_px,
            );
            let text_run = TextRun {
                len: run.text.len(),
                font: self.font.clone(),
                color: run.fg,
                background_color: None,
                underline: run
                    .flags
                    .contains(Flags::UNDERLINE)
                    .then(UnderlineStyle::default),
                strikethrough: run
                    .flags
                    .contains(Flags::STRIKEOUT)
                    .then(StrikethroughStyle::default),
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(run.text.as_str()),
                self.font_size,
                std::slice::from_ref(&text_run),
                Some(cell_width),
            );
            shaped_runs.push((pos, shaped));
        }

        let search_rects = Self::match_rects(
            &self.search_matches,
            self.active_match,
            offset,
            term_rows,
            bounds,
            cell_width,
            line_height_px,
        );

        // Shape the IME preedit line here too; paint only emits the quads.
        let cursor = cursor_grid.map(|(line, col)| {
            let pos = point(
                origin.x + col as f32 * cell_width,
                origin.y + line as f32 * line_height_px,
            );
            let block = size(cell_width, line_height_px);
            let bounds = Bounds::new(pos, block);
            let marked = if !self.marked_text.is_empty() {
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
                    Some(cell_width),
                );
                Some(MarkedPrepaint {
                    bg_size: size(shaped.width().max(cell_width), line_height_px),
                    shaped,
                })
            } else {
                None
            };
            CursorPrepaint { bounds, marked }
        });

        PrepaintState {
            bounds,
            cell_width,
            line_height_px,
            background,
            search_rects,
            shaped_runs,
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
        for region in &prepaint.background {
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

        // Pre-shaped text runs — paint only, no shaping or allocation here.
        for (pos, shaped) in &prepaint.shaped_runs {
            let _ = shaped.paint(*pos, lh, TextAlign::Left, None, window, cx);
        }

        // Cursor block + inline IME marked (preedit) text.
        if let Some(cursor) = &prepaint.cursor {
            if let Some(marked) = &cursor.marked {
                // Paint the preedit highlight bg, then the shaped preedit line.
                window.paint_quad(fill(
                    Bounds::new(cursor.bounds.origin, marked.bg_size),
                    self.theme.cursor,
                ));
                let _ = marked.shaped.paint(
                    cursor.bounds.origin,
                    lh,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            } else {
                window.paint_quad(fill(cursor.bounds, self.theme.cursor));
            }

            // Register the IME input handler for this frame so the platform
            // routes composition events here, with the candidate window placed
            // at the cursor.
            window.handle_input(
                &self.focus_handle,
                TerminalInputHandler {
                    view: self.view.clone(),
                    cursor_bounds: Some(cursor.bounds),
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
