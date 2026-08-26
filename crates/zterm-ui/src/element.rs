use std::mem;
use std::ops::Range as StdRange;

use gpui::{
    AbsoluteLength, App, Bounds, ContentMask, DispatchPhase, Element, Entity, FocusHandle, Font,
    FontFeatures, FontStyle, FontWeight, GlobalElementId, Hsla, IntoElement, InputHandler,
    LayoutId, MouseMoveEvent, Pixels, Point as GpuiPoint, Rgba, ShapedLine, StrikethroughStyle,
    Style, TextRun, TextStyle, UTF16Selection, UnderlineStyle, WhiteSpace, Window, fill, point,
    px, relative, size,
};
use zterm_core::{
    Cell, Color, CursorShape, IndexedCell, Modes, NamedColor, Point, Range, SelectionRange,
    Terminal, TerminalBounds, is_default_background_color,
};
use util::ResultExt;

const FONT_FAMILY: &str = "Menlo";
const FONT_SIZE: Pixels = px(14.0);
const LINE_HEIGHT_FACTOR: f32 = 1.618;
const SELECTION_COLOR: Rgba = Rgba {
    r: 0x26 as f32 / 255.0,
    g: 0x4F as f32 / 255.0,
    b: 0x78 as f32 / 255.0,
    a: 1.0,
};

/// The information generated during layout that is necessary for painting.
pub struct LayoutState {
    batched_text_runs: Vec<BatchedTextRun>,
    block_element_rects: Vec<BlockElementLayoutRect>,
    rects: Vec<LayoutRect>,
    selection: Option<SelectionRange>,
    cursor: Option<TerminalCursor>,
    cursor_visible: bool,
    hovered: Option<Range>,
    matches: Vec<Range>,
    background_color: Hsla,
    dimensions: TerminalBounds,
    display_offset: usize,
}

/// Helper struct for converting terminal cursor points to displayed cursor points.
#[derive(Copy, Clone)]
struct DisplayCursor {
    line: i32,
    col: usize,
}

impl DisplayCursor {
    fn from(cursor_point: Point, display_offset: usize) -> Self {
        Self {
            line: cursor_point.line + display_offset as i32,
            col: cursor_point.column,
        }
    }

    pub fn line(&self) -> i32 {
        self.line
    }

    pub fn col(&self) -> usize {
        self.col
    }
}

#[derive(Copy, Clone, Debug, Default)]
pub struct LayoutPoint {
    line: i32,
    column: i32,
}

impl LayoutPoint {
    fn new(line: i32, column: i32) -> Self {
        Self { line, column }
    }
}

/// A batched text run that combines multiple adjacent cells with the same style
#[derive(Debug)]
pub struct BatchedTextRun {
    pub start_point: LayoutPoint,
    pub text: String,
    pub cell_count: usize,
    pub style: TextRun,
    pub font_size: AbsoluteLength,
}

impl BatchedTextRun {
    fn new_from_char(
        start_point: LayoutPoint,
        c: char,
        style: TextRun,
        font_size: AbsoluteLength,
    ) -> Self {
        let mut text = String::with_capacity(100); // Pre-allocate for typical line length
        text.push(c);
        BatchedTextRun {
            start_point,
            text,
            cell_count: 1,
            style,
            font_size,
        }
    }

    fn can_append(&self, other_style: &TextRun) -> bool {
        self.style.font == other_style.font
            && self.style.color == other_style.color
            && self.style.background_color == other_style.background_color
            && self.style.underline == other_style.underline
            && self.style.strikethrough == other_style.strikethrough
    }

    fn append_char(&mut self, c: char) {
        self.append_char_internal(c, true);
    }

    fn append_zero_width_chars(&mut self, chars: &[char]) {
        for &c in chars {
            self.append_char_internal(c, false);
        }
    }

    fn append_char_internal(&mut self, c: char, counts_cell: bool) {
        self.text.push(c);
        if counts_cell {
            self.cell_count += 1;
        }
        self.style.len += c.len_utf8();
    }

    pub fn paint(
        &self,
        origin: GpuiPoint<Pixels>,
        dimensions: &TerminalBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let pos = GpuiPoint::new(
            origin.x + self.start_point.column as f32 * dimensions.cell_width,
            origin.y + self.start_point.line as f32 * dimensions.line_height,
        );
        window
            .text_system()
            .shape_line(
                self.text.clone().into(),
                self.font_size.to_pixels(window.rem_size()),
                std::slice::from_ref(&self.style),
                None,
            )
            .paint(
                pos,
                dimensions.line_height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            )
            .log_err();
    }
}

/// Block element glyphs are painted on a subcell grid: each terminal cell is
/// divided into 8 columns (for eighth blocks) and 24 lines (LCM of the 8-way
/// splits of eighth blocks and the 3-way splits of sextants).
const BLOCK_SUBCELL_COLUMNS: i32 = 8;
const BLOCK_SUBCELL_LINES: i32 = 24;

#[derive(Clone, Debug)]
pub struct BlockElementLayoutRect {
    point: LayoutPoint,
    num_of_columns: usize,
    num_of_lines: usize,
    color: Hsla,
}

impl BlockElementLayoutRect {
    fn new(point: LayoutPoint, num_of_columns: usize, num_of_lines: usize, color: Hsla) -> Self {
        Self {
            point,
            num_of_columns,
            num_of_lines,
            color,
        }
    }

