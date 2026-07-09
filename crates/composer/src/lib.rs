//! Self-built multiline text input for the composer.
//!
//! Replaces `gpui-component`'s `Input`/`InputState` in the main composer so we
//! fully own paste handling (clipboard image → `ComposerEvent::PastedImage`).
//! The architecture mirrors gpui's `examples/input.rs` — a view entity
//! implementing `EntityInputHandler` plus a custom `TextElement` that registers
//! the IME handler during paint — extended with multi-line wrap (via
//! `shape_text`/`WrappedLine`) and image paste. Written from scratch; no code
//! copied from gpui-component.

mod blink_cursor;
mod element;
mod input;
mod str_util;

pub use blink_cursor::BlinkCursor;
pub(crate) use element::TextElement;
pub use input::{ComposerEvent, ComposerInput};

use gpui::{App, KeyBinding};

/// Key context under which all composer input actions are bound. The render
/// div declares the same context via `key_context` so bindings only fire while
/// the composer is focused.
pub const CONTEXT: &str = "ComposerInput";

gpui::actions!(
    composer,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
        Enter,
        Newline,
    ]
);

/// Bind keys and register actions for the composer input. Call once at app
/// startup, alongside the other `*::init(cx)` calls.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        KeyBinding::new("left", Left, Some(CONTEXT)),
        KeyBinding::new("right", Right, Some(CONTEXT)),
        KeyBinding::new("shift-left", SelectLeft, Some(CONTEXT)),
        KeyBinding::new("shift-right", SelectRight, Some(CONTEXT)),
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-v", Paste, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-c", Copy, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-x", Cut, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-x", Cut, Some(CONTEXT)),
        KeyBinding::new("enter", Enter, Some(CONTEXT)),
        KeyBinding::new("shift-enter", Newline, Some(CONTEXT)),
        KeyBinding::new("home", Home, Some(CONTEXT)),
        KeyBinding::new("end", End, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, Some(CONTEXT)),
    ]);
}
