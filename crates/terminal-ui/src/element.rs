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

use std::cell::{Cell as SharedCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    App, BorderStyle, Bounds, DispatchPhase, Element, ElementId, Entity, FocusHandle, Font,
    FontFeatures, FontStyle, FontWeight, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, MouseUpEvent, Pixels, Point, ShapedLine, SharedString, Size, StrikethroughStyle,
    Style, TextAlign, TextRun, UnderlineStyle, Window, fill, outline, point, px, relative, rgba,
    size,
};
use terminal::alacritty_terminal::grid::Dimensions as _;
use terminal::alacritty_terminal::selection::SelectionRange;
use terminal::alacritty_terminal::vte::ansi::CursorShape;
use terminal::{Cell, Flags, HoverTarget, Terminal};

use crate::block_chars::{BlockRect, COLS, SUBROWS};
use crate::grid_renderer::{BackgroundRegion, BatchedTextRun, GridPlan, layout_grid};
use crate::layout_cache::{LineShapeCache, line_fingerprint};
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
    /// The hovered link/URL/path span, underlined in paint.
    pub hover: Option<HoverTarget>,
    /// The view's blink verdict for this frame; an invisible phase skips the
    /// cursor quad (but not IME preedit or the input-handler registration).
    pub cursor_visible: bool,
    /// Per-line shaped-run cache shared with the view (the element is rebuilt
    /// every render, so the cache lives there).
    pub shape_cache: Rc<RefCell<LineShapeCache<ShapedLine>>>,
    /// Written each prepaint: the scrollbar track bounds in window space,
    /// `None` without scrollback. The view hit-tests mouse input against it.
    pub scrollbar_track: Rc<SharedCell<Option<Bounds<Pixels>>>>,
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
    /// Pixel rects for the active text selection.
    selection_rects: Vec<Bounds<Pixels>>,
    /// Pixel rects for search matches; `true` = the active match.
    search_rects: Vec<(Bounds<Pixels>, bool)>,
    /// Pre-shaped text runs with their paint origin.
    shaped_runs: Vec<(Point<Pixels>, ShapedLine)>,
    /// Block-character cells as sub-grid rects.
    block_rects: Vec<BlockRect>,
    /// Cursor block, plus a pre-shaped preedit line when IME marked text is
    /// active. `None` when the terminal reports no cursor.
    cursor: Option<CursorPrepaint>,
    /// Scrollbar track + thumb rects; `None` without scrollback.
    scrollbar_track: Option<Bounds<Pixels>>,
    scrollbar_thumb: Option<Bounds<Pixels>>,
}

/// Everything prepaint needs from the locked Term, produced in a single
/// `read_with` pass (cell references cannot escape the lock).
struct TermSnapshot {
    background: Vec<BackgroundRegion>,
    runs: Vec<BatchedTextRun>,
    block_rects: Vec<BlockRect>,
    /// Content fingerprint per display line; keys the shaped-line cache.
    line_fps: HashMap<i32, u64>,
    /// Cursor position in display coordinates (line, column).
    cursor_grid: (i32, i32),
    cursor_shape: CursorShape,
    display_offset: i32,
    /// Scrollback line count; the scrollbar shows only when this is nonzero.
    history: usize,
    term_rows: i32,
    selection: Option<SelectionRange>,
}