    pub fn paint(
        &self,
        origin: GpuiPoint<Pixels>,
        dimensions: &TerminalBounds,
        window: &mut Window,
    ) {
        let subcell_width = dimensions.cell_width / BLOCK_SUBCELL_COLUMNS as f32;
        let subcell_height = dimensions.line_height / BLOCK_SUBCELL_LINES as f32;
        let position = point(
            origin.x + self.point.column as f32 * subcell_width,
            origin.y + self.point.line as f32 * subcell_height,
        );
        let size = size(
            subcell_width * self.num_of_columns as f32,
            subcell_height * self.num_of_lines as f32,
        );

        window.paint_quad(fill(Bounds::new(position, size), self.color));
    }
}

#[derive(Clone, Debug, Default)]
pub struct LayoutRect {
    point: LayoutPoint,
    num_of_cells: usize,
    color: Hsla,
}

impl LayoutRect {
    fn new(point: LayoutPoint, num_of_cells: usize, color: Hsla) -> LayoutRect {
        LayoutRect {
            point,
            num_of_cells,
            color,
        }
    }

    pub fn paint(
        &self,
        origin: GpuiPoint<Pixels>,
        dimensions: &TerminalBounds,
        window: &mut Window,
    ) {
        let position = {
            let layout_point = self.point;
            point(
                (origin.x + layout_point.column as f32 * dimensions.cell_width).floor(),
                origin.y + layout_point.line as f32 * dimensions.line_height,
            )
        };
        let size = point(
            (dimensions.cell_width * self.num_of_cells as f32).ceil(),
            dimensions.line_height,
        )
        .into();

        window.paint_quad(fill(Bounds::new(position, size), self.color));
    }
}

/// Represents a rectangular region with a specific color on a logical grid.
#[derive(Debug, Clone)]
struct BackgroundRegion {
    start_line: i32,
    start_col: i32,
    end_line: i32,
    end_col: i32,
    color: Hsla,
}

impl BackgroundRegion {
    fn new(line: i32, col: i32, color: Hsla) -> Self {
        BackgroundRegion {
            start_line: line,
            start_col: col,
            end_line: line,
            end_col: col,
            color,
        }
    }

    fn with_extents(
        start_line: i32,
        start_col: i32,
        end_line: i32,
        end_col: i32,
        color: Hsla,
    ) -> Self {
        BackgroundRegion {
            start_line,
            start_col,
            end_line,
            end_col,
            color,
        }
    }

    /// Check if this region can be merged with another region
    fn can_merge_with(&self, other: &BackgroundRegion) -> bool {
        if self.color != other.color {
            return false;
        }

        // Check if regions are adjacent horizontally
        if self.start_line == other.start_line && self.end_line == other.end_line {
            return self.end_col + 1 == other.start_col || other.end_col + 1 == self.start_col;
        }

        // Check if regions are adjacent vertically with same column span
        if self.start_col == other.start_col && self.end_col == other.end_col {
            return self.end_line + 1 == other.start_line || other.end_line + 1 == self.start_line;
        }

        false
    }

    /// Merge this region with another region
    fn merge_with(&mut self, other: &BackgroundRegion) {
        self.start_line = self.start_line.min(other.start_line);
        self.start_col = self.start_col.min(other.start_col);
        self.end_line = self.end_line.max(other.end_line);
        self.end_col = self.end_col.max(other.end_col);
    }
}

pub trait TerminalLayoutCell {
    fn point(&self) -> Point;
    fn cell(&self) -> &Cell;
}

impl TerminalLayoutCell for IndexedCell {
    fn point(&self) -> Point {
        self.point
    }

    fn cell(&self) -> &Cell {
        &self.cell
    }
}

impl TerminalLayoutCell for &IndexedCell {
    fn point(&self) -> Point {
        self.point
    }

    fn cell(&self) -> &Cell {
        &self.cell
    }
}

/// Merge grid regions to minimize the number of rectangles.
fn merge_background_regions(regions: Vec<BackgroundRegion>) -> Vec<BackgroundRegion> {
    if regions.is_empty() {
        return regions;
    }

    let mut merged = regions;
    let mut changed = true;

    // Keep merging until no more merges are possible
    while changed {
        changed = false;
        let mut i = 0;

        while i < merged.len() {
            let mut j = i + 1;
            while j < merged.len() {
                if merged[i].can_merge_with(&merged[j]) {
                    let other = merged.remove(j);
                    merged[i].merge_with(&other);
                    changed = true;
                } else {
                    j += 1;
                }
            }
            i += 1;
        }
    }

    merged
}

/// The cursor as laid out during prepaint, to be painted as plain quads.
struct TerminalCursor {
    /// Bounds relative to the terminal grid origin.
    bounds: Bounds<Pixels>,
    shape: CursorShape,
    /// The character under a block cursor, shaped in the background color.
    text: Option<ShapedLine>,
}

