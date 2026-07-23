//! `TerminalElement` — manox's first gpui `Element`.

use gpui::{
    App, Bounds, Element, ElementId, Entity, FocusHandle, Font, FontFeatures, FontStyle,
    FontWeight, GlobalElementId, InspectorElementId, IntoElement, LayoutId, Pixels, Point,
    ShapedLine, SharedString, Size, StrikethroughStyle, Style, TextAlign, TextRun, UnderlineStyle,
    Window, fill, point, px, relative, size,
};
use rmux_core::Screen;
use rmux_core::ScreenCellView;
use terminal::Terminal;

use crate::grid_renderer::{GridPlan, layout_grid};
use crate::terminal_view::{TerminalInputHandler, TerminalView};
use crate::theme::TerminalTheme;

pub struct TerminalElement {
    pub terminal: Entity<Terminal>,
    pub view: Entity<TerminalView>,
    pub focus_handle: FocusHandle,
    pub theme: TerminalTheme,
    pub font: Font,
    pub font_size: Pixels,
    pub line_height: f32,
    pub marked_text: SharedString,
}

pub struct PrepaintState {
    bounds: Bounds<Pixels>,
    cell_width: Pixels,
    line_height_px: Pixels,
    background: Vec<crate::grid_renderer::BackgroundRegion>,
    shaped_runs: Vec<(Point<Pixels>, ShapedLine)>,
    cursor: Option<CursorPrepaint>,
}

struct CursorPrepaint {
    bounds: Bounds<Pixels>,
    marked: Option<MarkedPrepaint>,
}

struct MarkedPrepaint {
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
        }
    }

    // Collect visible cells via absolute_line_view (owned ScreenCellView).
    fn collect_cells(
        screen: &Screen,
        rows: usize,
        cols: usize,
    ) -> Vec<(i32, usize, ScreenCellView)> {
        let mut out: Vec<(i32, usize, ScreenCellView)> = Vec::with_capacity(rows * cols);
        let history = screen.history_size();
        for row in 0..rows {
            if let Some(line) = screen.absolute_line_view(history + row) {
                for (col, cell) in line.cells().iter().enumerate().take(cols) {
                    out.push((row as i32, col, cell.clone()));
                }
            }
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

        let origin = bounds.origin;
        let (background, runs, cursor_grid) = self.terminal.read_with(cx, |t, _cx| {
            t.with_screen(|screen| {
                let (cursor_x, cursor_y) = screen.cursor_position();
                let cells = Self::collect_cells(screen, t.rows, t.cols);
                let GridPlan { background, runs } = layout_grid(cells.into_iter(), &self.theme);
                (background, runs, Some((cursor_y as i32, cursor_x as i32)))
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
                underline: run.underline.then(UnderlineStyle::default),
                strikethrough: run.strikethrough.then(StrikethroughStyle::default),
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(run.text.as_str()),
                self.font_size,
                std::slice::from_ref(&text_run),
                Some(cell_width),
            );
            shaped_runs.push((pos, shaped));
        }

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

        window.paint_quad(fill(prepaint.bounds, self.theme.default_bg));

        for region in &prepaint.background {
            let x = origin.x + region.start_col as f32 * cell_w;
            let y = origin.y + region.start_line as f32 * lh;
            let w = (region.end_col - region.start_col + 1) as f32 * cell_w;
            let h = (region.end_line - region.start_line + 1) as f32 * lh;
            window.paint_quad(fill(Bounds::new(point(x, y), size(w, h)), region.color));
        }

        for (pos, shaped) in &prepaint.shaped_runs {
            let _ = shaped.paint(*pos, lh, TextAlign::Left, None, window, cx);
        }

        if let Some(cursor) = &prepaint.cursor {
            if let Some(marked) = &cursor.marked {
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
