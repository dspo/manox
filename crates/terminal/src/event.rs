//! Bridge from alacritty `Event` to manox `TerminalEvent`.
//!
//! `Term<T>` calls `EventListener::send_event` for UI-relevant state changes it
//! cannot represent internally (title, bell, clipboard, …). `ManoxListener`
//! forwards a filtered subset onto an `async_channel` consumed by the gpui
//! task in `Terminal::new`. Clipboard/color/pty-write/textarea requests are
//! wired in stage 6; until then they are dropped.

use alacritty_terminal::event::{Event, EventListener};

/// Events crossing the gpui boundary. `PtyOutput` is internal (fed back into
/// the Term by the gpui task); the rest are re-emitted via `EventEmitter` so
/// the view layer can react.
#[derive(Debug, Clone)]
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
            // Stage 6 wires ClipboardStore/Load, ColorRequest, PtyWrite, TextAreaSizeRequest.
            _ => None,
        };
        if let Some(ev) = mapped {
            let _ = self.tx.send_blocking(ev);
        }
    }
}
