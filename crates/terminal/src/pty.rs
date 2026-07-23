//! PTY bridge — `rmux-pty` wrapper.
//!
//! `open` allocates a PTY pair, spawns the user's default shell, and hands
//! back a `PtyHandle` owning the `PtyMaster` (reader/writer) and `PtyChild`
//! (wait/kill). The reader / waiter threads are not started here —
//! `PtySource::start` does, so the trait contract is uniform across the
//! local shell and an agent-backed source (`CxSessionSource`).
//!
//! Once started, two `std::thread`s run:
//!   - **reader**: blocking `PtyMaster::read` into an `async_channel` as
//!     `TerminalEvent::PtyOutput`. A bare `std::thread` (not
//!     `spawn_blocking`) so the 2-worker tokio pool is never starved by an
//!     unbounded blocking read.
//!   - **waiter**: blocking `PtyChild::wait`, forwarding the exit code as
//!     `TerminalEvent::ChildExit`.
//!
//! The reader never touches `Screen`; it only forwards bytes to the gpui
//! side, which feeds them to `InputParser::parse(&mut screen, buf)` under
//! the `FairMutex` lock.

use std::io;
use std::path::Path;
use std::thread::{self, JoinHandle};

use anyhow::{Context as _, Result};
use rmux_pty::{ChildCommand, PtyChild, PtyMaster, Signal};
use rmux_types::TerminalSize as RmuxTerminalSize;

use crate::event::TerminalEvent;
use crate::pty_source::PtySource;

pub struct PtyHandle {
    master: PtyMaster,
    child: Option<PtyChild>,
    reader_thread: Option<JoinHandle<()>>,
    wait_thread: Option<JoinHandle<()>>,
}

/// Open a PTY pair, spawn the shell, and take the master + child handles.
/// The child stays on the `PtyHandle` until `PtySource::start` moves it into
/// the waiter thread. `shell` overrides the default user program when `Some`.
pub fn open(
    cwd: &Path,
    cols: u16,
    rows: u16,
    shell: Option<&str>,
    env: &[(String, String)],
) -> Result<PtyHandle> {
    let mut cmd = match shell {
        Some(prog) => ChildCommand::new(prog),
        None => ChildCommand::new(default_shell()),
    };
    cmd = cmd.current_dir(cwd).size(RmuxTerminalSize::new(cols, rows));
    for (k, v) in env {
        cmd = cmd.env(k, v);
    }

    let spawned = cmd.spawn().context("spawn pty child")?;
    let (master, child) = spawned.into_parts();

    Ok(PtyHandle {
        master,
        child: Some(child),
        reader_thread: None,
        wait_thread: None,
    })
}

/// Resolve the user's default shell from the `SHELL` env var, falling back
/// to `/bin/zsh` on macOS.
fn default_shell() -> &'static str {
    std::env::var("SHELL")
        .ok()
        .and_then(|s| match s.as_str() {
            "/bin/zsh" => Some("/bin/zsh"),
            "/bin/bash" => Some("/bin/bash"),
            "/bin/sh" => Some("/bin/sh"),
            _ => None,
        })
        .unwrap_or("/bin/zsh")
}

/// Build a `Box<dyn PtySource>` for the user's shell in `cwd`, sized for the
/// given cols / rows. Shell and env come from `[terminal]` in settings.toml.
/// Does not start the source — `Terminal::new` calls `start` once.
pub fn default_source(cwd: &Path, cols: u16, rows: u16) -> Result<Box<dyn PtySource>> {
    let settings = crate::settings::load();
    let shell = settings.shell.as_deref();
    let handle = open(cwd, cols, rows, shell, &settings.env)?;
    Ok(Box::new(handle))
}

impl PtySource for PtyHandle {
    fn start(&mut self, event_tx: async_channel::Sender<TerminalEvent>) {
        // `start` is called exactly once by `Terminal::new`; the child handle
        // is move-only, so a second call would have nothing to feed the waiter.
        let mut child = self.child.take().expect("PtySource::start called twice");

        // Clone the master's I/O endpoint for the reader thread —
        // `try_clone_io` gives an independent `PtyIo` sharing the same
        // underlying PTY fd, with its own `read` method.
        let reader_io = self.master.try_clone_io().expect("clone io for reader");
        let reader_tx = event_tx.clone();
        self.reader_thread = Some(
            thread::Builder::new()
                .name("manox-pty-reader".into())
                .spawn(move || {
                    let mut buf = [0u8; 8192];
                    loop {
                        match reader_io.read(&mut buf) {
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
                .expect("spawn reader thread"),
        );

        let wait_tx = event_tx.clone();
        self.wait_thread = Some(
            thread::Builder::new()
                .name("manox-pty-wait".into())
                .spawn(move || {
                    let code = match child.wait() {
                        Ok(status) => status.code().unwrap_or(-1),
                        Err(_) => -1,
                    };
                    let _ = wait_tx.send_blocking(TerminalEvent::ChildExit(code));
                })
                .expect("spawn wait thread"),
        );
    }

    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        self.master.write_all(bytes)
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .resize(RmuxTerminalSize::new(cols, rows))
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        // Kill the child so the waiter (if started) reaps it and the reader
        // hits EOF; both threads then exit on their own. They are detached
        // rather than joined: joining would hang if the gpui-side receiver —
        // dropped just after this handle in the same `Terminal` destructor —
        // were not being polled, leaving a thread blocked in `send_blocking`
        // with no drainer. The threads own their reader clone / child handle
        // and channel-sender clones, so they are safe to outlive this handle.
        if let Some(child) = &self.child {
            let _ = child.kill(Signal::KILL);
        }
        // If `start` was never called the child is still here — it was killed
        // above. After `start` the child was moved into the waiter thread.
        // Drop the join handles to detach the threads.
        self.reader_thread.take();
        self.wait_thread.take();
    }
}
