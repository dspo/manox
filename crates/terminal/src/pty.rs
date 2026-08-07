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
use crate::shell_kind::{ShellKind, resolve_shell_program};

/// Owns the PTY master. `Box<dyn MasterPty + Send>` cannot be unsized into an
/// `Arc<dyn MasterPty>`, so this newtype holds the box and derefs to the trait
/// object. Never shared — the gpui side uses it while the handle lives, then
/// `Drop` moves it onto the teardown thread.
struct MasterHolder(Box<dyn MasterPty + Send>);

impl Deref for MasterHolder {
    type Target = dyn MasterPty;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

pub struct PtyHandle {
    /// Moved into the teardown thread by `Drop` (its close is the last
    /// teardown action, never before the tree scan). `Option` so the move
    /// is possible out of `&mut self`.
    master: Option<MasterHolder>,
    /// Taken into the teardown thread by `Drop`: it dups the master fd, so
    /// no master-side fd of this handle may close before the tree scan.
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    /// Moved into the teardown thread by `Drop`; `Option` so the move is
    /// possible out of `&mut self`.
    killer: Option<Box<dyn ChildKiller + Send + Sync>>,
    /// Readiness-marker nonce the shell was wrapped with; `None` for
    /// unwrapped spawns (heuristic readiness).
    ready_nonce: Option<String>,
    /// Shell pid captured at spawn — the child handle moves into the waiter
    /// thread at `start`, so `Drop` cannot ask it. Teardown target root.
    #[cfg(unix)]
    child_pid: Option<libc::pid_t>,
    /// Basename of the spawned program; the foreground indicator hides itself
    /// while the shell itself owns the foreground.
    #[cfg(unix)]
    shell_name: String,
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
///
/// Known shells spawn through a marker wrapper (see `shell_kind`): the
/// wrapper prints the readiness OSC and `exec`s the shell, preserving the
/// PID and login behavior (default program → login shell; explicit override
/// → non-login). Unknown shells spawn bare — via portable-pty's default prog
/// when `shell` is `None` — and readiness uses the heuristic path.
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

    let program = resolve_shell_program(shell);
    let login = shell.is_none();
    let wrapped = ShellKind::detect(&program).and_then(|kind| {
        let nonce = uuid::Uuid::new_v4().to_string();
        kind.marker_command(&program, &nonce, login)
            .map(|argv| (argv, nonce))
    });
    let (mut cmd, ready_nonce) = match wrapped {
        Some((argv, nonce)) => {
            let mut c = CommandBuilder::new(&argv[0]);
            c.args(&argv[1..]);
            (c, Some(nonce))
        }
        None => {
            let c = match shell {
                Some(prog) => CommandBuilder::new(prog),
                None => CommandBuilder::new_default_prog(),
            };
            (c, None)
        }
    };
    cmd.cwd(cwd);

    // manox is the host terminal regardless of what launched it; TERM and
    // COLORTERM only fill gaps in sparse GUI-launch environments. User
    // `[terminal].env` applies last and wins.
    cmd.env("TERM_PROGRAM", "manox");
    cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
    if std::env::var_os("TERM").is_none() {
        cmd.env("TERM", "xterm-256color");
    }
    if std::env::var_os("COLORTERM").is_none() {
        cmd.env("COLORTERM", "truecolor");
    }
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
        master: Some(master),
        writer: Mutex::new(Some(writer)),
        killer: Some(killer),
        ready_nonce,
        #[cfg(unix)]
        child_pid: child.process_id().map(|p| p as libc::pid_t),
        #[cfg(unix)]
        shell_name: Path::new(&program)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&program)
            .to_string(),
        reader: Some(reader),
        child: Some(child),
        reader_thread: None,
        wait_thread: None,
    })
}

impl PtyHandle {
    /// Shell pid captured at spawn — tests snapshot the tree before drop.
    #[cfg(all(unix, test))]
    pub(crate) fn child_pid(&self) -> Option<libc::pid_t> {
        self.child_pid
    }
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
        let mut guard = self.writer.lock();
        guard
            .as_mut()
            .expect("writer lives until Drop")
            .write_all(bytes)
    }

    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        self.master
            .as_ref()
            .expect("master lives until Drop")
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| io::Error::other(e.to_string()))
    }

    fn ready_nonce(&self) -> Option<&str> {
        self.ready_nonce.as_deref()
    }

    /// The foreground process-group leader's comm name, unless it is the
    /// shell itself (an idle prompt shows no indicator). Cheap enough for a
    /// 1s poll: one tcgetpgrp + a targeted sysinfo refresh.
    #[cfg(unix)]
    fn foreground_process_name(&self) -> Option<String> {
        let pid = self.master.as_ref()?.process_group_leader()?;
        let name = crate::proctree::process_name(pid)?;
        if name.trim_start_matches('-') == self.shell_name {
            None
        } else {
            Some(name)
        }
    }
}

