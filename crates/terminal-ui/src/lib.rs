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

/// Register terminal UI actions and workspace tab integration.
/// Call at App startup, after `terminal::init`.
pub fn init(_cx: &mut App) {}
