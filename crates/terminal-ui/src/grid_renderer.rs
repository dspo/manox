//! Grid rendering — batch terminal cells into paintable runs.
//!
//! `layout_grid` does two passes over the visible cells:
//!   - **background**: merge horizontally- and vertically-adjacent cells with
//!     the same bg color into `BackgroundRegion`s, so one `paint_quad` covers
//!     a whole run instead of per-cell.
//!   - **text**: merge same-line, adjacent cells with the same fg/bg/flags
//!     into `BatchedTextRun`s, so one `shape_line` covers a whole run.
//!
//! Wide-char spacers are skipped (they are placeholders for the second cell
//! of a wide glyph). The element layer converts each run to a gpui `TextRun`
//! at paint time; here we keep the raw fg/bg/flags to stay gpui-agnostic.

use terminal::{Cell, Flags};
use gpui::Hsla;

use crate::theme::{TerminalTheme, convert, is_default_background};

/// A merged background rectangle in grid coordinates (line/col, not pixels).
#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundRegion {
    pub start_line: i32,
    pub start_col: i32,
    pub end_line: i32,
    pub end_col: i32,
    pub color: Hsla,
}

impl BackgroundRegion {
    fn new(line: i32, col: i32, color: Hsla) -> Self {
        Self {
            start_line: line,
            start_col: col,
            end_line: line,
            end_col: col,
            color,
        }
    }

    fn can_merge_with(&self, other: &BackgroundRegion) -> bool {
        if self.color != other.color {
            return false;
        }
        // Horizontal adjacency on the same line span.
        if self.start_line == other.start_line && self.end_line == other.end_line {
            return self.end_col + 1 == other.start_col || other.end_col + 1 == self.start_col;
        }
        // Vertical adjacency with matching column span.
        if self.start_col == other.start_col && self.end_col == other.end_col {
            return self.end_line + 1 == other.start_line || other.end_line + 1 == self.start_line;
        }
        false
    }

    fn merge_with(&mut self, other: &BackgroundRegion) {
        self.start_line = self.start_line.min(other.start_line);
        self.start_col = self.start_col.min(other.start_col);
        self.end_line = self.end_line.max(other.end_line);
        self.end_col = self.end_col.max(other.end_col);
    }
}

/// A merged text run: adjacent same-style cells on one line.
#[derive(Clone, Debug)]
pub struct BatchedTextRun {
    pub start_line: i32,
    pub start_col: i32,
    pub text: String,
    pub cell_count: usize,
    pub fg: Hsla,
    pub bg: Hsla,
    pub flags: Flags,
}

impl BatchedTextRun {
    fn new(line: i32, col: i32, c: char, fg: Hsla, bg: Hsla, flags: Flags) -> Self {
        let mut text = String::with_capacity(64);
        text.push(c);
        Self {
            start_line: line,
            start_col: col,
            text,
            cell_count: 1,
            fg,
            bg,
            flags,
        }
    }

    fn can_append(&self, fg: Hsla, bg: Hsla, flags: Flags) -> bool {
        self.fg == fg && self.bg == bg && self.flags == flags
    }

    fn append_char(&mut self, c: char) {
        self.text.push(c);
        self.cell_count += 1;
    }
}

/// The result of `layout_grid`: everything the element needs to paint.
pub struct GridPlan {
    pub background: Vec<BackgroundRegion>,
    pub runs: Vec<BatchedTextRun>,
}

/// Batch visible cells into background regions and text runs.
///
/// `cells` yields `(display_line, column, &Cell)` in display order
/// (top-to-bottom, left-to-right). The caller is responsible for applying
/// the display offset when mapping from alacritty's `display_iter`.
pub fn layout_grid<'a>(
    cells: impl Iterator<Item = (i32, usize, &'a Cell)>,
    theme: &TerminalTheme,
) -> GridPlan {
    let mut background: Vec<BackgroundRegion> = Vec::new();
    let mut runs: Vec<BatchedTextRun> = Vec::new();
    let mut current: Option<BatchedTextRun> = None;

    for (line, col, cell) in cells {
        let mut fg = cell.fg;
        let mut bg = cell.bg;
        if cell.flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }

        // Background region (skip default bg — the element fills bounds once).
        if !is_default_background(&bg) {
            let color = convert(&bg, theme);
            let col = col as i32;
            if let Some(last) = background.last_mut()
                && last.color == color
                && last.start_line == line
                && last.end_line == line
                && last.end_col + 1 == col
            {
                last.end_col = col;
            } else {
                background.push(BackgroundRegion::new(line, col, color));
            }
        }

        // Wide-char spacers are layout placeholders, not paintable glyphs.
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }

        if cell.c == ' ' {
            // A blank cell still contributes its background above, but no text.
            if let Some(batch) = current.take() {
                runs.push(batch);
            }
            continue;
        }

        let fg = convert(&fg, theme);
        let bg = convert(&bg, theme);
        let flags = cell.flags;
        let col = col as i32;

        if let Some(batch) = current.as_mut()
            && batch.can_append(fg, bg, flags)
            && batch.start_line == line
            && batch.start_col + batch.cell_count as i32 == col
        {
            batch.append_char(cell.c);
        } else {
            if let Some(batch) = current.take() {
                runs.push(batch);
            }
            current = Some(BatchedTextRun::new(line, col, cell.c, fg, bg, flags));
        }
    }
    if let Some(batch) = current.take() {
        runs.push(batch);
    }

    GridPlan {
        background: merge_background_regions(background),
        runs,
    }
}

