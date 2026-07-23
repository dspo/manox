//! Grid rendering — batch terminal cells into paintable runs.

use gpui::Hsla;
use rmux_core::GridAttr;
use rmux_core::ScreenCellView;

use crate::theme::{TerminalTheme, convert, is_default_background};

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
        Self { start_line: line, start_col: col, end_line: line, end_col: col, color }
    }

    fn can_merge_with(&self, other: &BackgroundRegion) -> bool {
        if self.color != other.color {
            return false;
        }
        if self.start_line == other.start_line && self.end_line == other.end_line {
            return self.end_col + 1 == other.start_col || other.end_col + 1 == self.start_col;
        }
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

#[derive(Clone, Debug)]
pub struct BatchedTextRun {
    pub start_line: i32,
    pub start_col: i32,
    pub text: String,
    pub cell_count: usize,
    pub fg: Hsla,
    pub bg: Hsla,
    pub underline: bool,
    pub strikethrough: bool,
}

impl BatchedTextRun {
    fn new(line: i32, col: i32, c: char, fg: Hsla, bg: Hsla, attr: u16) -> Self {
        let mut text = String::with_capacity(64);
        text.push(c);
        Self {
            start_line: line,
            start_col: col,
            text,
            cell_count: 1,
            fg,
            bg,
            underline: attr & GridAttr::UNDERSCORE != 0,
            strikethrough: attr & GridAttr::STRIKETHROUGH != 0,
        }
    }

    fn can_append(&self, fg: Hsla, bg: Hsla, attr: u16) -> bool {
        self.fg == fg
            && self.bg == bg
            && self.underline == (attr & GridAttr::UNDERSCORE != 0)
            && self.strikethrough == (attr & GridAttr::STRIKETHROUGH != 0)
    }

    fn append_char(&mut self, c: char) {
        self.text.push(c);
        self.cell_count += 1;
    }
}

pub struct GridPlan {
    pub background: Vec<BackgroundRegion>,
    pub runs: Vec<BatchedTextRun>,
}

pub fn layout_grid(
    cells: impl Iterator<Item = (i32, usize, ScreenCellView)>,
    theme: &TerminalTheme,
) -> GridPlan {
    let mut background: Vec<BackgroundRegion> = Vec::new();
    let mut runs: Vec<BatchedTextRun> = Vec::new();
    let mut current: Option<BatchedTextRun> = None;

    for (line, col, cell) in cells {
        let mut fg = cell.fg();
        let mut bg = cell.bg();
        if cell.attr() & GridAttr::REVERSE != 0 {
            std::mem::swap(&mut fg, &mut bg);
        }

        let col_i = col as i32;

        if !is_default_background(&bg) {
            let color = convert(&bg, theme);
            if let Some(last) = background.last_mut()
                && last.color == color
                && last.start_line == line
                && last.end_line == line
                && last.end_col + 1 == col_i
            {
                last.end_col = col_i;
            } else {
                background.push(BackgroundRegion::new(line, col_i, color));
            }
        }

        if cell.is_padding() {
            continue;
        }

        if cell.text() == " " {
            if let Some(batch) = current.take() {
                runs.push(batch);
            }
            continue;
        }

        let fg_hsla = convert(&fg, theme);
        let bg_hsla = convert(&bg, theme);
        let attr = cell.attr();

        if let Some(batch) = current.as_mut()
            && batch.can_append(fg_hsla, bg_hsla, attr)
            && batch.start_line == line
            && batch.start_col + batch.cell_count as i32 == col_i
        {
            batch.append_char(cell.text().chars().next().unwrap_or(' '));
        } else {
            if let Some(batch) = current.take() {
                runs.push(batch);
            }
            let c = cell.text().chars().next().unwrap_or(' ');
            current = Some(BatchedTextRun::new(line, col_i, c, fg_hsla, bg_hsla, attr));
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


