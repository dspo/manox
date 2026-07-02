use gpui::{Hsla, Pixels, hsla, px};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RichTextTheme {
    pub background: Hsla,
    pub border: Hsla,
    pub radius: Pixels,
    pub foreground: Hsla,
    pub muted_foreground: Hsla,
    pub selection: Hsla,
    pub code_bg: Hsla,
    pub link_color: Hsla,
}

impl Default for RichTextTheme {
    fn default() -> Self {
        Self {
            background: hsla(0., 0., 1., 1.),
            border: hsla(0., 0., 0.88, 1.),
            radius: px(8.),
            foreground: hsla(0., 0., 0.12, 1.),
            muted_foreground: hsla(0., 0., 0.42, 1.),
            selection: hsla(0.58, 1.0, 0.5, 0.25),
            code_bg: hsla(0., 0., 0.5, 0.12),
            link_color: hsla(0.62, 0.6, 0.45, 1.),
        }
    }
}
