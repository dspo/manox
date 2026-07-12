//! Shared chrome for management pages (Settings, Plugin Manager).
//!
//! The window `TitleBar`, "back to app" control, and page-title rhythm are
//! unified here so every management mode reads as the same application under
//! one window chrome, instead of a separate product mounted inside the
//! window. Each management page mounts [`titlebar`] as its first child so
//! window-drag and macOS traffic-light avoidance match the main workspace;
//! the back control and page title sit on the leading side of that same
//! `TitleBar`, just past the traffic-light inset the component reserves.

use gpui::{AnyElement, ClickEvent, SharedString, Window, prelude::*};
use gpui_component::{
    Icon, IconName, Sizable as _, TITLE_BAR_HEIGHT, TitleBar, h_flex, theme::Theme,
};

/// The unified "back to app" navigation control — a sidebar-action-row-style
/// row (`ArrowLeft` + label) so it reads as a peer of the main sidebar's menu
/// items, not an isolated button. The density, hover wash, corner radius, and
/// text size mirror `sidebar::menu_item` exactly, giving both Settings and
/// the Plugin Manager the same back affordance.
pub fn back_control(
    theme: &Theme,
    label: SharedString,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> AnyElement {
    h_flex()
        .id("mgmt-back")
        .w_full()
        .items_center()
        .gap_2()
        .px_2()
        .py_1p5()
        .rounded(theme.radius)
        .text_sm()
        .text_color(theme.foreground)
        .hover(|s| s.bg(theme.accent.opacity(0.08)))
        .cursor_pointer()
        .on_click(on_click)
        .child(
            Icon::new(IconName::ArrowLeft)
                .small()
                .text_color(theme.muted_foreground),
        )
        .child(label)
        .into_any_element()
}

/// Build the window `TitleBar` for a management page. The back control and
/// page title sit on the leading side (after the traffic-light inset the
/// `TitleBar` reserves), and an optional trailing slot (search / page
/// actions) sits on the trailing side. The title truncates and the trailing
/// slot is shrink-fixed so a long title or a narrow window never pushes
/// actions off-screen.
///
/// Mounted as the first child of every management page, this is what makes
/// window-drag, traffic-light avoidance, and the title band identical across
/// management modes and the main workspace.
pub fn titlebar(
    theme: &Theme,
    back_label: SharedString,
    back_on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
    page_title: SharedString,
    trailing: Option<AnyElement>,
) -> TitleBar {
    TitleBar::new()
        .h(TITLE_BAR_HEIGHT)
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .items_center()
                .gap_3()
                // The back control is shrink-fixed so the title, not the nav
                // affordance, absorbs window narrowing.
                .child(h_flex().flex_shrink_0().child(back_control(
                    theme,
                    back_label,
                    back_on_click,
                )))
                .child(
                    gpui::div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .text_base()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .truncate()
                        .child(page_title),
                ),
        )
        .child(trailing.unwrap_or_else(|| h_flex().into_any_element()))
}