impl Drop for PtyHandle {
    fn drop(&mut self) {
        // Teardown must not block the gpui thread: the killer / child /
        // master / writer move onto a detached thread that scans the tree,
        // SIGTERMs it (plus the foreground process group), grants a short
        // grace, and SIGKILLs survivors. Every master-side fd this handle
        // owns travels with the thread, so the struct's own field drops
        // close nothing that could disturb the tree before the scan. The
        // reader / waiter threads exit on their own once the child dies
        // (EOF / reap) — they own their reader fd / child handle and
        // channel-sender clones, so they are safe to outlive this handle.
        #[cfg(unix)]
        {
            let fg_pgid = self.master.as_ref().and_then(|m| m.process_group_leader());
            let killer = self.killer.take();
            let child = self.child.take();
            let shell_pid = self.child_pid;
            let master = self.master.take();
            let writer = self.writer.get_mut().take();
            let _ = thread::Builder::new()
                .name("manox-pty-teardown".into())
                .spawn(move || {
                    crate::proctree::terminate(shell_pid, fg_pgid, killer, child);
                    drop(master);
                    drop(writer);
                });
        }
        #[cfg(not(unix))]
        {
            if let Some(killer) = &self.killer {
                let _ = killer.kill();
            }
            // If `start` was never called the child is still here — reap it
            // directly so it isn't orphaned. After `start` the child was
            // moved into the waiter thread and this is `None`.
            if let Some(mut child) = self.child.take() {
                let _ = child.wait();
            }
        }
        // Drop the join handles to detach the threads.
        self.reader_thread.take();
        self.wait_thread.take();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    /// Spawn a wrapped `/bin/sh`, run `setup` in it, and return the live
    /// handle plus its shell pid.
    fn spawn_shell(
        setup: &[u8],
    ) -> (
        PtyHandle,
        libc::pid_t,
        async_channel::Receiver<TerminalEvent>,
    ) {
        let (event_tx, event_rx) = async_channel::bounded::<TerminalEvent>(256);
        let mut pty = open(&PathBuf::from("/tmp"), 80, 24, Some("/bin/sh"), &[]).expect("open pty");
        let shell_pid = pty.child_pid().expect("shell pid captured at spawn");
        pty.start(event_tx);
        std::thread::sleep(Duration::from_millis(150));
        pty.write(setup).expect("write setup input");
        (pty, shell_pid, event_rx)
    }

    /// Snapshot the shell's descendant tree, retrying until the setup command
    /// had time to spawn its children.
    fn snapshot_tree(shell_pid: libc::pid_t) -> Vec<libc::pid_t> {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let tree = crate::proctree::descendant_pids(shell_pid);
            if !tree.is_empty() {
                return tree;
            }
            assert!(
                Instant::now() < deadline,
                "child process never appeared in the shell's tree"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// After drop, every pid in the snapshot (and the shell itself) must be
    /// gone — kill(pid, 0) liveness, generous deadline for launchd reaping.
    fn assert_tree_gone(shell_pid: libc::pid_t, tree: &[libc::pid_t]) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let survivors: Vec<_> = std::iter::once(shell_pid)
                .chain(tree.iter().copied())
                .filter(|&p| crate::proctree::alive(p))
                .collect();
            if survivors.is_empty() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "tree survived teardown: {}",
                describe_survivors(&survivors)
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Survivor pids plus their /proc stat on Linux, so a CI failure shows
    /// state (zombie/running) and parent without a reproducer.
    #[cfg(target_os = "linux")]
    fn describe_survivors(survivors: &[libc::pid_t]) -> String {
        survivors
            .iter()
            .map(|p| {
                let stat = std::fs::read_to_string(format!("/proc/{p}/stat"))
                    .unwrap_or_else(|_| "<gone>".into());
                format!("{p}: {stat}")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[cfg(not(target_os = "linux"))]
    fn describe_survivors(survivors: &[libc::pid_t]) -> String {
        format!("{survivors:?}")
    }

    /// The shell's whole tree dies with the handle — a background job living
    /// in its own process group is not leaked when the terminal closes.
    #[test]
    fn teardown_kills_process_tree() {
        let (pty, shell_pid, _rx) = spawn_shell(b"sleep 300 &\r");
        let tree = snapshot_tree(shell_pid);
        drop(pty);
        assert_tree_gone(shell_pid, &tree);
    }

    /// A tree member that ignores SIGTERM still dies: teardown escalates to
    /// SIGKILL after the grace window. (Ignored dispositions survive exec, so
    /// the sleep below has SIGTERM ignored.)
    #[test]
    fn teardown_escalates_to_sigkill() {
        let (pty, shell_pid, _rx) = spawn_shell(b"sh -c 'trap \"\" TERM; exec sleep 301' &\r");
        let tree = snapshot_tree(shell_pid);
        drop(pty);
        assert_tree_gone(shell_pid, &tree);
    }
}
