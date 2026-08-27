//! Block-character sub-grid rendering.
//!
//! Cells holding block / shade glyphs (`▀▌▓` and friends) are painted as
//! rectangles on an 8×24 sub-grid instead of going through font shaping, so
//! TUI progress bars and gauges render crisply at any cell size. The mapping
//! is pure data: which character means which shape, and how a shape expands
//! to filled sub-rects. Terminal rendering policy (the `block_char_render`
//! setting) lives in the caller, not here.

use gpui::Hsla;

/// Cell sub-grid resolution. 24 = 8×3 so eighths (8 rows) and sextants
/// (3 rows) both land on integer boundaries.
pub const COLS: usize = 8;
pub const SUBROWS: usize = 24;

/// Which edge of the cell a [`BlockShape::Partial`] band hugs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

/// The geometric meaning of a block character, in sub-grid units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BlockShape {
    /// A filled band `eighths`/8 thick along one edge. Covers the 1/8-step
    /// blocks (`▁▂▃▄▅▆▇█▉▊▋▌▍▎▏▔▕`) and the half blocks (`▀▄▐`).
    Partial { edge: Edge, eighths: u8 },
    /// 2×2 quadrant bitmap: bit 0 upper-left, 1 upper-right, 2 lower-left,
    /// 3 lower-right (`▘▝▖▗▚▞▛▜▙▟`).
    Quadrant { bits: u8 },
    /// 2×3 sextant bitmap, bit n = row n/2, column n%2 (U+1FB00..=1FB3B).
    Sextant { bits: u16 },
    /// Uniform overlay over the whole cell at the given opacity (`░▒▓`).
    Shade { opacity: f32 },
}

/// One filled rectangle of a block cell, in sub-grid units (x in 0..=8,
/// y in 0..=24, half-open). `color` is the rect's fill.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubGridRect {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
    pub color: Hsla,
}

/// The shape of a block character, `None` for ordinary glyphs.
pub fn block_shape(c: char) -> Option<BlockShape> {
    use BlockShape::{Partial, Quadrant, Sextant, Shade};
    use Edge::{Bottom, Left, Right, Top};
    let p = |edge, eighths| Some(Partial { edge, eighths });
    match c {
        '▀' => p(Top, 4),
        '▁' => p(Bottom, 1),
        '▂' => p(Bottom, 2),
        '▃' => p(Bottom, 3),
        '▄' => p(Bottom, 4),
        '▅' => p(Bottom, 5),
        '▆' => p(Bottom, 6),
        '▇' => p(Bottom, 7),
        '█' => p(Bottom, 8),
        '▉' => p(Left, 7),
        '▊' => p(Left, 6),
        '▋' => p(Left, 5),
        '▌' => p(Left, 4),
        '▍' => p(Left, 3),
        '▎' => p(Left, 2),
        '▏' => p(Left, 1),
        '▐' => p(Right, 4),
        '▔' => p(Top, 1),
        '▕' => p(Right, 1),
        '░' => Some(Shade { opacity: 0.25 }),
        '▒' => Some(Shade { opacity: 0.5 }),
        '▓' => Some(Shade { opacity: 0.75 }),
        '▘' => Some(Quadrant { bits: 0b0001 }),
        '▝' => Some(Quadrant { bits: 0b0010 }),
        '▖' => Some(Quadrant { bits: 0b0100 }),
        '▗' => Some(Quadrant { bits: 0b1000 }),
        '▚' => Some(Quadrant { bits: 0b1001 }),
        '▞' => Some(Quadrant { bits: 0b0110 }),
        '▛' => Some(Quadrant { bits: 0b0111 }),
        '▜' => Some(Quadrant { bits: 0b1011 }),
        '▙' => Some(Quadrant { bits: 0b1101 }),
        '▟' => Some(Quadrant { bits: 0b1110 }),
        c if ('\u{1FB00}'..='\u{1FB3B}').contains(&c) => {
            // The 60 sextant codepoints encode the 6-bit cell masks in binary
            // order, skipping the four masks already covered by block
            // elements (empty, ▌ = 0b010101, ▐ = 0b101010, █ = 0b111111).
            let offset = c as u32 - 0x1FB00;
            let mask = offset + 1 + u32::from(offset >= 20) + u32::from(offset >= 40);
            Some(Sextant { bits: mask as u16 })
        }
        _ => None,
    }
}
/// One block-character cell in grid coordinates plus its expanded sub-rects.
#[derive(Debug, Clone, PartialEq)]
pub struct BlockRect {
    pub line: i32,
    pub col: i32,
    pub rects: Vec<SubGridRect>,
}

