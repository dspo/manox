//! View rendering layer for the conversation.

pub mod braille_spinner;
pub mod browser_view;
pub mod completion;
pub mod composer_menu;
pub mod context_rail;
pub mod launcher;
pub mod management_shell;
pub mod message;
pub mod model_cascade;
pub mod plugin_manager;
pub mod popup_menu;
pub mod settings;
pub mod sidebar;
pub mod subagent_panel;
pub mod subagents;
pub mod title_menu;
pub mod turn_navigator;

use gpui::prelude::*;
use std::{cell::Cell, rc::Rc};

use gpui::{Div, ListState, Pixels, px};

/// Width-keyed invalidation for native message-list row heights.
///
/// The pinned official GPUI revision remeasures visible rows, but retains the
/// cached heights of off-screen rows across a width change. A height measured
/// at a narrow width can therefore survive after a resize and present as a
/// large blank range. Keeping this tracker outside the list's row callback
/// lets the application invalidate the entire cache after final layout has
/// produced a positive, definite width.
#[derive(Clone, Default)]
pub struct MessageListWidthInvalidator {
    last_width: Rc<Cell<Option<Pixels>>>,
}

impl MessageListWidthInvalidator {
    /// Returns `true` when callers must schedule one more frame to consume the
    /// invalidated cache. Sub-pixel jitter is ignored.
    pub fn update(&self, width: Pixels, state: &ListState) -> bool {
        if width <= px(0.) {
            return false;
        }
        let previous = self.last_width.replace(Some(width));
        let changed = previous.is_some_and(|previous| (previous - width).abs() > px(0.5));
        if changed {
            state.remeasure_items(0..state.item_count());
        }
        changed
    }
}

/// Wrap content in a full-width, centered container that adapts to the
/// window width (no cap — the host dropped the fixed content width so wide
/// windows get the full span).
///
/// Used by message entries and the input area. The horizontal inset keeps
/// content off the panel edges when the window shrinks near its minimum
/// width.
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
    h_flex()
        .w_full()
        .min_w_0()
        .justify_center()
        .px_4()
        .child(v_flex().w_full().min_w_0().child(child))
}
