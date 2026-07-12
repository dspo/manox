//! PTY bridge — `portable-pty` wrapper.
//!
//! `open` opens a PTY pair, spawns the user's default shell, and hands back a
//! `PtyHandle` owning the master, writer, child-killer, and the not-yet-moved
//! reader fd + child handle. The reader / waiter threads are not started here —
//! `PtySource::start` does, so the trait contract is uniform across the local
//! shell and an agent-backed source (a future `CxSessionSource`).
//!
//! Once started, two `std::thread`s run:
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

use std::io::{self, Read, Write};
use std::ops::Deref;
use std::path::Path;
use std::thread::{self, JoinHandle};

use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::event::TerminalEvent;
use crate::pty_source::PtySource;

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
    // Moved into the reader / waiter threads by `PtySource::start`. Held until
    // then so `Drop` can reap a handle that was never started.
    reader: Option<Box<dyn Read + Send>>,
    child: Option<Box<dyn Child + Send>>,
    reader_thread: Option<JoinHandle<()>>,
    wait_thread: Option<JoinHandle<()>>,
}

/// Open a PTY pair, spawn the shell, and take the master writer + child
/// killer. The reader fd and child handle stay on the `PtyHandle` until
/// `PtySource::start` moves them into its threads. `shell` overrides the
/// default user program when `Some`.
pub fn open(
    cwd: &Path,
    cols: u16,
    rows: u16,
    shell: Option<&str>,
    env: &[(String, String)],
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

    let child = pair.slave.spawn_command(cmd).context("spawn_command")?;
    drop(pair.slave);

    let reader = pair.master.try_clone_reader().context("try_clone_reader")?;
    let writer = pair.master.take_writer().context("take_writer")?;
    let killer = child.clone_killer();
    let master = MasterHolder(pair.master);

    Ok(PtyHandle {
        master,
        writer: Mutex::new(writer),
        killer,
        reader: Some(reader),
        child: Some(child),
        reader_thread: None,
        wait_thread: None,
    })
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
        // `start` is called exactly once by `Terminal::new`; the reader fd and
        // child handle are move-only, so a second call would have nothing to
        // feed the threads.
        let mut reader = self.reader.take().expect("PtySource::start called twice");
        let mut child = self.child.take().expect("PtySource::start called twice");

        let reader_tx = event_tx.clone();
        self.reader_thread = Some(
            thread::Builder::new()
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
                .expect("spawn reader thread"),
        );

        let wait_tx = event_tx.clone();
        self.wait_thread = Some(
            thread::Builder::new()
                .name("manox-pty-wait".into())
                .spawn(move || {
                    let code = match child.wait() {
                        Ok(status) => status.exit_code() as i32,
                        Err(_) => -1,
                    };
                    let _ = wait_tx.send_blocking(TerminalEvent::ChildExit(code));
                })
                .expect("spawn wait thread"),
        );
    }

    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        self.writer.lock().write_all(bytes)
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
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
        // Kill the child so the waiter (if started) reaps it and the reader
        // hits EOF; both threads then exit on their own. They are detached
        // rather than joined: joining would hang if the gpui-side receiver —
        // dropped just after this handle in the same `Terminal` destructor —
        // were not being polled, leaving a thread blocked in `send_blocking`
        // with no drainer. The threads own their reader fd / child handle and
        // channel-sender clones, so they are safe to outlive this handle.
        let _ = self.killer.kill();
        // If `start` was never called the child is still here — reap it
        // directly so it isn't orphaned. After `start` the child was moved
        // into the waiter thread and this is `None`.
        if let Some(mut child) = self.child.take() {
            let _ = child.wait();
        }
        // Drop the join handles to detach the threads.
        self.reader_thread.take();
        self.wait_thread.take();
    }
}
