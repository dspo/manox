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

gpui::actions!(terminal_ui, [SendTab, SendShiftTab, Paste, CopySelection]);

/// Key bindings for the focused terminal view, registered in the `"Terminal"`
/// key context so they shadow gpui-component Root's window-wide `tab`/copy
/// bindings (Root's focus traversal would otherwise steal Tab, and its Copy
/// action would swallow cmd/ctrl-c).
pub fn terminal_key_bindings() -> Vec<gpui::KeyBinding> {
    let mut bindings = vec![
        gpui::KeyBinding::new("tab", SendTab, Some("Terminal")),
        gpui::KeyBinding::new("shift-tab", SendShiftTab, Some("Terminal")),
    ];
    #[cfg(target_os = "macos")]
    {
        bindings.push(gpui::KeyBinding::new("cmd-v", Paste, Some("Terminal")));
        bindings.push(gpui::KeyBinding::new(
            "cmd-c",
            CopySelection,
            Some("Terminal"),
        ));
    }
    #[cfg(not(target_os = "macos"))]
    {
        bindings.push(gpui::KeyBinding::new("ctrl-v", Paste, Some("Terminal")));
        bindings.push(gpui::KeyBinding::new(
            "ctrl-c",
            CopySelection,
            Some("Terminal"),
        ));
    }
    bindings
}

/// Register terminal UI actions and workspace tab integration.
/// Call at App startup, after `terminal::init`.
pub fn init(_cx: &mut App) {}
