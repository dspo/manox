use gpui::prelude::*;
use gpui::{AnyElement, Hsla, PathBuilder, SharedString, canvas, point, px};
use gpui_component::{Theme, h_flex, v_flex};

/// A full-width message frame with an open center on the bottom edge.
///
/// The frame paints the door-shaped border as one continuous path. This keeps
/// the visual treatment from looking assembled out of independent rail nodes.
pub struct TurnFrame {
    group: Option<SharedString>,
    header: Option<AnyElement>,
    trailing: Option<AnyElement>,
    children: Vec<AnyElement>,
    accent: Hsla,
}

impl TurnFrame {
    pub fn new(theme: &Theme) -> Self {
        Self {
            group: None,
            header: None,
            trailing: None,
            children: Vec::new(),
            accent: theme.accent,
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
        let bottom_lift = px(10.);
        let lower_tail = px(18.);
        let accent = self.accent;

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
            .relative()
            .px_4()
            .pt_3()
            .pb_3()
            .gap_2()
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, _| {
                        let inset = rail / 2.;
                        let width = f32::from(bounds.size.width);
                        let height = f32::from(bounds.size.height);
                        if width <= f32::from(rail) || height <= f32::from(rail) {
                            return;
                        }

                        let radius = px(f32::from(radius)
                            .min(((width.min(height)) / 2. - f32::from(inset)).max(0.)));
                        let bottom_lift = px(f32::from(bottom_lift).min((height / 3.).max(0.)));
                        let lower_tail = px(f32::from(lower_tail).min((width / 4.).max(0.)));

                        let left = bounds.origin.x + inset;
                        let right = bounds.origin.x + bounds.size.width - inset;
                        let top = bounds.origin.y + inset;
                        let bottom = bounds.origin.y + bounds.size.height - inset - bottom_lift;

                        let mut path = PathBuilder::stroke(rail);
                        path.move_to(point(left + radius + lower_tail, bottom));
                        path.line_to(point(left + radius, bottom));
                        path.curve_to(point(left, bottom - radius), point(left, bottom));
                        path.line_to(point(left, top + radius));
                        path.curve_to(point(left + radius, top), point(left, top));
                        path.line_to(point(right - radius, top));
                        path.curve_to(point(right, top + radius), point(right, top));
                        path.line_to(point(right, bottom - radius));
                        path.curve_to(point(right - radius, bottom), point(right, bottom));
                        path.line_to(point(right - radius - lower_tail, bottom));

                        if let Ok(path) = path.build() {
                            window.paint_path(path, accent);
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .bottom_0(),
            )
            .child(header_row)
            .children(self.children)
            .into_any_element()
    }
}
