//! GPUI rendering layer for the terminal emulator.
//!
//! `TerminalElement` (a gpui `Element`) + `TerminalView` + the grid/cursor/
//! selection/search/vi/hyperlink/ime sublayers. Depends on gpui-component;
//! pure terminal logic lives in the `terminal` crate.
//!
//! Stage 0 leaves the module empty so the crate compiles. The Element, View,
//! and `actions!` are implemented in stages 2 and 9.

use gpui::App;

pub mod element;
pub mod grid_renderer;
pub mod terminal_view;
pub mod theme;

pub use terminal_view::TerminalView;

gpui::actions!(terminal_ui, [ReclaimedKey]);

/// Bindings for the configured reclaimed-keys whitelist (see
/// `terminal::settings::TerminalSettings::reclaimed_keys`; first phase:
/// tab / shift-tab / the platform copy key). Registered in the `"Terminal"`
/// key context so they shadow gpui-component Root's window-wide bindings
/// (focus traversal would otherwise steal Tab, its Copy action would swallow
/// cmd/ctrl-c) while the terminal is focused. The `ReclaimedKey` action
/// deliberately has no listener: with no action listener to stop propagation,
/// the key falls through to `TerminalView::on_key_down`'s general PTY
/// translation, so behavior stays fully generic.
pub fn terminal_key_bindings() -> Vec<gpui::KeyBinding> {
    terminal::settings::load()
        .reclaimed_keys
        .into_iter()
        .filter(|s| gpui::Keystroke::parse(s).is_ok())
        .map(|s| gpui::KeyBinding::new(&s, ReclaimedKey, Some("Terminal")))
        .collect()
}

/// Register terminal UI actions and workspace tab integration.
/// Call at App startup, after `terminal::init`.
pub fn init(_cx: &mut App) {}
