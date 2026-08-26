//! Theme bridge: workspace `Theme` → flat style table for the renderer.

use std::sync::Arc;

use gpui::{AbsoluteLength, Hsla, hsla, rems};
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
    /// Inline-code foreground color. Carried here so callers can override via
    /// `Markdown::inline_code` without touching the theme.
    pub inline_code_fg: Hsla,
    /// Flat wash behind selected glyphs in selectable code/diff blocks.
    pub selection_bg: Hsla,
    /// Underline color for clickable link spans.
    pub link_color: Hsla,
    /// Wash behind the link under the cursor; `None` disables the hover
    /// highlight.
    pub hover_link_bg: Option<Hsla>,
    /// Document body type size (paragraphs, list items, inline code,
    /// blockquotes; headings inherit it except the 1rem steps). Mounts
    /// override per instance; default is the chrome base size (1rem).
    pub body_size: AbsoluteLength,
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
            inline_code_fg: theme.info,
            // The universal light-blue text-selection tint. `theme.accent` varies
            // per palette and at low alpha can vanish against the message
            // background; a fixed blue keeps the drag highlight legible across
            // light/dark themes.
            selection_bg: hsla(211.0 / 360.0, 0.85, 0.6, 0.4),
            link_color: theme.accent_foreground,
            // A faint accent wash behind the hovered link, subtle on both
            // light and dark backgrounds.
            hover_link_bg: Some(hsla(theme.accent.h, theme.accent.s, theme.accent.l, 0.15)),
            body_size: rems(1.).into(),
        }
    }
}