impl TerminalCursor {
    fn paint(
        &self,
        origin: GpuiPoint<Pixels>,
        dimensions: &TerminalBounds,
        window: &mut Window,
        cx: &mut App,
    ) {
        let bounds = self.bounds + origin;
        let color: Hsla = zterm_core::TERMINAL_FOREGROUND.into();
        match self.shape {
            CursorShape::Block => {
                window.paint_quad(fill(bounds, color));
                if let Some(text) = &self.text {
                    text.paint(
                        bounds.origin,
                        dimensions.line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    )
                    .log_err();
                }
            }
            CursorShape::Bar => {
                let bar_width = px(2.0).min(bounds.size.width);
                window.paint_quad(fill(
                    Bounds::new(bounds.origin, size(bar_width, bounds.size.height)),
                    color,
                ));
            }
            CursorShape::Underline => {
                let underline_height = px(2.0).min(bounds.size.height);
                window.paint_quad(fill(
                    Bounds::new(
                        point(
                            bounds.origin.x,
                            bounds.origin.y + bounds.size.height - underline_height,
                        ),
                        size(bounds.size.width, underline_height),
                    ),
                    color,
                ));
            }
            CursorShape::HollowBlock => {
                let thickness = px(1.0);
                // Top and bottom edges.
                window.paint_quad(fill(
                    Bounds::new(bounds.origin, size(bounds.size.width, thickness)),
                    color,
                ));
                window.paint_quad(fill(
                    Bounds::new(
                        point(
                            bounds.origin.x,
                            bounds.origin.y + bounds.size.height - thickness,
                        ),
                        size(bounds.size.width, thickness),
                    ),
                    color,
                ));
                // Left and right edges.
                window.paint_quad(fill(
                    Bounds::new(bounds.origin, size(thickness, bounds.size.height)),
                    color,
                ));
                window.paint_quad(fill(
                    Bounds::new(
                        point(
                            bounds.origin.x + bounds.size.width - thickness,
                            bounds.origin.y,
                        ),
                        size(thickness, bounds.size.height),
                    ),
                    color,
                ));
            }
            CursorShape::Hidden => {}
        }
    }
}

pub struct TerminalElement {
    terminal: Entity<Terminal>,
    view: Entity<crate::view::TerminalView>,
    focus: FocusHandle,
    focused: bool,
    marked_text: Option<String>,
}

impl TerminalElement {
    pub fn new(
        terminal: Entity<Terminal>,
        view: Entity<crate::view::TerminalView>,
        focus: FocusHandle,
        focused: bool,
        marked_text: Option<String>,
    ) -> TerminalElement {
        TerminalElement {
            terminal,
            view,
            focus,
            focused,
            marked_text,
        }
    }
    pub fn layout_grid<T: TerminalLayoutCell>(
        grid: impl Iterator<Item = T>,
        start_line_offset: i32,
        text_style: &TextStyle,
    ) -> (
        Vec<LayoutRect>,
        Vec<BatchedTextRun>,
        Vec<BlockElementLayoutRect>,
    ) {
        // Pre-allocate with estimated capacity to reduce reallocations
        let estimated_cells = grid.size_hint().0;
        let estimated_runs = estimated_cells / 10; // Estimate ~10 cells per run
        let estimated_regions = estimated_cells / 20; // Estimate ~20 cells per background region

        let mut batched_runs = Vec::with_capacity(estimated_runs);
        let mut block_element_regions = Vec::new();

        // Collect background regions for efficient merging
        let mut background_regions: Vec<BackgroundRegion> = Vec::with_capacity(estimated_regions);
        let mut current_batch: Option<BatchedTextRun> = None;

        // Renderable cells arrive in row-major order; track line changes to
        // flush text batches at line boundaries, as chunk_by groups would.
        let mut line_index: i32 = -1;
        let mut current_line: Option<i32> = None;
        let mut previous_cell_had_extras = false;

        for cell in grid {
            let point = cell.point();
            let cell = cell.cell();

            if current_line != Some(point.line) {
                if let Some(batch) = current_batch.take() {
                    batched_runs.push(batch);
                }
                current_line = Some(point.line);
                line_index += 1;
                previous_cell_had_extras = false;
            }
            let display_line = start_line_offset + line_index;

            let mut fg = cell.foreground();
            let mut bg = cell.background();
            if cell.is_inverse() {
                mem::swap(&mut fg, &mut bg);
            }

            // Collect background regions (skip default background)
            if !is_default_background_color(bg) {
                let color = convert_color(&bg);
                let col = point.column as i32;

                // Try to extend the last region if it's on the same line with the same color
                if let Some(last_region) = background_regions.last_mut()
                    && last_region.color == color
                    && last_region.start_line == display_line
                    && last_region.end_line == display_line
                    && last_region.end_col + 1 == col
                {
                    last_region.end_col = col;
                } else {
                    background_regions.push(BackgroundRegion::new(display_line, col, color));
                }
            }
            // Skip wide character spacers - they're just placeholders for the second cell of wide characters
            if cell.is_wide_char_spacer() {
                continue;
            }

            // Skip spaces that follow cells with extras (emoji variation sequences)
            if cell.character() == ' ' && previous_cell_had_extras {
                previous_cell_had_extras = false;
                continue;
            }
            // Update tracking for next iteration
            previous_cell_had_extras =
                matches!(cell.zerowidth(), Some(chars) if !chars.is_empty());

            //Layout current cell text
            if !is_blank(cell) {
                let cell_style = TerminalElement::cell_style(cell, fg, text_style);

                let cell_point = LayoutPoint::new(display_line, point.column as i32);
                if Self::collect_block_element_regions(
                    cell_point,
                    cell.character(),
                    cell_style.color,
                    &mut block_element_regions,
                ) {
                    if let Some(batch) = current_batch.take() {
                        batched_runs.push(batch);
                    }
                    continue;
                }

                let zero_width_chars = cell.zerowidth();

                // Try to batch with existing run
                if let Some(ref mut batch) = current_batch {
                    if batch.can_append(&cell_style)
                        && batch.start_point.line == cell_point.line
                        && batch.start_point.column + batch.cell_count as i32 == cell_point.column
                    {
                        batch.append_char(cell.character());
                        if let Some(chars) = zero_width_chars {
                            batch.append_zero_width_chars(chars);
                        }
                    } else {
                        // Flush current batch and start new one
                        if let Some(old_batch) = current_batch.take() {
                            batched_runs.push(old_batch);
                        }
                        let mut new_batch = BatchedTextRun::new_from_char(
                            cell_point,
                            cell.character(),
                            cell_style,
                            text_style.font_size,
                        );
                        if let Some(chars) = zero_width_chars {
                            new_batch.append_zero_width_chars(chars);
                        }
                        current_batch = Some(new_batch);
                    }
                } else {
                    // Start new batch
                    let mut new_batch = BatchedTextRun::new_from_char(
                        cell_point,
                        cell.character(),
                        cell_style,
                        text_style.font_size,
                    );
                    if let Some(chars) = zero_width_chars {
                        new_batch.append_zero_width_chars(chars);
                    }
                    current_batch = Some(new_batch);
                }
            }
        }

        // Flush any remaining batch
        if let Some(batch) = current_batch {
            batched_runs.push(batch);
        }

        // Merge background regions and convert to layout rects.
        // Since LayoutRect only supports single-line rectangles, split multi-line regions.
        let merged_regions = merge_background_regions(background_regions);
        let mut rects = Vec::with_capacity(merged_regions.len());
        for region in merged_regions {
            for line in region.start_line..=region.end_line {
                rects.push(LayoutRect::new(
                    LayoutPoint::new(line, region.start_col),
                    (region.end_col - region.start_col + 1) as usize,
                    region.color,
                ));
            }
        }

        let block_element_rects = Self::block_element_regions_to_rects(block_element_regions);

        (rects, batched_runs, block_element_rects)
    }

