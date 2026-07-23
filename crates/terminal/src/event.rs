//! Terminal events crossing the gpui boundary.
//!
//! rmux-core's `Screen` is a pull-based model: after `InputParser::parse`
//! the caller drains side-effects via `take_bell_count()`,
//! `take_terminal_passthrough()`, `take_replies()`, and `title()`. This
//! differs from alacritty's push-based `EventListener::send_event` callback.
//! The gpui task in `Terminal::new` processes these events.

/// Events crossing the gpui boundary. `PtyOutput` is internal (fed back into
/// the Screen by the gpui task); the rest are re-emitted via `EventEmitter`
/// so the view layer can react.
///
/// `Send` so the bounded `async_channel` can carry these across the PTY
/// reader thread and the gpui task.
pub enum TerminalEvent {
    /// Raw bytes read from the PTY master. Consumed by the gpui task only.
    PtyOutput(Vec<u8>),
    /// Generic redraw nudge.
    Wakeup,
    /// Window title changed; `None` resets to the default.
    Title(Option<String>),
    /// Terminal bell.
    Bell,
    /// Shutdown requested by the terminal.
    Exit,
    /// Child process exited with this code.
    ChildExit(i32),
    /// OSC 52 / clipboard write: store `text` on the system clipboard.
    ClipboardStore(String),
    /// OSC 52 / clipboard read: the reply bytes to write back to the PTY.
    ClipboardReadReply(Vec<u8>),
    /// Raw bytes the terminal asked to emit on its own behalf (DCS replies,
    /// DSR cursor-position reports, etc.).
    PtyWrite(Vec<u8>),
}
