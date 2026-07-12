//! The `PtySource` abstraction.
//!
//! A `Terminal` is agnostic to where its bytes come from. The local user
//! shell (`PtyHandle`) and an in-process agent session (a future
//! `CxSessionSource` driving a `cx::SessionHandle`) are both `PtySource`s:
//! each starts a reader / waiter pair that emits `TerminalEvent`s, and each
//! forwards `write` / `resize` to its underlying PTY. The trait keeps the
//! alacritty grid + rendering layer untouched — it only consumes events.

use std::io;
use std::path::Path;

use async_channel::Sender;

use crate::event::TerminalEvent;

/// A live PTY backing a `Terminal`.
///
/// `start` is called once from `Terminal::new` and is the only method that
/// kicks off the event stream; it takes `&mut self` so the source can move its
/// read fd / child handle into the reader / waiter threads without interior
/// mutability. `write` and `resize` are `&self` so the UI thread can call them
/// freely while the background threads run.
pub trait PtySource: Send + 'static {
    /// Begin forwarding PTY output and child-exit events on `event_tx`.
    ///
    /// The source owns its reader / waiter threads (or equivalent) and detaches
    /// them so they outlive the source without blocking the gpui task's channel
    /// drain — the threads hold their own reader fd / child handle and channel
    /// sender clones, so they are safe to outlive the `PtySource`.
    fn start(&mut self, event_tx: Sender<TerminalEvent>);

    /// Write input bytes (keystrokes, paste) to the PTY master.
    fn write(&self, bytes: &[u8]) -> io::Result<()>;

    /// Resize the PTY to the given cols / rows.
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;

    /// Path of an injection socket, if the source exposes one. The local shell
    /// has none; an agent session's IPC socket is surfaced here so the host
    /// can later inject messages as if the user typed them.
    fn socket_path(&self) -> Option<&Path> {
        None
    }
}
