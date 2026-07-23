//! Terminal emulator core for manox.
//!
//! `Terminal` Entity + PTY (rmux-pty) + rmux-core Screen domain model. manox
//! drives the rmux-core `Screen` — which implements `ScreenWriter` — via
//! `InputParser::parse(buf, &mut screen)`, so no per-method ANSI handler is
//! written here. The PTY reader runs on a dedicated std::thread; bytes are
//! piped back to the gpui Entity through an `async_channel`, mirroring the
//! provider streaming bridge in `agent::provider::anthropic`.
//!
//! The terminal crate is pure logic and does not depend on gpui-component;
//! the GPUI `Element` rendering layer lives in the `terminal-ui` crate.

pub mod cx_session;
pub mod event;
pub mod mappings;
pub mod pty;
pub mod pty_source;
pub mod settings;
pub mod store;
pub mod term;

use gpui::App;

// Re-export the rmux-core types the rendering layer needs, so `terminal-ui`
// depends only on `terminal` and never on `rmux-core` directly.
pub use rmux_core;
pub use rmux_core::TerminalPassthrough;
pub use rmux_core::input::{
    COLOUR_DEFAULT, COLOUR_FLAG_256, COLOUR_FLAG_RGB, COLOUR_NONE, COLOUR_TERMINAL, Colour,
    GridAttr, InputParser, ScreenWriter,
};
pub use rmux_core::{Screen, ScreenCellRef, ScreenCellView, ScreenLineView};
pub use rmux_types::TerminalSize;
pub use term::Terminal;

/// Register the `TerminalStore` against the shared `ThreadsDatabase`.
/// Call at App startup, after `agent::init`.
pub fn init(cx: &mut App) {
    store::init(cx);
}
