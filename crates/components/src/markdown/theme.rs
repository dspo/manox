//! Theme bridge: workspace `Theme` → flat style table for the renderer.

use std::sync::Arc;

use gpui::{Hsla, Pixels, hsla};
use gpui_component::Theme;
use gpui_component::highlighter::HighlightTheme;

/// Style table built once per render from the workspace theme.
///
/// Colors are plain `Hsla` (cheap to copy); the highlight theme is `Arc`-shared
/// so code blocks can hand it to `SyntaxHighlighter::styles` without cloning
/// the palette. The base font/color is *not* stored here — `StyledText`
/// inherits it from `window.text_style()` (set by the parent `div`'s
/// `.text_sm()`/`.text_color()`/...) at layout time.
#[derive(Clone)]
pub struct MdStyles {
    pub foreground: Hsla,
    pub muted: Hsla,
    pub secondary: Hsla,
    pub border: Hsla,
    pub transparent: Hsla,
    pub highlight_theme: Arc<HighlightTheme>,
    /// Diff `+`/`-` accents — foreground is the saturated accent, background
    /// is the same hue faded to a wash so long runs stay readable.
    pub diff_add_fg: Hsla,
    pub diff_add_bg: Hsla,
    pub diff_del_fg: Hsla,
    pub diff_del_bg: Hsla,
    /// Inline-code pill: the wash behind `` `code` `` spans and the corner
    /// radius. Carried here so callers can override via `Markdown::inline_code`
    /// without touching the theme.
    pub inline_code_bg: Hsla,
    pub inline_code_radius: Pixels,
    /// Flat wash behind selected glyphs in selectable code/diff blocks.
    pub selection_bg: Hsla,
}

impl MdStyles {
    pub fn from_theme(theme: &Theme) -> Self {
        let success = theme.success;
        let danger = theme.danger;
        Self {
            foreground: theme.foreground,
            muted: theme.muted_foreground,
            secondary: theme.secondary,
            border: theme.border,
            transparent: hsla(0., 0., 0., 0.),
            highlight_theme: theme.highlight_theme.clone(),
            diff_add_fg: success,
            diff_add_bg: hsla(success.h, success.s, success.l, 0.15),
            diff_del_fg: danger,
            diff_del_bg: hsla(danger.h, danger.s, danger.l, 0.15),
            inline_code_bg: theme.secondary,
            inline_code_radius: theme.radius,
            // The universal light-blue text-selection tint. `theme.accent` varies
            // per palette and at low alpha can vanish against the message
            // background; a fixed blue keeps the drag highlight legible across
            // light/dark themes.
            selection_bg: hsla(211.0 / 360.0, 0.85, 0.6, 0.4),
        }
    }
}
