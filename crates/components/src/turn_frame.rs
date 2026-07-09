use gpui::prelude::*;
use gpui::{AnyElement, Hsla, SharedString, px};
use gpui_component::{Theme, h_flex, v_flex};

/// A full-width message frame with an open center on the bottom edge.
///
/// The frame draws one rounded border, then masks the bottom center so callers
/// can treat the open-bottom treatment as a single text container.
pub struct TurnFrame {
    group: Option<SharedString>,
    header: Option<AnyElement>,
    trailing: Option<AnyElement>,
    children: Vec<AnyElement>,
    accent: Hsla,
    gap_fill: Hsla,
}

impl TurnFrame {
    pub fn new(theme: &Theme) -> Self {
        Self {
            group: None,
            header: None,
            trailing: None,
            children: Vec::new(),
            accent: theme.accent,
            gap_fill: theme.background,
        }
    }

    pub fn group(mut self, group: impl Into<SharedString>) -> Self {
        self.group = Some(group.into());
        self
    }

    pub fn accent(mut self, accent: Hsla) -> Self {
        self.accent = accent;
        self
    }

    pub fn gap_fill(mut self, fill: Hsla) -> Self {
        self.gap_fill = fill;
        self
    }

    pub fn header(mut self, header: impl IntoElement) -> Self {
        self.header = Some(header.into_any_element());
        self
    }

    pub fn trailing(mut self, trailing: impl IntoElement) -> Self {
        self.trailing = Some(trailing.into_any_element());
        self
    }

    pub fn child(mut self, child: impl IntoElement) -> Self {
        self.children.push(child.into_any_element());
        self
    }
}

impl IntoElement for TurnFrame {
    type Element = AnyElement;

    fn into_element(self) -> Self::Element {
        let rail = px(2.);
        let radius = px(12.);
        let corner_w = px(34.);

        let mut shell = v_flex().w_full().min_w_0();
        if let Some(group) = self.group {
            shell = shell.group(group);
        }

        let mut header_row = h_flex().w_full().min_w_0().items_center().gap_2().text_xs();
        if let Some(header) = self.header {
            header_row = header_row.child(gpui::div().flex_1().min_w_0().truncate().child(header));
        } else {
            header_row = header_row.child(gpui::div().flex_1().min_w_0());
        }
        if let Some(trailing) = self.trailing {
            header_row = header_row.child(trailing);
        }

        shell
            .w_full()
            .min_w_0()
            .relative()
            .overflow_hidden()
            .border(rail)
            .rounded(radius)
            .border_color(self.accent)
            .px_4()
            .pt_3()
            .pb_3()
            .gap_2()
            .child(header_row)
            .children(self.children)
            .child(
                gpui::div()
                    .absolute()
                    .bottom_0()
                    .left(corner_w)
                    .right(corner_w)
                    .h(rail)
                    .bg(self.gap_fill),
            )
            .into_any_element()
    }
}
