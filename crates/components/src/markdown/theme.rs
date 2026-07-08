//! Theme bridge: workspace `Theme` → flat style table for the renderer.

use std::sync::Arc;

use gpui::{Hsla, hsla};
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
    pub highlight_theme: Arc<HighlightTheme>,
    /// Diff `+`/`-` accents — foreground is the saturated accent, background
    /// is the same hue faded to a wash so long runs stay readable.
    pub diff_add_fg: Hsla,
    pub diff_add_bg: Hsla,
    pub diff_del_fg: Hsla,
    pub diff_del_bg: Hsla,
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
            highlight_theme: theme.highlight_theme.clone(),
            diff_add_fg: success,
            diff_add_bg: hsla(success.h, success.s, success.l, 0.15),
            diff_del_fg: danger,
            diff_del_bg: hsla(danger.h, danger.s, danger.l, 0.15),
        }
    }
}