    /// Computes the cursor position based on the cursor point and terminal dimensions.
    fn cursor_position(
        cursor_point: DisplayCursor,
        size: &TerminalBounds,
    ) -> Option<GpuiPoint<Pixels>> {
        if cursor_point.line() < size.num_lines() as i32 {
            // When on pixel boundaries round the origin down
            Some(point(
                (cursor_point.col() as f32 * size.cell_width()).floor(),
                (cursor_point.line() as f32 * size.line_height()).floor(),
            ))
        } else {
            None
        }
    }

    /// Returns the filled subcells of a sextant character as a bitmap, where
    /// bit `row * 2 + column` is set when that 2x3 subcell is filled.
    ///
    /// U+1FB00..=U+1FB3B enumerate all 2x3 fill combinations except the four
    /// that already exist as Block Elements (empty, `▌` = 0b010101,
    /// `▐` = 0b101010, and `█` = 0b111111), hence the gap adjustments.
    fn sextant_char_to_filled_bits(ch: char) -> Option<u8> {
        let offset = (ch as u32).checked_sub(0x1FB00)?;
        if offset > 0x3B {
            return None;
        }

        Some((offset + 1 + u32::from(offset >= 20) + u32::from(offset >= 40)) as u8)
    }

    /// Returns the filled quadrants of a quadrant character as a bitmap, where
    /// bit `row * 2 + column` is set when that 2x2 subcell is filled.
    fn quadrant_char_to_filled_bits(ch: char) -> Option<u8> {
        Some(match ch {
            '▘' => 0b0001,
            '▝' => 0b0010,
            '▖' => 0b0100,
            '▗' => 0b1000,
            '▚' => 0b1001,
            '▞' => 0b0110,
            '▛' => 0b0111,
            '▜' => 0b1011,
            '▙' => 0b1101,
            '▟' => 0b1110,
            _ => return None,
        })
    }

    /// Returns `(column, line, num_of_columns, num_of_lines)` in subcell units
    /// for block element characters that consist of a single rectangle.
    fn block_char_to_rect(ch: char) -> Option<(i32, i32, i32, i32)> {
        let codepoint = ch as u32;
        Some(match codepoint {
            // ▀ upper half
            0x2580 => (0, 0, 8, 12),
            // ▁▂▃▄▅▆▇█ lower blocks of 1..=8 eighths
            0x2581..=0x2588 => {
                let eighths = (codepoint - 0x2580) as i32;
                (0, 24 - eighths * 3, 8, eighths * 3)
            }
            // ▉▊▋▌▍▎▏ left blocks of 7..=1 eighths
            0x2589..=0x258F => (0, 0, (0x2590 - codepoint) as i32, 24),
            // ▐ right half
            0x2590 => (4, 0, 4, 24),
            // ▔ upper eighth
            0x2594 => (0, 0, 8, 3),
            // ▕ right eighth
            0x2595 => (7, 0, 1, 24),
            _ => return None,
        })
    }

    /// Approximates the shade characters `░▒▓` with the foreground color at
    /// reduced opacity instead of the stipple patterns fonts use, trading
    /// pattern fidelity for seamless cell coverage.
    fn shade_char_to_opacity(ch: char) -> Option<f32> {
        match ch {
            '░' => Some(0.25),
            '▒' => Some(0.5),
            '▓' => Some(0.75),
            _ => None,
        }
    }

