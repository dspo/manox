//! PTY bridge — `portable-pty` wrapper.
//!
//! `spawn` opens a PTY pair, spawns the user's default shell, and starts two
//! dedicated `std::thread`s:
//!   - **reader**: blocking `master.read` into an `async_channel` as
//!     `TerminalEvent::PtyOutput`. A bare `std::thread` (not
//!     `spawn_blocking`) so the 2-worker tokio pool is never starved by an
//!     unbounded blocking read.
//!   - **waiter**: blocking `child.wait()`, forwarding the exit code as
//!     `TerminalEvent::ChildExit`.
//!
//! The reader never touches `Term`; it only forwards bytes to the gpui side,
//! which feeds them to `Processor::advance(&mut term, ..)` under the
//! `FairMutex` lock.
//!
//! `Box<dyn MasterPty + Send>` cannot be unsized into `Arc<dyn MasterPty>`
//! directly, so `MasterHolder` is a thin newtype that derefs to the trait
//! object — no `unsafe`.

use std::io::{self, Write};
use std::ops::Deref;
use std::path::Path;
use std::thread::{self, JoinHandle};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::event::TerminalEvent;

/// Owns the PTY master. `Box<dyn MasterPty + Send>` cannot be unsized into an
/// `Arc<dyn MasterPty>`, so this newtype holds the box and derefs to the trait
/// object. Not shared across threads — only the gpui side calls `resize`, so
/// no `Arc`/`Sync` is needed.
struct MasterHolder(Box<dyn MasterPty + Send>);

impl Deref for MasterHolder {
    type Target = dyn MasterPty;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

pub struct PtyHandle {
    master: MasterHolder,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    reader_thread: Option<JoinHandle<()>>,
    wait_thread: Option<JoinHandle<()>>,
}

/// Spawn the shell in a PTY of the given size, returning a handle that owns
/// the master, writer, child-killer, and the two background threads. `shell`
/// overrides the default user program when `Some`.
pub fn spawn(
    cwd: &Path,
    cols: u16,
    rows: u16,
    shell: Option<&str>,
    env: &[(String, String)],
    event_tx: async_channel::Sender<TerminalEvent>,
) -> Result<PtyHandle> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty")?;

    let mut cmd = match shell {
        Some(prog) => {
            let mut c = CommandBuilder::new(prog);
            c.cwd(cwd);
            c
        }
        None => {
            let mut c = CommandBuilder::new_default_prog();
            c.cwd(cwd);
            c
        }
    };
    for (k, v) in env {
        cmd.env(k, v);
    }

    let mut child = pair.slave.spawn_command(cmd).context("spawn_command")?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().context("try_clone_reader")?;
    let writer = pair.master.take_writer().context("take_writer")?;
    let killer = child.clone_killer();
    let master = MasterHolder(pair.master);

    let reader_tx = event_tx.clone();
    let reader_thread = thread::Builder::new()
        .name("manox-pty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
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
        .context("spawn reader thread")?;

    let wait_tx = event_tx.clone();
    let wait_thread = thread::Builder::new()
        .name("manox-pty-wait".into())
        .spawn(move || {
            let code = match child.wait() {
                Ok(status) => status.exit_code() as i32,
                Err(_) => -1,
            };
            let _ = wait_tx.send_blocking(TerminalEvent::ChildExit(code));
        })
        .context("spawn wait thread")?;

    Ok(PtyHandle {
        master,
        writer: Mutex::new(writer),
        killer,
        reader_thread: Some(reader_thread),
        wait_thread: Some(wait_thread),
    })
}

impl PtyHandle {
    /// Write input bytes (keystrokes, paste) to the PTY master.
    pub fn write(&self, bytes: &[u8]) -> io::Result<()> {
        self.writer.lock().write_all(bytes)
    }

    /// Resize the PTY to the given cols/rows. Pixel dimensions are left zero;
    /// applications that care (e.g. `resize -s`) are not a stage-1 concern.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        // Kill the child first so the waiter reaps it and the reader hits EOF;
        // both threads then exit on their own. Join so the process group is
        // guaranteed gone before we return.
        let _ = self.killer.kill();
        if let Some(t) = self.reader_thread.take() {
            let _ = t.join();
        }
        if let Some(t) = self.wait_thread.take() {
            let _ = t.join();
        }
    }
}
