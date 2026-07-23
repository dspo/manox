//! Terminal color theme — rmux-core `Colour` → gpui `Hsla`.
//!
//! manox ships a default ANSI-16 palette + fg/bg/cursor. `convert` resolves
//! any rmux-core `Colour` (named, truecolor, 256-indexed) against a
//! `TerminalTheme`.

use gpui::Hsla;
use gpui_component::Theme;
use rmux_core::input::{Colour, COLOUR_DEFAULT, COLOUR_FLAG_256, COLOUR_FLAG_RGB, COLOUR_NONE, COLOUR_TERMINAL};

/// Resolved terminal palette used by the renderer.
pub struct TerminalTheme {
    pub default_fg: Hsla,
    pub default_bg: Hsla,
    pub cursor: Hsla,
    /// ANSI colors 0..16 (Black, Red, …, BrightWhite).
    pub ansi: [Hsla; 16],
}

impl Default for TerminalTheme {
    fn default() -> Self {
        Self {
            default_fg: hsla(0.0, 0.0, 0.87, 1.0),
            default_bg: hsla(0.0, 0.0, 0.06, 1.0),
            cursor: hsla(0.0, 0.0, 0.87, 1.0),
            ansi: [
                hsla(0.0, 0.0, 0.0, 1.0),    // Black
                hsla(0.0, 0.78, 0.47, 1.0),  // Red
                hsla(0.33, 0.70, 0.40, 1.0), // Green
                hsla(0.08, 0.78, 0.47, 1.0), // Yellow
                hsla(0.58, 0.70, 0.45, 1.0), // Blue
                hsla(0.83, 0.70, 0.45, 1.0), // Magenta
                hsla(0.50, 0.70, 0.45, 1.0), // Cyan
                hsla(0.0, 0.0, 0.87, 1.0),   // White
                hsla(0.0, 0.0, 0.40, 1.0),   // BrightBlack
                hsla(0.0, 0.81, 0.57, 1.0),  // BrightRed
                hsla(0.33, 0.75, 0.50, 1.0), // BrightGreen
                hsla(0.08, 0.81, 0.57, 1.0), // BrightYellow
                hsla(0.58, 0.75, 0.55, 1.0), // BrightBlue
                hsla(0.83, 0.75, 0.55, 1.0), // BrightMagenta
                hsla(0.50, 0.75, 0.55, 1.0), // BrightCyan
                hsla(0.0, 0.0, 0.97, 1.0),   // BrightWhite
            ],
        }
    }
}

impl TerminalTheme {
    /// Build a terminal palette whose bg/fg/cursor track the app theme.
    pub fn from_app_theme(theme: &Theme) -> Self {
        let bg = theme.background;
        let fg = theme.foreground;
        if theme.is_dark() {
            Self {
                default_fg: fg,
                default_bg: bg,
                cursor: fg,
                ansi: [
                    bg,                          // Black → theme bg
                    hsla(0.0, 0.78, 0.47, 1.0),  // Red
                    hsla(0.33, 0.70, 0.40, 1.0), // Green
                    hsla(0.08, 0.78, 0.47, 1.0), // Yellow
                    hsla(0.58, 0.70, 0.45, 1.0), // Blue
                    hsla(0.83, 0.70, 0.45, 1.0), // Magenta
                    hsla(0.50, 0.70, 0.45, 1.0), // Cyan
                    fg,                          // White → theme fg
                    hsla(0.0, 0.0, 0.40, 1.0),   // BrightBlack
                    hsla(0.0, 0.81, 0.57, 1.0),  // BrightRed
                    hsla(0.33, 0.75, 0.50, 1.0), // BrightGreen
                    hsla(0.08, 0.81, 0.57, 1.0), // BrightYellow
                    hsla(0.58, 0.75, 0.55, 1.0), // BrightBlue
                    hsla(0.83, 0.75, 0.55, 1.0), // BrightMagenta
                    hsla(0.50, 0.75, 0.55, 1.0), // BrightCyan
                    hsla(0.0, 0.0, 0.97, 1.0),   // BrightWhite
                ],
            }
        } else {
            Self {
                default_fg: fg,
                default_bg: bg,
                cursor: fg,
                ansi: [
                    bg,                          // Black → theme bg
                    hsla(0.0, 0.78, 0.35, 1.0),  // Red
                    hsla(0.33, 0.70, 0.30, 1.0), // Green
                    hsla(0.08, 0.78, 0.35, 1.0), // Yellow
                    hsla(0.58, 0.70, 0.35, 1.0), // Blue
                    hsla(0.83, 0.70, 0.35, 1.0), // Magenta
                    hsla(0.50, 0.70, 0.35, 1.0), // Cyan
                    fg,                          // White → theme fg
                    hsla(0.0, 0.0, 0.50, 1.0),   // BrightBlack
                    hsla(0.0, 0.81, 0.45, 1.0),  // BrightRed
                    hsla(0.33, 0.75, 0.40, 1.0), // BrightGreen
                    hsla(0.08, 0.81, 0.45, 1.0), // BrightYellow
                    hsla(0.58, 0.75, 0.45, 1.0), // BrightBlue
                    hsla(0.83, 0.75, 0.45, 1.0), // BrightMagenta
                    hsla(0.50, 0.75, 0.45, 1.0), // BrightCyan
                    fg,                          // BrightWhite → theme fg
                ],
            }
        }
    }
}

