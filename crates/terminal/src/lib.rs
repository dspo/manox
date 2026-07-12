//! Terminal emulator core for manox.
//!
//! `Terminal` Entity + PTY (portable-pty) + alacritty_terminal data-structure
//! layer. manox drives `alacritty_terminal::term::Term` — which itself
//! implements `vte::ansi::Handler` — via `Processor::advance`, so no
//! per-method ANSI handler is written here. The PTY reader runs on a
//! dedicated std::thread; bytes are piped back to the gpui Entity through an
//! `async_channel`, mirroring the provider streaming bridge in
//! `agent::provider::anthropic`.
//!
//! The terminal crate is pure logic and does not depend on gpui-component;
//! the GPUI `Element` rendering layer lives in the `terminal-ui` crate.

pub mod event;
pub mod mappings;
pub mod pty;
pub mod pty_source;
pub mod settings;
pub mod store;
pub mod term;

use gpui::App;

// Re-export the alacritty data-structure types the rendering layer needs, so
// `terminal-ui` depends only on `terminal` and never on `alacritty_terminal`
// directly.
pub use alacritty_terminal;
pub use alacritty_terminal::grid::Indexed;
pub use alacritty_terminal::index::{Column, Line, Point};
pub use alacritty_terminal::term::cell::{Cell, Flags};
pub use alacritty_terminal::term::{RenderableContent, Term};
pub use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
pub use term::Terminal;

/// Register the `TerminalStore` against the shared `ThreadsDatabase`.
/// Call at App startup, after `agent::init`.
pub fn init(cx: &mut App) {
    store::init(cx);
}