    fn collect_block_element_regions(
        point: LayoutPoint,
        ch: char,
        color: Hsla,
        regions: &mut Vec<BackgroundRegion>,
    ) -> bool {
        if let Some((column, line, num_of_columns, num_of_lines)) = Self::block_char_to_rect(ch) {
            Self::push_block_element_region(
                point,
                column,
                line,
                num_of_columns,
                num_of_lines,
                color,
                regions,
            );
            return true;
        }

        if let Some(filled) = Self::quadrant_char_to_filled_bits(ch) {
            for row in 0..2 {
                for column in 0..2 {
                    if filled & (1 << (row * 2 + column)) != 0 {
                        Self::push_block_element_region(
                            point,
                            column * 4,
                            row * 12,
                            4,
                            12,
                            color,
                            regions,
                        );
                    }
                }
            }
            return true;
        }

        if let Some(filled) = Self::sextant_char_to_filled_bits(ch) {
            for row in 0..3 {
                for column in 0..2 {
                    if filled & (1 << (row * 2 + column)) != 0 {
                        Self::push_block_element_region(
                            point,
                            column * 4,
                            row * 8,
                            4,
                            8,
                            color,
                            regions,
                        );
                    }
                }
            }
            return true;
        }

        if let Some(opacity) = Self::shade_char_to_opacity(ch) {
            Self::push_block_element_region(point, 0, 0, 8, 24, color.opacity(opacity), regions);
            return true;
        }

        false
    }

    fn push_block_element_region(
        point: LayoutPoint,
        column: i32,
        line: i32,
        num_of_columns: i32,
        num_of_lines: i32,
        color: Hsla,
        regions: &mut Vec<BackgroundRegion>,
    ) {
        let start_line = point.line * BLOCK_SUBCELL_LINES + line;
        let start_col = point.column * BLOCK_SUBCELL_COLUMNS + column;
        let end_line = start_line + num_of_lines - 1;
        let end_col = start_col + num_of_columns - 1;

        // Extend the previous region when possible (e.g. runs of `█` in a QR
        // code) to keep the quadratic merge pass over a small input.
        if let Some(last_region) = regions.last_mut()
            && last_region.color == color
            && last_region.start_line == start_line
            && last_region.end_line == end_line
            && last_region.end_col + 1 == start_col
        {
            last_region.end_col = end_col;
            return;
        }

        regions.push(BackgroundRegion::with_extents(
            start_line, start_col, end_line, end_col, color,
        ));
    }

    fn block_element_regions_to_rects(
        regions: Vec<BackgroundRegion>,
    ) -> Vec<BlockElementLayoutRect> {
        merge_background_regions(regions)
            .into_iter()
            .map(|region| {
                BlockElementLayoutRect::new(
                    LayoutPoint::new(region.start_line, region.start_col),
                    (region.end_col - region.start_col + 1) as usize,
                    (region.end_line - region.start_line + 1) as usize,
                    region.color,
                )
            })
            .collect()
    }