/// Resolve a rmux-core `Colour` (i32) to a paintable `Hsla`.
///
/// - 0–7: standard ANSI colors
/// - 8 (`COLOUR_DEFAULT`): default foreground
/// - 9 (`COLOUR_TERMINAL`): terminal (same as default for rendering)
/// - `COLOUR_FLAG_256 | idx`: 256-color palette
/// - `COLOUR_FLAG_RGB | packed_rgb`: truecolor
pub fn convert(colour: &Colour, theme: &TerminalTheme) -> Hsla {
    let c = *colour;
    if c == COLOUR_DEFAULT || c == COLOUR_TERMINAL || c == COLOUR_NONE {
        theme.default_fg
    } else if c & COLOUR_FLAG_RGB != 0 {
        let r = ((c >> 16) & 0xFF) as u8;
        let g = ((c >> 8) & 0xFF) as u8;
        let b = (c & 0xFF) as u8;
        rgb_to_hsla(r, g, b)
    } else if c & COLOUR_FLAG_256 != 0 {
        let idx = (c & 0xFF) as u8;
        indexed(idx, theme)
    } else if (0..16).contains(&c) {
        theme.ansi[c as usize]
    } else {
        theme.default_fg
    }
}

/// True if `colour` is the palette's default background — cells with this bg
/// need no `BackgroundRegion` (the element fills the whole bounds once).
pub fn is_default_background(colour: &Colour) -> bool {
    *colour == COLOUR_DEFAULT
}

/// 256-color palette lookup: 0..15 ANSI, 16..231 6×6×6 cube, 232..255 grayscale.
fn indexed(idx: u8, theme: &TerminalTheme) -> Hsla {
    match idx {
        0..=15 => theme.ansi[idx as usize],
        16..=231 => {
            let idx = idx - 16;
            let r = idx / 36;
            let g = (idx / 6) % 6;
            let b = idx % 6;
            let ramp = |v: u8| -> u8 { if v == 0 { 0 } else { 40 + 55 * v } };
            rgb_to_hsla(ramp(r), ramp(g), ramp(b))
        }
        _ => {
            let v = 8 + 10 * (idx - 232) as u32;
            let v = v.min(255) as u8;
            rgb_to_hsla(v, v, v)
        }
    }
}

fn rgb_to_hsla(r: u8, g: u8, b: u8) -> Hsla {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < 1e-6 {
        return hsla(0.0, 0.0, l, 1.0);
    }
    let d = max - min;
    let s = d / (2.0 - max - min);
    let h = if max == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if max == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    hsla(h / 6.0, s, l, 1.0)
}

/// gpui `Hsla` constructor (hue is a 0..1 turn).
fn hsla(h: f32, s: f32, l: f32, a: f32) -> Hsla {
    Hsla { h, s, l, a }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmux_core::input::colour_join_rgb;

    #[test]
    fn truecolor_roundtrip() {
        let red = convert(&colour_join_rgb(255, 0, 0), &TerminalTheme::default());
        assert!((red.h - 0.0).abs() < 1e-3);
        assert!((red.s - 1.0).abs() < 1e-3);
        assert!((red.l - 0.5).abs() < 1e-3);
    }

    #[test]
    fn standard_black_is_palette_slot_zero() {
        let theme = TerminalTheme::default();
        let black = convert(&0, &theme);
        assert_eq!(black, theme.ansi[0]);
    }

    #[test]
    fn indexed_grayscale_is_gray() {
        let g = convert(&(COLOUR_FLAG_256 | 232), &TerminalTheme::default());
        assert!((g.s).abs() < 1e-6);
        assert!((g.l - 8.0 / 255.0).abs() < 1e-3);
    }

    #[test]
    fn default_background_detected() {
        assert!(is_default_background(&COLOUR_DEFAULT));
        assert!(!is_default_background(&0));
    }
}