/// Expand a shape into filled sub-grid rects. `cols`/`subrows` are the grid
/// resolution (defaults [`COLS`]/[`SUBROWS`]); `color` is the cell's
/// foreground, which shade shapes modulate by their opacity.
pub fn expand(shape: BlockShape, color: Hsla, cols: usize, subrows: usize) -> Vec<SubGridRect> {
    match shape {
        BlockShape::Partial { edge, eighths } => {
            let eighths = eighths.min(8) as usize;
            let (x0, y0, x1, y1) = match edge {
                Edge::Top => (0, 0, cols, subrows * eighths / 8),
                Edge::Bottom => (0, subrows * (8 - eighths) / 8, cols, subrows),
                Edge::Left => (0, 0, cols * eighths / 8, subrows),
                Edge::Right => (cols * (8 - eighths) / 8, 0, cols, subrows),
            };
            vec![SubGridRect {
                x0,
                y0,
                x1: x1.max(x0 + 1),
                y1: y1.max(y0 + 1),
                color,
            }]
        }
        BlockShape::Quadrant { bits } => {
            let (qw, qh) = (cols / 2, subrows / 2);
            let quads = [
                (0, 0, 0b0001),   // upper-left
                (qw, 0, 0b0010),  // upper-right
                (0, qh, 0b0100),  // lower-left
                (qw, qh, 0b1000), // lower-right
            ];
            quads
                .into_iter()
                .filter(|(_, _, bit)| bits & bit != 0)
                .map(|(x0, y0, _)| SubGridRect {
                    x0,
                    y0,
                    x1: x0 + qw,
                    y1: y0 + qh,
                    color,
                })
                .collect()
        }
        BlockShape::Sextant { bits } => {
            let (sw, sh) = (cols / 2, subrows / 3);
            let mut rects = Vec::with_capacity(6);
            for bit in 0..6 {
                if bits & (1 << bit) != 0 {
                    let (row, col) = (bit / 2, bit % 2);
                    rects.push(SubGridRect {
                        x0: col * sw,
                        y0: row * sh,
                        x1: col * sw + sw,
                        y1: row * sh + sh,
                        color,
                    });
                }
            }
            rects
        }
        BlockShape::Shade { opacity } => {
            let mut shade = color;
            shade.a *= opacity;
            vec![SubGridRect {
                x0: 0,
                y0: 0,
                x1: cols,
                y1: subrows,
                color: shade,
            }]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Hsla;

    fn color() -> Hsla {
        Hsla {
            h: 0.6,
            s: 1.0,
            l: 0.5,
            a: 1.0,
        }
    }

    #[test]
    fn every_block_char_expands_to_a_non_empty_rect() {
        let chars = [
            '▀', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█', '▉', '▊', '▋', '▌', '▍', '▎', '▏', '▐',
            '▔', '▕', '░', '▒', '▓', '▘', '▝', '▖', '▗', '▚', '▞', '▛', '▜', '▙', '▟',
        ];
        for c in chars {
            let shape = block_shape(c).unwrap_or_else(|| panic!("{c:?} unmapped"));
            let rects = expand(shape, color(), COLS, SUBROWS);
            assert!(!rects.is_empty(), "{c:?} expanded to nothing");
            for r in &rects {
                assert!(r.x0 < r.x1 && r.x1 <= COLS, "{c:?} bad x: {r:?}");
                assert!(r.y0 < r.y1 && r.y1 <= SUBROWS, "{c:?} bad y: {r:?}");
            }
        }
    }

    #[test]
    fn upper_half_block_is_top_12_of_24() {
        let rects = expand(block_shape('▀').unwrap(), color(), COLS, SUBROWS);
        assert_eq!(rects.len(), 1);
        assert_eq!((rects[0].x0, rects[0].x1), (0, 8));
        assert_eq!((rects[0].y0, rects[0].y1), (0, 12));
    }

    #[test]
    fn lower_eighth_block_is_bottom_3_of_24() {
        let rects = expand(block_shape('▁').unwrap(), color(), COLS, SUBROWS);
        assert_eq!((rects[0].y0, rects[0].y1), (21, 24));
    }

    #[test]
    fn left_seven_eighth_block() {
        let rects = expand(block_shape('▉').unwrap(), color(), COLS, SUBROWS);
        assert_eq!((rects[0].x0, rects[0].x1), (0, 7));
        assert_eq!((rects[0].y0, rects[0].y1), (0, 24));
    }

    #[test]
    fn quadrant_upper_left_and_diagonal_pairs() {
        let ul = expand(block_shape('▘').unwrap(), color(), COLS, SUBROWS);
        assert_eq!(ul.len(), 1);
        assert_eq!((ul[0].x0, ul[0].y0, ul[0].x1, ul[0].y1), (0, 0, 4, 12));
        // ▚ (U+259A) = upper-left + lower-right — the diagonal pair.
        let diag = expand(block_shape('▚').unwrap(), color(), COLS, SUBROWS);
        assert_eq!(diag.len(), 2);
        assert!(
            diag.iter()
                .any(|r| (r.x0, r.y0, r.x1, r.y1) == (0, 0, 4, 12))
        );
        assert!(
            diag.iter()
                .any(|r| (r.x0, r.y0, r.x1, r.y1) == (4, 12, 8, 24))
        );
        // ▞ (U+259E) = upper-right + lower-left — the other diagonal.
        let other = expand(block_shape('▞').unwrap(), color(), COLS, SUBROWS);
        assert_eq!(other.len(), 2);
        assert!(
            other
                .iter()
                .any(|r| (r.x0, r.y0, r.x1, r.y1) == (4, 0, 8, 12))
        );
        assert!(
            other
                .iter()
                .any(|r| (r.x0, r.y0, r.x1, r.y1) == (0, 12, 4, 24))
        );
    }

    #[test]
    fn sextant_masks_follow_binary_order_with_skipped_masks() {
        // U+1FB00 = offset 0 → mask 1 = upper-left; U+1FB01 → mask 2 =
        // upper-right. No gap before offset 20.
        let ul = expand(block_shape('\u{1FB00}').unwrap(), color(), COLS, SUBROWS);
        assert_eq!(ul.len(), 1);
        assert_eq!((ul[0].x0, ul[0].y0, ul[0].x1, ul[0].y1), (0, 0, 4, 8));
        let ur = expand(block_shape('\u{1FB01}').unwrap(), color(), COLS, SUBROWS);
        assert_eq!((ur[0].x0, ur[0].x1), (4, 8));
        // The ▌ mask (0b010101 = 21) is skipped, so U+1FB14 (offset 20) maps
        // to mask 22 = 0b010110 (UR, ML, LL).
        let after_gap = expand(block_shape('\u{1FB14}').unwrap(), color(), COLS, SUBROWS);
        assert_eq!(after_gap.len(), 3);
        assert!(
            after_gap
                .iter()
                .any(|r| (r.x0, r.y0, r.x1, r.y1) == (4, 0, 8, 8))
        );
        assert!(
            after_gap
                .iter()
                .any(|r| (r.x0, r.y0, r.x1, r.y1) == (0, 8, 4, 16))
        );
        assert!(
            after_gap
                .iter()
                .any(|r| (r.x0, r.y0, r.x1, r.y1) == (0, 16, 4, 24))
        );
        // The ▐ mask (0b101010 = 42) is skipped too: U+1FB28 (offset 40) maps
        // to mask 43 = 0b101011 (UL, UR, MR, LR).
        let second_gap = expand(block_shape('\u{1FB28}').unwrap(), color(), COLS, SUBROWS);
        assert_eq!(second_gap.len(), 4);
        assert!(
            second_gap
                .iter()
                .any(|r| (r.x0, r.y0, r.x1, r.y1) == (0, 0, 4, 8))
        );
        assert!(
            second_gap
                .iter()
                .any(|r| (r.x0, r.y0, r.x1, r.y1) == (4, 0, 8, 8))
        );
        assert!(
            second_gap
                .iter()
                .any(|r| (r.x0, r.y0, r.x1, r.y1) == (4, 8, 8, 16))
        );
        assert!(
            second_gap
                .iter()
                .any(|r| (r.x0, r.y0, r.x1, r.y1) == (4, 16, 8, 24))
        );
        // The last codepoint encodes the near-full mask 62 (0b111110): all
        // but the upper-left cell.
        let near_full = expand(block_shape('\u{1FB3B}').unwrap(), color(), COLS, SUBROWS);
        assert_eq!(near_full.len(), 5);
        assert!(!near_full.iter().any(|r| (r.x0, r.y0) == (0, 0)));
    }

    #[test]
    fn shade_modulates_alpha_over_full_cell() {
        for (c, expected) in [('░', 0.25), ('▒', 0.5), ('▓', 0.75)] {
            let rects = expand(block_shape(c).unwrap(), color(), COLS, SUBROWS);
            assert_eq!(rects.len(), 1);
            assert_eq!(
                (rects[0].x0, rects[0].y0, rects[0].x1, rects[0].y1),
                (0, 0, 8, 24)
            );
            assert!((rects[0].color.a - expected).abs() < 1e-6);
        }
    }

    #[test]
    fn ordinary_glyphs_have_no_shape() {
        assert!(block_shape('a').is_none());
        assert!(block_shape(' ').is_none());
        assert!(block_shape('漢').is_none());
    }

    #[test]
    fn shade_rect_keeps_fg_hue() {
        let rects = expand(block_shape('▒').unwrap(), color(), COLS, SUBROWS);
        assert_eq!(rects[0].color.h, color().h);
    }
}