/// Cursor paint data: the cell bounds, the glyph shape the program last
/// asked for (DECSCUSR / settings default), and a pre-shaped preedit line
/// when IME marked text is non-empty (`None` paints the plain cursor glyph).
pub struct CursorPrepaint {
    bounds: Bounds<Pixels>,
    shape: CursorShape,
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
            hover: None,
            cursor_visible: true,
            shape_cache: Rc::new(RefCell::new(LineShapeCache::new())),
            scrollbar_track: Rc::new(SharedCell::new(None)),
        }
    }

    /// Map alacritty's display iterator to `(display_line, grid_line, col,
    /// &Cell)`, assigning a 0-based display line by detecting line changes.
    /// The grid line (alacritty's scroll-stable coordinate) keys the
    /// shaped-line cache. Consumes the `RenderableContent` (GridIterator is
    /// not Clone).
    fn display_cells<'a>(
        mut content: terminal::RenderableContent<'a>,
    ) -> Vec<(i32, i32, usize, &'a Cell)> {
        let mut out: Vec<(i32, i32, usize, &Cell)> = Vec::new();
        let mut display_line = -1i32;
        let mut prev: Option<i32> = None;
        for idx in content.display_iter.by_ref() {
            let line = idx.point.line.0;
            if prev != Some(line) {
                display_line += 1;
                prev = Some(line);
            }
            out.push((display_line, line, idx.point.column.0, idx.cell));
        }
        out
    }

    /// Convert a `SelectionRange` (grid coordinates) to pixel rects, one per
    /// visible display line. For simple selection the column range varies per
    /// line; for block selection all lines share the same column range.
    fn selection_rects(
        selection: Option<SelectionRange>,
        offset: i32,
        rows: i32,
        cols: i32,
        bounds: Bounds<Pixels>,
        cell_w: Pixels,
        lh: Pixels,
    ) -> Vec<Bounds<Pixels>> {
        let Some(sel) = selection else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let start_line = sel.start.line.0;
        let end_line = sel.end.line.0;
        let block = sel.is_block;

        for grid_line in start_line..=end_line {
            let display_row = grid_line + offset;
            if !(0..rows).contains(&display_row) {
                continue;
            }
            let (start_col, end_col) = if block {
                (sel.start.column.0 as i32, sel.end.column.0 as i32)
            } else {
                let from = if grid_line == start_line {
                    sel.start.column.0 as i32
                } else {
                    0
                };
                let to = if grid_line == end_line {
                    sel.end.column.0 as i32
                } else {
                    // Middle lines span the full width. Clamp to cols-1 instead
                    // of i32::MAX to avoid overflow when computing the width.
                    cols - 1
                };
                (from, to)
            };
            let x = bounds.origin.x + start_col as f32 * cell_w;
            let y = bounds.origin.y + display_row as f32 * lh;
            let w = ((end_col - start_col + 1).max(1) as f32) * cell_w;
            out.push(Bounds::new(point(x, y), size(w, lh)));
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
        // Same-frame write-back (no notify) so the view's mouse handlers can
        // translate window-space positions into element-local coordinates.
        self.view.update(cx, |v, _| v.set_last_bounds(bounds));

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
        let snapshot = self.terminal.read_with(cx, |t, _cx| {
            t.with_term(|term| {
                let content = term.renderable_content();
                let selection = content.selection;
                let cursor_pt = content.cursor.point;
                let cursor_shape = content.cursor.shape;
                let offset = term.grid().display_offset() as i32;
                let history = term.grid().history_size();
                let cells = Self::display_cells(content);
                // Content fingerprint per display line — the shaped-line
                // cache key. Cells are line-major, so equal display lines
                // are contiguous.
                let mut line_fps: HashMap<i32, u64> = HashMap::new();
                let mut s = 0;
                while s < cells.len() {
                    let mut e = s + 1;
                    while e < cells.len() && cells[e].0 == cells[s].0 {
                        e += 1;
                    }
                    line_fps.insert(
                        cells[s].0,
                        line_fingerprint(cells[s..e].iter().map(|c| c.3)),
                    );
                    s = e;
                }
                let GridPlan {
                    background,
                    runs,
                    block_rects,
                } = layout_grid(
                    cells.iter().map(|(d, _g, c, cell)| (*d, *c, *cell)),
                    &self.theme,
                    t.block_char_render(),
                );
                TermSnapshot {
                    background,
                    runs,
                    block_rects,
                    line_fps,
                    cursor_grid: (cursor_pt.line.0 + offset, cursor_pt.column.0 as i32),
                    cursor_shape,
                    display_offset: offset,
                    history,
                    term_rows: t.rows as i32,
                    selection,
                }
            })
        });

        let selection_rects = Self::selection_rects(
            snapshot.selection,
            snapshot.display_offset,
            snapshot.term_rows,
            cols as i32,
            bounds,
            cell_width,
            line_height_px,
        );

        // Shape text runs line by line, reusing the cache when a line's
        // fingerprint is unchanged; paint positions are recomputed per frame
        // so scrolled content keeps its shaped glyphs.
        let mut cache = self.shape_cache.borrow_mut();
        let mut shaped_runs: Vec<(Point<Pixels>, ShapedLine)> =
            Vec::with_capacity(snapshot.runs.len());
        let runs = &snapshot.runs;
        let mut i = 0;
        while i < runs.len() {
            let line = runs[i].start_line;
            let mut j = i + 1;
            while j < runs.len() && runs[j].start_line == line {
                j += 1;
            }
            let grid_line = line - snapshot.display_offset;
            let fp = snapshot.line_fps.get(&line).copied().unwrap_or(0);
            if let Some(cached) = cache.get(grid_line, fp) {
                for (start_col, shaped) in cached {
                    let pos = point(
                        origin.x + start_col as f32 * cell_width,
                        origin.y + line as f32 * line_height_px,
                    );
                    shaped_runs.push((pos, shaped));
                }
            } else {
                let mut fresh: Vec<(i32, ShapedLine)> = Vec::with_capacity(j - i);
                for run in &runs[i..j] {
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
                    shaped_runs.push((pos, shaped.clone()));
                    fresh.push((run.start_col, shaped));
                }
                cache.insert(grid_line, fp, fresh);
            }
            i = j;
        }
        // Drop lines nothing touched this frame — the cache stays bounded by
        // the visible window as content scrolls.
        cache.sweep();
        drop(cache);

        let search_rects = Self::match_rects(
            &self.search_matches,
            self.active_match,
            snapshot.display_offset,
            snapshot.term_rows,
            bounds,
            cell_width,
            line_height_px,
        );

        // Scrollbar geometry, plus the track write-back the view hit-tests
        // against. No scrollback → no scrollbar.
        let (scrollbar_track, scrollbar_thumb) = if snapshot.history > 0 {
            let track = Bounds::new(
                point(
                    bounds.origin.x + bounds.size.width - px(2.),
                    bounds.origin.y,
                ),
                size(px(2.), bounds.size.height),
            );
            self.scrollbar_track.set(Some(track));
            let thumb = scrollbar_thumb(
                track,
                snapshot.history,
                snapshot.term_rows.max(0) as usize,
                snapshot.display_offset.max(0) as usize,
            );
            (Some(track), Some(thumb))
        } else {
            self.scrollbar_track.set(None);
            (None, None)
        };

        // Shape the IME preedit line here too; paint only emits the quads.
        let (cursor_line, cursor_col) = snapshot.cursor_grid;
        let cursor = {
            let pos = point(
                origin.x + cursor_col as f32 * cell_width,
                origin.y + cursor_line as f32 * line_height_px,
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
            CursorPrepaint {
                bounds,
                shape: snapshot.cursor_shape,
                marked,
            }
        };

        PrepaintState {
            bounds,
            cell_width,
            line_height_px,
            background: snapshot.background,
            selection_rects,
            search_rects,
            shaped_runs,
            block_rects: snapshot.block_rects,
            cursor: Some(cursor),
            scrollbar_track,
            scrollbar_thumb,
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

        // Active selection highlight (semi-transparent blue).
        for rect in &prepaint.selection_rects {
            window.paint_quad(fill(*rect, rgba(0x3366ff66)));
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

        // Hover target underline (OSC 8 link / URL / path), at the span's
        // bottom edge, in the theme's link-hover color.
        if let Some(hover) = &self.hover {
            let x = origin.x + hover.start_col as f32 * cell_w;
            let y = origin.y + (hover.row + 1) as f32 * lh - px(2.);
            let w = (hover.end_col - hover.start_col + 1) as f32 * cell_w;
            window.paint_quad(fill(
                Bounds::new(point(x, y), size(w, px(2.))),
                self.theme.link_hover,
            ));
        }

        // Block-character sub-grid rects, converted from grid coords to
        // pixels. Painted before the text runs — block cells never shape.
        for block in &prepaint.block_rects {
            let bx = origin.x + block.col as f32 * cell_w;
            let by = origin.y + block.line as f32 * lh;
            for r in &block.rects {
                let x = bx + r.x0 as f32 * cell_w / COLS as f32;
                let y = by + r.y0 as f32 * lh / SUBROWS as f32;
                let w = (r.x1 - r.x0) as f32 * cell_w / COLS as f32;
                let h = (r.y1 - r.y0) as f32 * lh / SUBROWS as f32;
                window.paint_quad(fill(Bounds::new(point(x, y), size(w, h)), r.color));
            }
        }

        // Pre-shaped text runs — paint only, no shaping or allocation here.
        for (pos, shaped) in &prepaint.shaped_runs {
            let _ = shaped.paint(*pos, lh, TextAlign::Left, None, window, cx);
        }

        // Scrollbar over the right edge: faint track + proportional thumb.
        if let (Some(track), Some(thumb)) = (prepaint.scrollbar_track, prepaint.scrollbar_thumb) {
            window.paint_quad(fill(track, rgba(0x80808026)));
            window.paint_quad(fill(thumb, rgba(0x80808073)));
        }

        // Cursor glyph + inline IME marked (preedit) text. The preedit paints
        // regardless of the blink phase; a blinked-out phase only skips the
        // cursor glyph, not the input-handler registration below.
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
            } else if self.cursor_visible {
                match cursor.shape {
                    CursorShape::Hidden => {}
                    CursorShape::Block => {
                        window.paint_quad(fill(cursor.bounds, self.theme.cursor));
                    }
                    CursorShape::Underline => {
                        let bar = Bounds::new(
                            point(cursor.bounds.origin.x, cursor.bounds.origin.y + lh - px(2.)),
                            size(cursor.bounds.size.width, px(2.)),
                        );
                        window.paint_quad(fill(bar, self.theme.cursor));
                    }
                    CursorShape::Beam => {
                        let bar = Bounds::new(
                            cursor.bounds.origin,
                            size(px(2.), cursor.bounds.size.height),
                        );
                        window.paint_quad(fill(bar, self.theme.cursor));
                    }
                    CursorShape::HollowBlock => {
                        window.paint_quad(outline(
                            cursor.bounds,
                            self.theme.cursor,
                            BorderStyle::Solid,
                        ));
                    }
                }
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

        // While a selection or scrollbar drag is in flight, finalize on
        // mouse-up anywhere in the window: releases outside the terminal div
        // never reach the div's own `on_mouse_up`.
        if self
            .view
            .read_with(cx, |v, _| v.is_selecting() || v.is_scrollbar_dragging())
        {
            let view = self.view.clone();
            window.on_mouse_event(move |_: &MouseUpEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                view.update(cx, |v, cx| {
                    v.finalize_selection(cx);
                    v.end_scrollbar_drag();
                });
            });
        }
    }
}

/// Scrollbar thumb rect within `track` for a buffer of `history` + `rows`
/// lines with the display scrolled `offset` lines up from the live edge.
/// Thumb height is the visible fraction of the buffer (minimum 20px);
/// `offset == history` puts the thumb at the track top, `offset == 0` at the
/// bottom.
fn scrollbar_thumb(
    track: Bounds<Pixels>,
    history: usize,
    rows: usize,
    offset: usize,
) -> Bounds<Pixels> {
    let track_h = f32::from(track.size.height);
    let total = (history + rows).max(1) as f32;
    let thumb_h = (track_h * rows as f32 / total).max(20.).min(track_h);
    let max_top = (track_h - thumb_h).max(0.);
    let frac = if history == 0 {
        0.
    } else {
        1. - offset.min(history) as f32 / history as f32
    };
    Bounds::new(
        point(track.origin.x, track.origin.y + px(max_top * frac)),
        size(track.size.width, px(thumb_h)),
    )
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollbar_thumb_maps_offset_extremes() {
        let track = Bounds::new(point(px(100.), px(10.)), size(px(2.), px(200.)));
        // history 100 + rows 100 → the thumb covers half the track.
        let at_live = scrollbar_thumb(track, 100, 100, 0);
        assert_eq!(at_live.size.height, px(100.));
        assert_eq!(at_live.origin.y, px(110.));
        let at_oldest = scrollbar_thumb(track, 100, 100, 100);
        assert_eq!(at_oldest.origin.y, px(10.));
    }

    #[test]
    fn scrollbar_thumb_has_minimum_height() {
        let track = Bounds::new(point(px(0.), px(0.)), size(px(2.), px(100.)));
        // 10_000 lines of history with 50 visible → natural thumb ≈ 0.5px.
        let thumb = scrollbar_thumb(track, 10_000, 50, 0);
        assert_eq!(thumb.size.height, px(20.));
        assert_eq!(thumb.origin.y, px(80.));
    }

    #[test]
    fn scrollbar_thumb_fills_track_without_history() {
        let track = Bounds::new(point(px(0.), px(0.)), size(px(2.), px(100.)));
        let thumb = scrollbar_thumb(track, 0, 50, 0);
        assert_eq!(thumb.size.height, px(100.));
        assert_eq!(thumb.origin.y, px(0.));
    }
}
