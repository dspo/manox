use gpui::prelude::*;
use gpui::{AnyElement, Hsla, SharedString, px};
use gpui_component::{Theme, h_flex, v_flex};

/// A full-width message frame with an open center on the bottom edge.
///
/// The frame owns the top, side rails, and bottom corner strokes so callers can
/// treat it as a single text container instead of assembling border fragments.
pub struct TurnFrame {
    group: Option<SharedString>,
    header: Option<AnyElement>,
    trailing: Option<AnyElement>,
    children: Vec<AnyElement>,
    accent: Hsla,
    tint: Hsla,
}

impl TurnFrame {
    pub fn new(theme: &Theme) -> Self {
        Self {
            group: None,
            header: None,
            trailing: None,
            children: Vec::new(),
            accent: theme.accent,
            tint: theme.accent.opacity(0.035),
        }
    }

    pub fn group(mut self, group: impl Into<SharedString>) -> Self {
        self.group = Some(group.into());
        self
    }

    pub fn accent(mut self, accent: Hsla) -> Self {
        self.accent = accent;
        self.tint = accent.opacity(0.035);
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
        let corner_h = px(12.);

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

        let body = v_flex()
            .w_full()
            .min_w_0()
            .overflow_hidden()
            .border_t(rail)
            .border_l(rail)
            .border_r(rail)
            .rounded_t(radius)
            .border_color(self.accent)
            .bg(self.tint)
            .px_4()
            .pt_3()
            .pb_1()
            .gap_2()
            .child(header_row)
            .children(self.children);

        let bottom_corners = h_flex()
            .w_full()
            .min_w_0()
            .h(corner_h)
            .child(
                gpui::div()
                    .w(corner_w)
                    .h_full()
                    .border_l(rail)
                    .border_b(rail)
                    .rounded_bl(radius)
                    .border_color(self.accent),
            )
            .child(gpui::div().flex_1().min_w_0())
            .child(
                gpui::div()
                    .w(corner_w)
                    .h_full()
                    .border_r(rail)
                    .border_b(rail)
                    .rounded_br(radius)
                    .border_color(self.accent),
            );

        shell.child(body).child(bottom_corners).into_any_element()
    }
}
