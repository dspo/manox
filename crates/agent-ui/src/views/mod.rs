//! View rendering layer for the conversation.

pub mod braille_spinner;
pub mod browser_view;
pub mod completion;
pub mod composer_menu;
pub mod context_rail;
pub mod management_shell;
pub mod member_panel;
pub mod message;
pub mod plugin_manager;
pub mod popup_menu;
pub mod settings;
pub mod sidebar;
pub mod subagent_panel;
pub mod subagents;
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
    use gpui_component::v_flex;
    // Centering is `margin: 0 auto` inside a column, not `justify_center` in a
    // row. A row flex derives its own height from the cross-size the item
    // reports at a *probe* width; with horizontal slack (viewport wider than
    // `CONTENT_MAX_W`) that probe is the item's min-content width, so wrapped
    // text reports a far taller height than it paints at the resolved width,
    // and the surplus stays in the row as blank space. A column flex resolves
    // the item's width first, so height is measured at the width it paints.
    v_flex().w_full().min_w_0().px_4().child(
        v_flex()
            .w_full()
            .min_w_0()
            .max_w(px(CONTENT_MAX_W))
            .mx_auto()
            .child(child),
    )
}