    /// Converts the Alacritty cell styles to GPUI text styles.
    fn cell_style(cell: &Cell, fg: Color, text_style: &TextStyle) -> TextRun {
        let mut fg = convert_color(&fg);

        // Use a dim multiplier that stays close to the existing Alacritty look.
        if cell.is_dim() {
            fg.a *= 0.7;
        }

        let underline = cell.has_underline().then(|| UnderlineStyle {
            color: Some(fg),
            thickness: Pixels::from(1.0),
            wavy: cell.has_undercurl(),
        });

        let strikethrough = cell.has_strikeout().then(|| StrikethroughStyle {
            color: Some(fg),
            thickness: Pixels::from(1.0),
        });

        let weight = if cell.is_bold() {
            FontWeight::BOLD
        } else {
            text_style.font_weight
        };

        let style = if cell.is_italic() {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        };

        TextRun {
            len: cell.character().len_utf8(),
            color: fg,
            background_color: None,
            font: Font {
                weight,
                style,
                ..text_style.font()
            },
            underline,
            strikethrough,
        }
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = LayoutState;

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        let layout_id = window.request_layout(style, None, cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {

        let empty_layout = LayoutState {
            batched_text_runs: Vec::new(),
            block_element_rects: Vec::new(),
            rects: Vec::new(),
            selection: None,
            cursor: None,
            cursor_visible: true,
            matches: Vec::new(),
            hovered: None,
            background_color: zterm_core::TERMINAL_BACKGROUND.into(),
            dimensions: TerminalBounds::default(),
            display_offset: 0,
        };

        let text_style = TextStyle {
            font_family: FONT_FAMILY.into(),
            font_features: FontFeatures::disable_ligatures(),
            font_weight: FontWeight::default(),
            font_fallbacks: None,
            font_size: FONT_SIZE.into(),
            font_style: FontStyle::Normal,
            line_height: px(LINE_HEIGHT_FACTOR).into(),
            background_color: Some(zterm_core::TERMINAL_BACKGROUND.into()),
            white_space: WhiteSpace::Normal,
            // These are going to be overridden per-cell
            color: zterm_core::TERMINAL_FOREGROUND.into(),
            ..Default::default()
        };

        let text_system = cx.text_system();
        let rem_size = window.rem_size();
        let font_pixels = text_style.font_size.to_pixels(rem_size);
        let line_height = f32::from(font_pixels) * LINE_HEIGHT_FACTOR;
        let font_id = text_system.resolve_font(&text_style.font());

        let Some(cell_width) = text_system
            .advance(font_id, font_pixels, 'm')
            .log_err()
            .map(|advance| advance.width)
        else {
            return empty_layout;
        };

        let dimensions = {
            let mut size = bounds.size;
            size.width -= cell_width;

            // https://github.com/zed-industries/zed/issues/2750
            // if the terminal is one column wide, rendering 🦀
            // causes alacritty to misbehave.
            if size.width < cell_width * 2.0 {
                size.width = cell_width * 2.0;
            }

            let mut origin = bounds.origin;
            origin.x += cell_width;

            let content = self.terminal.read(cx).last_content();
            let should_anchor_to_bottom = content.mode.contains(Modes::ALT_SCREEN)
                || (content.scrolled_to_bottom && content.bottom_row_occupied);

            // Snap the height to whole rows on device pixel boundaries so a
            // fractional row never consumes space or triggers a reflow.
            let available_height = size.height;
            let scale_factor = window.scale_factor();
            let line_height_pixels = px(line_height);
            let line_height_device_px = (f32::from(line_height_pixels) * scale_factor)
                .round()
                .max(1.0) as i32;
            let available_height_device_px = (f32::from(available_height) * scale_factor)
                .floor()
                .max(0.0) as i32;

            let rows = ((available_height_device_px / line_height_device_px) as usize).max(1);
            let snapped_height_device_px = (rows as i32) * line_height_device_px;
            let padding_device_px = (available_height_device_px - snapped_height_device_px).max(0);

            let snapped_height = px(snapped_height_device_px as f32 / scale_factor.max(1.0));
            let padding = px(padding_device_px as f32 / scale_factor.max(1.0));

            size.height = snapped_height;
            if should_anchor_to_bottom {
                origin.y += padding;
            }

            // Snap to device pixels to avoid subpixel jitter while resizing.
            // Terminal rendering is grid-based; allowing fractional origins can cause the
            // glyph rasterization to shift between frames, which looks like flicker.
            let snap_px = |value: Pixels| {
                Pixels::from((f32::from(value) * scale_factor).floor() / scale_factor)
            };
            origin.x = snap_px(origin.x);
            origin.y = snap_px(origin.y);

            TerminalBounds::new(px(line_height), cell_width, Bounds { origin, size })
        };

        self.terminal.update(cx, |terminal, cx| {
            terminal.set_size(dimensions);
            terminal.sync(window, cx);
        });

        let (rects, batched_text_runs, block_element_rects) = self
            .terminal
            .read(cx)
            .with_renderable_cells(|cells| TerminalElement::layout_grid(cells, 0, &text_style));

        let content = self.terminal.read(cx).last_content();
        let display_offset = content.display_offset;
        let selection = content.selection;
        let cursor = content.cursor;
        let cursor_char = content.cursor_char;

        // Layout cursor. Shape the character under a block cursor so it can be
        // painted in the background color over the filled block.
        let cursor_point = DisplayCursor::from(cursor.point, display_offset);
        let cursor_text = {
            let cursor_text = cursor_char.to_string();
            let len = cursor_text.len();
            window.text_system().shape_line(
                cursor_text.into(),
                text_style.font_size.to_pixels(window.rem_size()),
                &[TextRun {
                    len,
                    font: text_style.font(),
                    color: zterm_core::TERMINAL_BACKGROUND.into(),
                    ..Default::default()
                }],
                None,
            )
        };

        // For whitespace, use cell width to avoid cursor stretching.
        // For other characters, use the larger of shaped width and cell width
        // to properly cover wide characters like emojis.
        let cursor_width = if cursor_char.is_whitespace() {
            dimensions.cell_width()
        } else {
            cursor_text.width.max(dimensions.cell_width())
        };

        let cursor = if let CursorShape::Hidden = cursor.shape {
            None
        } else {
            let shape = if self.focused {
                cursor.shape
            } else {
                CursorShape::HollowBlock
            };
            TerminalElement::cursor_position(cursor_point, &dimensions).map(|cursor_position| {
                TerminalCursor {
                    bounds: Bounds {
                        origin: cursor_position,
                        size: size(cursor_width.ceil(), dimensions.line_height),
                    },
                    shape,
                    text: if shape == CursorShape::Block {
                        Some(cursor_text)
                    } else {
                        None
                    },
                }
            })
        };

        let cursor_visible = self.view.read(cx).cursor_blink_visible();
        let hovered = self
            .terminal
            .read(cx)
            .last_content
            .last_hovered_word
            .clone()
            .map(|word| word.word_match);
        let matches = self.terminal.read(cx).matches.clone();

        LayoutState {
            batched_text_runs,
            block_element_rects,
            rects,
            selection,
            cursor,
            cursor_visible,
            hovered,
            matches,
            background_color: zterm_core::TERMINAL_BACKGROUND.into(),
            dimensions,
            display_offset,
        }
    }

    fn paint(
        &mut self,
        _global_id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        layout: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_quad(fill(bounds, layout.background_color));
            let origin = layout.dimensions.bounds.origin;
            let scale_factor = window.scale_factor();
            let snap_px = |value: Pixels| {
                Pixels::from((f32::from(value) * scale_factor).floor() / scale_factor)
            };
            let origin = point(snap_px(origin.x), snap_px(origin.y));

            for rect in &layout.rects {
                rect.paint(origin, &layout.dimensions, window);
            }

            if let Some(selection) = layout.selection {
                let selection_color: Hsla = SELECTION_COLOR.into();
                for selection_rect in selection_rects(
                    selection.point_range(),
                    &layout.dimensions,
                    layout.display_offset,
                    origin,
                ) {
                    window.paint_quad(fill(selection_rect, selection_color));
                }
            }

            if !layout.matches.is_empty() {
                let match_color: Hsla = gpui::yellow();
                for m in &layout.matches {
                    for rect in selection_rects(
                        *m,
                        &layout.dimensions,
                        layout.display_offset,
                        origin,
                    ) {
                        window.paint_quad(fill(rect, match_color));
                    }
                }
            }

            if let Some(hovered) = layout.hovered {
                let link_color: Hsla = zterm_core::TERMINAL_FOREGROUND.into();
                for mut rect in selection_rects(
                    hovered,
                    &layout.dimensions,
                    layout.display_offset,
                    origin,
                ) {
                    rect.origin.y += rect.size.height - px(1.0);
                    rect.size.height = px(1.0);
                    window.paint_quad(fill(rect, link_color));
                }
            }

            for batch in &layout.batched_text_runs {
                batch.paint(origin, &layout.dimensions, window, cx);
            }
            for block_element_rect in &layout.block_element_rects {
                block_element_rect.paint(origin, &layout.dimensions, window);
            }

            let cursor_bounds = layout.cursor.as_ref().map(|cursor| cursor.bounds);

            // While an IME composition is in progress, render the pre-edit text
            // underlined at the cursor and hide the regular cursor; otherwise the
            // shell would echo raw pinyin. The normal cursor paints only when no
            // text is marked.
            if let Some(marked) = &self.marked_text {
                if !marked.is_empty()
                    && let Some(ime_bounds) = cursor_bounds
                {
                    let ime_position = (ime_bounds + origin).origin;
                    let base_color: Hsla = zterm_core::TERMINAL_FOREGROUND.into();
                    let shaped_line = window.text_system().shape_line(
                        marked.clone().into(),
                        FONT_SIZE,
                        &[TextRun {
                            len: marked.len(),
                            font: Font {
                                family: FONT_FAMILY.into(),
                                ..Default::default()
                            },
                            color: base_color,
                            underline: Some(UnderlineStyle {
                                color: Some(base_color),
                                thickness: px(1.0),
                                wavy: false,
                            }),
                            ..Default::default()
                        }],
                        None,
                    );
                    window.paint_quad(fill(
                        Bounds::new(
                            ime_position,
                            size(shaped_line.width, layout.dimensions.line_height),
                        ),
                        layout.background_color,
                    ));
                    shaped_line
                        .paint(
                            ime_position,
                            layout.dimensions.line_height,
                            gpui::TextAlign::Left,
                            None,
                            window,
                            cx,
                        )
                        .log_err();
                }
            } else if layout.cursor_visible
                && let Some(cursor) = &layout.cursor
            {
                cursor.paint(origin, &layout.dimensions, window, cx);
            }

            // Register the text input handler so macOS delivers committed
            // characters (and IME text) to the terminal. Without this, printable
            // keys never reach the PTY on macOS.
            window.handle_input(
                &self.focus,
                ZtermInputHandler {
                    view: self.view.clone(),
                    cursor_bounds: cursor_bounds.map(|bounds| bounds + origin),
                },
                cx,
            );

            // Element-level on_mouse_move never sees button-pressed motion, so
            // drag selection and mouse-report forwarding are wired at the window
            // level, mirroring the original terminal element.
            window.on_mouse_event({
                let terminal = self.terminal.clone();
                let focus = self.focus.clone();
                move |e: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    if e.pressed_button.is_some()
                        && !cx.has_active_drag()
                        && focus.is_focused(window)
                    {
                        let hovered = bounds.contains(&window.mouse_position());
                        terminal.update(cx, |terminal, cx| {
                            if terminal.selection_started() || hovered {
                                terminal.mouse_drag(e, bounds, cx);
                                cx.notify();
                            }
                        });
                    }
                    if bounds.contains(&window.mouse_position()) {
                        terminal.update(cx, |terminal, cx| terminal.mouse_move(e, cx));
                    }
                }
            });
        });
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// Converts a terminal point range into one filled rect per row, clamped to
/// the viewport and normalized by the display offset.
fn selection_rects(
    range: Range,
    dimensions: &TerminalBounds,
    display_offset: usize,
    origin: GpuiPoint<Pixels>,
) -> Vec<Bounds<Pixels>> {
    // Normalize to viewport relative, from terminal relative.
    // lines are i32s, which are negative above the top left corner of the terminal
    // If the user has scrolled, we use the display_offset to tell us which offset
    // of the grid data we should be looking at. But for the rendering step, we don't
    // want negatives. We want things relative to the 'viewport' (the area of the grid
    // which is currently shown according to the display offset)
    let display_offset = i32::try_from(display_offset).unwrap_or(i32::MAX);
    let unclamped_start_line = range.start().line.saturating_add(display_offset);
    let unclamped_start_column = range.start().column;
    let unclamped_end_line = range.end().line.saturating_add(display_offset);
    let unclamped_end_column = range.end().column;

    // Clamp range to viewport, and return nothing if it doesn't overlap
    if unclamped_end_line < 0 || unclamped_start_line > dimensions.num_lines() as i32 {
        return Vec::new();
    }

    let clamped_start_line = unclamped_start_line.max(0) as usize;
    let clamped_end_line = unclamped_end_line.min(dimensions.num_lines() as i32) as usize;

    // Expand ranges that cross lines into a collection of single-line rects
    let mut rects = Vec::new();
    for line in clamped_start_line..=clamped_end_line {
        let mut line_start = 0;
        let mut line_end = dimensions.num_columns();

        if line == clamped_start_line && unclamped_start_line >= 0 {
            line_start = unclamped_start_column;
        }
        if line == clamped_end_line && unclamped_end_line <= dimensions.num_lines() as i32 {
            line_end = unclamped_end_column + 1; // +1 for inclusive
        }

        let start_x = origin.x + line_start as f32 * dimensions.cell_width;
        let end_x = origin.x + line_end as f32 * dimensions.cell_width;
        let y = origin.y + line as f32 * dimensions.line_height;
        rects.push(Bounds::new(
            point(start_x, y),
            size(end_x - start_x, dimensions.line_height),
        ));
    }

    rects
}

/// Converts a 2, 8, or 24 bit color ANSI color to the GPUI equivalent.
pub fn convert_color(color: &Color) -> Hsla {
    match color {
        // Named colors
        Color::Named(named) => match named {
            NamedColor::Black => zterm_core::get_color_at_index(0),
            NamedColor::Red => zterm_core::get_color_at_index(1),
            NamedColor::Green => zterm_core::get_color_at_index(2),
            NamedColor::Yellow => zterm_core::get_color_at_index(3),
            NamedColor::Blue => zterm_core::get_color_at_index(4),
            NamedColor::Magenta => zterm_core::get_color_at_index(5),
            NamedColor::Cyan => zterm_core::get_color_at_index(6),
            NamedColor::White => zterm_core::get_color_at_index(7),
            NamedColor::BrightBlack => zterm_core::get_color_at_index(8),
            NamedColor::BrightRed => zterm_core::get_color_at_index(9),
            NamedColor::BrightGreen => zterm_core::get_color_at_index(10),
            NamedColor::BrightYellow => zterm_core::get_color_at_index(11),
            NamedColor::BrightBlue => zterm_core::get_color_at_index(12),
            NamedColor::BrightMagenta => zterm_core::get_color_at_index(13),
            NamedColor::BrightCyan => zterm_core::get_color_at_index(14),
            NamedColor::BrightWhite => zterm_core::get_color_at_index(15),
            NamedColor::Foreground => zterm_core::TERMINAL_FOREGROUND.into(),
            NamedColor::Background => zterm_core::TERMINAL_BACKGROUND.into(),
            NamedColor::Cursor => zterm_core::TERMINAL_FOREGROUND.into(),
            NamedColor::DimBlack => zterm_core::get_color_at_index(259),
            NamedColor::DimRed => zterm_core::get_color_at_index(260),
            NamedColor::DimGreen => zterm_core::get_color_at_index(261),
            NamedColor::DimYellow => zterm_core::get_color_at_index(262),
            NamedColor::DimBlue => zterm_core::get_color_at_index(263),
            NamedColor::DimMagenta => zterm_core::get_color_at_index(264),
            NamedColor::DimCyan => zterm_core::get_color_at_index(265),
            NamedColor::DimWhite => zterm_core::get_color_at_index(266),
            NamedColor::BrightForeground => zterm_core::get_color_at_index(267),
            NamedColor::DimForeground => {
                let mut rgb = zterm_core::TERMINAL_FOREGROUND;
                rgb.r /= 2.0;
                rgb.g /= 2.0;
                rgb.b /= 2.0;
                rgb.into()
            }
        },
        // 'True' colors
        Color::Spec(rgb) => zterm_core::rgba_color(rgb.r, rgb.g, rgb.b),
        // 8 bit, indexed colors
        Color::Indexed(i) => zterm_core::get_color_at_index(*i as usize),
    }
}

pub fn is_blank(cell: &Cell) -> bool {
    if cell.character() != ' ' {
        return false;
    }

    if !is_default_background_color(cell.background()) {
        return false;
    }

    if cell.has_visible_style_modifier() {
        return false;
    }

    true
}

/// Minimal [`InputHandler`] so the macOS text system delivers committed and
/// IME text to the PTY. The terminal has no editable document, so selection and
/// marked-text queries report empty ranges.
struct ZtermInputHandler {
    view: Entity<crate::view::TerminalView>,
    cursor_bounds: Option<Bounds<Pixels>>,
}

impl InputHandler for ZtermInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(&mut self, _window: &mut Window, cx: &mut App) -> Option<StdRange<usize>> {
        self.view.read(cx).marked_text_range()
    }

    fn text_for_range(
        &mut self,
        _range: StdRange<usize>,
        _adjusted_range: &mut Option<StdRange<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<StdRange<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.view.update(cx, |view, cx| view.commit_text(text, cx));
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<StdRange<usize>>,
        new_text: &str,
        _new_selected_range: Option<StdRange<usize>>,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.view
            .update(cx, |view, cx| view.set_marked_text(new_text.to_string(), cx));
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut App) {
        self.view.update(cx, |view, cx| view.clear_marked_text(cx));
    }

    fn bounds_for_range(
        &mut self,
        _range: StdRange<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        self.cursor_bounds
    }

    fn character_index_for_point(
        &mut self,
        _point: GpuiPoint<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn apple_press_and_hold_enabled(&mut self) -> bool {
        false
    }
}
