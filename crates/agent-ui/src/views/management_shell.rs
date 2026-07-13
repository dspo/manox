//! Shared chrome for management pages.
//!
//! The unified "back to app" navigation control lives here so every management
//! sidebar reads as a peer of the main sidebar's menu items. The window
//! TitleBar itself is owned by each page's main column (mirroring the
//! conversation column), not by this module — see the settings shell for the
//! shared scaffold.

use gpui::{AnyElement, ClickEvent, SharedString, Window, prelude::*};
use gpui_component::{Icon, IconName, Sizable as _, h_flex, theme::Theme};

/// The unified "back to app" navigation control — a sidebar-action-row-style
/// row (`ArrowLeft` + label) so it reads as a peer of the main sidebar's menu
/// items, not an isolated button. The density, hover wash, corner radius, and
/// text size mirror `sidebar::menu_item` exactly, giving the settings sidebar
/// the same back affordance.
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
