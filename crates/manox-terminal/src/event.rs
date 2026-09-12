//! Bridge from alacritty `Event` to manox `TerminalEvent`.
//!
//! `Term<T>` calls `EventListener::send_event` for UI-relevant state changes it
//! cannot represent internally (title, bell, clipboard, …). `ManoxListener`
//! forwards a filtered subset onto an `async_channel` consumed by the event
//! pump in `Terminal::spawn`. `ClipboardLoad` carries alacritty's response
//! callback; the pump loads the system clipboard through the capability seam,
//! invokes it, and writes the returned string back to the PTY.

use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::vte::ansi::Rgb;

/// Events crossing the PTY / listener boundary. `PtyOutput` is internal (fed
/// back into the Term by the event pump); the rest are re-emitted on the
/// handle's subscriber channel so the view layer can react.
///
/// `Send` so the bounded `async_channel` can carry these across the PTY
/// reader thread and the event pump. Not `Debug`/`Clone` — callbacks and
/// single-consumer dispatch don't need either.
pub enum TerminalEvent {
    /// Raw bytes read from the PTY master. Consumed by the event pump only.
    PtyOutput(Vec<u8>),
    /// Generic redraw nudge.
    Wakeup,
    /// Cursor blink state changed (DECSET 12, DECSCUSR). Split out of
    /// `Wakeup` so the view's blink manager can reset its phase instead of
    /// treating it as a plain redraw.
    CursorBlinkingChange,
    /// OSC 10/11/12 color query: index 0-255 palette, 256 default fg, 257
    /// default bg, 258 cursor. The view resolves the color from its theme,
    /// invokes the formatter, and writes the returned string to the PTY.
    ColorRequest(usize, Arc<dyn Fn(Rgb) -> String + Send + Sync + 'static>),
    /// OSC 7: the shell reported its working directory (via the byte tap,
    /// not the alacritty listener — vte does not dispatch OSC 7).
    CwdChanged(PathBuf),
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
    /// The shell finished init and accepts input (readiness marker tap or
    /// output-timing heuristic). Emitted at most once per terminal.
    Ready,
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
            Event::Wakeup | Event::MouseCursorDirty => Some(TerminalEvent::Wakeup),
            Event::CursorBlinkingChange => Some(TerminalEvent::CursorBlinkingChange),
            Event::ColorRequest(idx, fmt) => Some(TerminalEvent::ColorRequest(idx, fmt)),
            Event::Title(t) => Some(TerminalEvent::Title(Some(t))),
            Event::ResetTitle => Some(TerminalEvent::Title(None)),
            Event::Bell => Some(TerminalEvent::Bell),
            Event::Exit => Some(TerminalEvent::Exit),
            Event::ChildExit(code) => Some(TerminalEvent::ChildExit(code)),
            Event::ClipboardStore(_ty, text) => Some(TerminalEvent::ClipboardStore(text)),
            Event::ClipboardLoad(_ty, cb) => Some(TerminalEvent::ClipboardLoad(cb)),
            Event::PtyWrite(text) => Some(TerminalEvent::PtyWrite(text)),
            // TextAreaSizeRequest is niche; left unhandled until a real
            // caller appears.
            _ => None,
        };
        if let Some(ev) = mapped {
            // `send_event` runs on the event pump under the `FairMutex` lock,
            // inside `Processor::advance`. A blocking send would self-deadlock
            // the pump if the channel were ever full (the pump is also the
            // only drainer). Use
            // `try_send` and drop on backpressure instead — `Wakeup`, the
            // frequent event, is idempotent; rare events re-sync on the next.
            match self.tx.try_send(ev) {
                Ok(()) => {}
                Err(async_channel::TrySendError::Full(_)) => {
                    tracing::warn!("terminal event channel full; dropping event");
                }
                Err(async_channel::TrySendError::Closed(_)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_blinking_change_is_not_folded_into_wakeup() {
        let (tx, rx) = async_channel::bounded(8);
        let listener = ManoxListener::new(tx);
        listener.send_event(Event::CursorBlinkingChange);
        listener.send_event(Event::Wakeup);
        assert!(matches!(
            rx.try_recv(),
            Ok(TerminalEvent::CursorBlinkingChange)
        ));
        assert!(matches!(rx.try_recv(), Ok(TerminalEvent::Wakeup)));
    }

    #[test]
    fn color_request_carries_index_and_formatter() {
        let (tx, rx) = async_channel::bounded(8);
        let listener = ManoxListener::new(tx);
        listener.send_event(Event::ColorRequest(
            256,
            Arc::new(|rgb| format!("rgb:{:02x}", rgb.r)),
        ));
        let Ok(TerminalEvent::ColorRequest(idx, fmt)) = rx.try_recv() else {
            panic!("expected a ColorRequest");
        };
        assert_eq!(idx, 256);
        assert_eq!(fmt(Rgb { r: 255, g: 0, b: 0 }), "rgb:ff");
    }
}
