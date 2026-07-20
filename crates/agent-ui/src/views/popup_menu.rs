//! Shared styling for popup menus (completion popover, turn navigator, etc.).
//!
//! Both the slash-command completion and the turn navigator render as floating
//! panels with selectable rows. This module centralizes their visual tokens so
//! the two surfaces stay consistent.

use gpui::{App, ClickEvent, Pixels, Window, prelude::*, px};
use gpui_component::{Theme, v_flex};

/// Height of a single row in the popup menu.
pub const ROW_HEIGHT: Pixels = px(28.0);

/// Height of the search input when present.
pub const SEARCH_HEIGHT: Pixels = px(36.0);

/// Height of the empty-state placeholder.
pub const EMPTY_HEIGHT: Pixels = px(72.0);

/// Maximum height of the scrollable list area.
pub const MAX_LIST_HEIGHT: Pixels = px(300.0);

/// Horizontal inset between popup rows and the panel edge.
pub const LIST_HORIZONTAL_PADDING: Pixels = px(4.0);

/// Hover background tint — muted foreground at very low opacity.
pub fn hover_bg(theme: &Theme) -> gpui::Hsla {
    theme.muted_foreground.opacity(0.08)
}

/// Selected row background — uses the secondary accent.
pub fn selected_bg(theme: &Theme) -> gpui::Hsla {
    theme.secondary
}

/// Render a styled popup row with hover and selected states.
///
/// The row is 28px tall, horizontally padded, with rounded corners matching the
/// theme. When `is_selected` is true, it uses `selected_bg`; on hover it uses
/// `hover_bg`. The two never stack — selected already implies hover-like
/// affordance.
pub fn render_popup_row(
    ix: usize,
    id_prefix: &'static str,
    is_selected: bool,
    theme: &Theme,
    content: impl IntoElement,
    on_select: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let hover = hover_bg(theme);
    let selected = selected_bg(theme);
    let radius = theme.radius;

    let mut row = gpui::div()
        .id((id_prefix, ix))
        .w_full()
        .h(ROW_HEIGHT)
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .rounded(radius)
        .debug_selector(move || format!("{}_{}", id_prefix, ix))
        .cursor_pointer();

    if is_selected {
        row = row.bg(selected);
    } else {
        row = row.hover(move |s| s.bg(hover));
    }

    row.child(gpui::div().w_full().min_w_0().items_center().child(content))
        .on_click(on_select)
}

/// Wrap a list of rows in the standard popup container: popover background,
/// border, shadow, rounded corners.
pub fn popup_container(theme: &Theme, content: impl IntoElement) -> gpui::Div {
    v_flex()
        .w_full()
        .bg(theme.popover)
        .text_color(theme.popover_foreground)
        .border_1()
        .border_color(theme.border)
        .rounded(theme.radius)
        .shadow_md()
        .overflow_hidden()
        .child(content)
}

/// Render the empty-state placeholder centered in the popup.
pub fn render_empty_state(theme: &Theme, message: impl IntoElement) -> impl IntoElement {
    v_flex()
        .w_full()
        .h(EMPTY_HEIGHT)
        .items_center()
        .justify_center()
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(message)
}
