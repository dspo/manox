//! View rendering layer for the conversation.

pub mod composer_menu;
pub mod message;
pub mod outline;
pub mod plugin_manager;
pub mod settings;
pub mod sidebar;
pub mod title_menu;

use gpui::prelude::*;
use gpui::{Div, px};

/// Max content width (centered, width-capped).
pub const CONTENT_MAX_W: f32 = 760.0;

/// Wrap content in a full-width, centered, width-capped container.
///
/// Used by message entries and the input area so lines don't run too long on
/// wide screens. The horizontal inset keeps content off the panel edges when
/// the window shrinks near its minimum width.
pub fn centered(child: impl gpui::IntoElement) -> Div {
    use gpui_component::{h_flex, v_flex};
    h_flex()
        .w_full()
        .justify_center()
        .px_4()
        .child(v_flex().w_full().max_w(px(CONTENT_MAX_W)).child(child))
}
