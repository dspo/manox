//! Terminal emulator core for manox.
//!
//! `Terminal` state + PTY (portable-pty) + alacritty_terminal data-structure
//! layer, exposed through the gpui-free `TerminalHandle` /
//! `TerminalStoreHandle`. manox drives `alacritty_terminal::term::Term` —
//! which itself implements `vte::ansi::Handler` — via `Processor::advance`,
//! so no per-method ANSI handler is written here. The PTY reader runs on a
//! dedicated std::thread; bytes are piped back to the event pumps through an
//! `async_channel`, mirroring the provider streaming bridge in
//! `manox_agent::provider::anthropic`.
//!
//! The terminal crate is pure logic and does not depend on gpui or
//! gpui-component; the GPUI `Element` rendering layer lives in the
//! `terminal-ui` crate.

pub mod cx_session;
pub mod event;
pub mod mappings;
#[cfg(unix)]
pub mod proctree;
pub mod pty;
pub mod pty_source;
pub mod readiness;
pub mod runtime;
pub mod settings;
pub mod shell_kind;
pub mod store;
pub mod tap;
pub mod term;
pub mod theme;

// Re-export the alacritty data-structure types the rendering layer needs, so
// `terminal-ui` depends only on `terminal` and never on `alacritty_terminal`
// directly.
pub use alacritty_terminal;
pub use alacritty_terminal::grid::Indexed;
pub use alacritty_terminal::index::{Column, Line, Point};
pub use alacritty_terminal::term::cell::{Cell, Flags};
pub use alacritty_terminal::term::{RenderableContent, Term};
pub use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
pub use mappings::keys::{KeyEvent, Modifiers};
pub use store::TerminalStoreHandle;
pub use term::{HoverKind, HoverTarget, Terminal, TerminalHandle};

/// Register the `TerminalStore` against the shared `ThreadsDatabase`.
/// Call at App startup, after `manox_agent::init` and
/// `runtime::set_runtime`.
pub fn init() {
    store::init();
}
