//! View rendering layer for the conversation.

pub mod browser_view;
pub mod completion;
pub mod composer_menu;
pub mod context_rail;
pub mod management_shell;
pub mod member_panel;
pub mod message;
pub mod plugin_manager;
pub mod settings;
pub mod sidebar;
pub mod title_menu;
pub mod turn_navigator;

use gpui::prelude::*;
use gpui::{Div, px};

/// Max content width (centered, width-capped).
pub const CONTENT_MAX_W: f32 = 760.0;

/// Wrap content in a full-width, centered, width-capped container.
///
/// Used by message entries and the input area so lines don't run too long on
/// wide screens. The horizontal inset keeps content off the panel edges when
/// the window shrinks near its minimum width.
///
/// `min_w_0` on both the outer row and inner column breaks the min-content
/// chain end to end: without it the row's auto min-size = its widest child's
/// min-content (e.g. a long unbreakable code run in a message, or the composer
/// chip row), pinning the whole list to that width and forcing overflow into
/// the env-card gutter when the window narrows. With it the row shrinks with
/// the window, the input and chip-row gaps absorb the slack, and only the true
/// chip-row floor resists (enforced by `MIN_WINDOW_W`). The list clips any
/// residual incompressible content at its own edge (`overflow_x_hidden`) so it
/// never reaches the window as a horizontal scrollbar.
pub fn centered(child: impl gpui::IntoElement) -> Div {
    use gpui_component::{h_flex, v_flex};
    h_flex().w_full().min_w_0().justify_center().px_4().child(
        v_flex()
            .w_full()
            .min_w_0()
            .max_w(px(CONTENT_MAX_W))
            .child(child),
    )
}