/// Merge adjacent regions until no further merges are possible. O(n²) but n is
/// small (one pass over visible cells); correctness over fancy here.
fn merge_background_regions(regions: Vec<BackgroundRegion>) -> Vec<BackgroundRegion> {
    if regions.is_empty() {
        return regions;
    }
    let mut merged = regions;
    let mut changed = true;
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

#[cfg(test)]
mod tests {
    use super::*;
    use terminal::{Cell, Color, Flags, NamedColor};

    fn cell(c: char, fg: Color, bg: Color, flags: Flags) -> Cell {
        Cell {
            c,
            fg,
            bg,
            flags,
            extra: None,
        }
    }

    fn theme() -> TerminalTheme {
        TerminalTheme::default()
    }

    #[test]
    fn merges_horizontal_same_color() {
        let red = Color::Named(NamedColor::Red);
        let cells = [
            (0, 0, cell('a', Color::Named(NamedColor::Foreground), red, Flags::empty())),
            (0, 1, cell('b', Color::Named(NamedColor::Foreground), red, Flags::empty())),
            (0, 2, cell('c', Color::Named(NamedColor::Foreground), red, Flags::empty())),
        ];
        let plan = layout_grid(cells.iter().map(|(l, c, cell)| (*l, *c, cell)), &theme());
        // One merged background region across cols 0..=2, one text run "abc".
        assert_eq!(plan.background.len(), 1);
        let bg = &plan.background[0];
        assert_eq!((bg.start_line, bg.start_col, bg.end_col), (0, 0, 2));
        assert_eq!(plan.runs.len(), 1);
        assert_eq!(plan.runs[0].text, "abc");
        assert_eq!(plan.runs[0].cell_count, 3);
    }

    #[test]
    fn splits_on_color_change() {
        let red = Color::Named(NamedColor::Red);
        let green = Color::Named(NamedColor::Green);
        let cells = [
            (0, 0, cell('a', Color::Named(NamedColor::Foreground), red, Flags::empty())),
            (0, 1, cell('b', Color::Named(NamedColor::Foreground), green, Flags::empty())),
        ];
        let plan = layout_grid(cells.iter().map(|(l, c, cell)| (*l, *c, cell)), &theme());
        assert_eq!(plan.background.len(), 2);
        assert_eq!(plan.runs.len(), 2);
    }

    #[test]
    fn blank_cells_break_text_run_only() {
        let fg = Color::Named(NamedColor::Foreground);
        let cells = [
            (0, 0, cell('a', fg, Color::Named(NamedColor::Background), Flags::empty())),
            (0, 1, cell(' ', fg, Color::Named(NamedColor::Background), Flags::empty())),
            (0, 2, cell('b', fg, Color::Named(NamedColor::Background), Flags::empty())),
        ];
        let plan = layout_grid(cells.iter().map(|(l, c, cell)| (*l, *c, cell)), &theme());
        assert_eq!(plan.runs.len(), 2);
        assert_eq!(plan.runs[0].text, "a");
        assert_eq!(plan.runs[1].text, "b");
        // Default background produces no regions.
        assert!(plan.background.is_empty());
    }

    #[test]
    fn inverse_swaps_fg_and_bg() {
        let fg = Color::Named(NamedColor::Foreground);
        let bg = Color::Named(NamedColor::Background);
        let cell = cell('x', fg, bg, Flags::INVERSE);
        let plan = layout_grid(std::iter::once((0, 0, &cell)), &theme());
        // Inverse: the painted fg is the cell's bg (default bg → theme default_bg),
        // and the background region uses the cell's fg (default fg → theme default_fg).
        assert_eq!(plan.runs[0].fg, theme().default_bg);
        assert_eq!(plan.background[0].color, theme().default_fg);
    }
}
