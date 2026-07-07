//! Bridge from alacritty `Event` to manox `TerminalEvent`.
//!
//! `Term<T>` calls `EventListener::send_event` for UI-relevant state changes it
//! cannot represent internally (title, bell, clipboard, …). `ManoxListener`
//! forwards a filtered subset onto an `async_channel` consumed by the gpui
//! task in `Terminal::new`. `ClipboardLoad` carries alacritty's response
//! callback; the gpui task loads the system clipboard, invokes it, and writes
//! the returned string back to the PTY.

use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener};

/// Events crossing the gpui boundary. `PtyOutput` is internal (fed back into
/// the Term by the gpui task); the rest are re-emitted via `EventEmitter` so
/// the view layer can react.
///
/// `Send` so the bounded `async_channel` can carry these across the PTY
/// reader thread and the gpui task. Not `Debug`/`Clone` — callbacks and
/// single-consumer dispatch don't need either.
pub enum TerminalEvent {
    /// Raw bytes read from the PTY master. Consumed by the gpui task only.
    PtyOutput(Vec<u8>),
    /// Generic redraw nudge.
    Wakeup,
    /// Window title changed; `None` resets to the default.
    Title(Option<String>),
    /// Terminal bell.
    Bell,
    /// Shutdown requested by the Term.
    Exit,
    /// Child process exited with this code.
    ChildExit(i32),
    /// OSC 52 / clipboard write: store `text` on the system clipboard.
    ClipboardStore(String),
    /// OSC 52 / clipboard read: invoke the callback with the current clipboard
    /// text and write the returned string back to the PTY.
    ClipboardLoad(Arc<dyn Fn(&str) -> String + Send + Sync + 'static>),
    /// Raw bytes the TUI asked the terminal to emit on its own behalf.
    PtyWrite(String),
}

/// Forwards alacritty `Event`s onto an `async_channel` as `TerminalEvent`s.
///
/// `Send` because `async_channel::Sender` is `Send`, which makes
/// `Term<ManoxListener>: Send` and thus storable behind `FairMutex`.
pub struct ManoxListener {
    tx: async_channel::Sender<TerminalEvent>,
}

impl ManoxListener {
    pub fn new(tx: async_channel::Sender<TerminalEvent>) -> Self {
        Self { tx }
    }
}

impl EventListener for ManoxListener {
    fn send_event(&self, event: Event) {
        let mapped = match event {
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {
                Some(TerminalEvent::Wakeup)
            }
            Event::Title(t) => Some(TerminalEvent::Title(Some(t))),
            Event::ResetTitle => Some(TerminalEvent::Title(None)),
            Event::Bell => Some(TerminalEvent::Bell),
            Event::Exit => Some(TerminalEvent::Exit),
            Event::ChildExit(code) => Some(TerminalEvent::ChildExit(code)),
            Event::ClipboardStore(_ty, text) => Some(TerminalEvent::ClipboardStore(text)),
            Event::ClipboardLoad(_ty, cb) => Some(TerminalEvent::ClipboardLoad(cb)),
            Event::PtyWrite(text) => Some(TerminalEvent::PtyWrite(text)),
            // ColorRequest / TextAreaSizeRequest are niche (theme introspection
            // used by some TUIs); left unhandled until a real caller appears.
            _ => None,
        };
        if let Some(ev) = mapped {
            let _ = self.tx.send_blocking(ev);
        }
    }
}
