//! `PtySource` backed by a `cx::SessionHandle`.
//!
//! An external agent CLI (claude/codex/copilot) runs under a PTY that cx owns;
//! this module bridges cx's blocking `SessionHandle::read` / `wait` into the
//! same `TerminalEvent` channel a local-shell `PtyHandle` emits onto, so the
//! rendering pipeline is identical for both sources.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::thread;

use async_channel::Sender;

use cx::SessionHandle;

use crate::event::TerminalEvent;
use crate::pty_source::PtySource;

/// A `PtySource` over a shared `cx::SessionHandle`. The handle is held behind
/// `Arc` so the external-session owner (the workspace's `ExternalSession`)
/// keeps an independent clone for explicit `kill` on close, while the terminal
/// render path drives IO through this source.
pub struct CxSessionSource {
    handle: Arc<SessionHandle>,
}

impl CxSessionSource {
    /// Wrap a shared handle. The caller keeps its own `Arc` clone for the
    /// external-session close path; this source only borrows the share.
    pub fn new(handle: Arc<SessionHandle>) -> Self {
        Self { handle }
    }
}

impl PtySource for CxSessionSource {
    fn start(&mut self, event_tx: Sender<TerminalEvent>) {
        // Reader: blocking `handle.read` → `PtyOutput`. EOF (0 bytes) means the
        // agent closed its stdout / died; `Interrupted` is retried, other
        // errors break. A bare `std::thread` (not `spawn_blocking`) so the
        // tokio pool is never blocked by an unbounded PTY read.
        let reader_handle = Arc::clone(&self.handle);
        let reader_tx = event_tx.clone();
        thread::Builder::new()
            .name("manox-cx-reader".into())
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader_handle.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if reader_tx
                                .send_blocking(TerminalEvent::PtyOutput(buf[..n].to_vec()))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            })
            .expect("spawn cx reader thread");

        // Waiter: blocking `handle.wait` reaps the child once, then emits
        // `ChildExit`. `wait` is one-shot (takes the child out of its Mutex),
        // so exactly one waiter thread exists per session — the source is
        // constructed once per session and `start` is called once.
        let waiter_handle = Arc::clone(&self.handle);
        thread::Builder::new()
            .name("manox-cx-wait".into())
            .spawn(move || {
                let code = match waiter_handle.wait() {
                    Ok(result) => result.exit_code,
                    Err(_) => -1,
                };
                let _ = event_tx.send_blocking(TerminalEvent::ChildExit(code));
            })
            .expect("spawn cx waiter thread");
    }

    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        // `write_bytes` — raw, no trailing newline. Keystroke-grade TUI
        // driving (arrow keys, Ctrl+C, paste fragments) rather than line
        // injection; `cx::SessionHandle::write` is the line path.
        self.handle
            .write_bytes(bytes)
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.handle
            .resize(cols, rows)
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn socket_path(&self) -> Option<&Path> {
        self.handle.socket_path()
    }
}
