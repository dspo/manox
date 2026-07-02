use gpui::Hsla;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct InlineStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub code: bool,
    pub image_placeholder: bool,
    pub link_url: Option<String>,
    pub fg: Option<Hsla>,
    pub bg: Option<Hsla>,
}

impl InlineStyle {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
