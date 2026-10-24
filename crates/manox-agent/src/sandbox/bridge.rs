//! The host half of the managed-route network bridge.
//!
//! Under the managed route the sandbox runs in its own network namespace
//! (bubblewrap `--unshare-net`), where the inner helper
//! ([`super::inner`]) binds a loopback listener and hands it back over a
//! socketpair created before the spawn. This module owns the host end: it
//! receives the descriptor (`SCM_RIGHTS`), validates its shape, and runs a
//! relay that copies every accepted connection to the session's real
//! loopback proxy port (the CONNECT judge in `netpolicy`). The namespace is
//! the confinement — an in-namespace client can reach nothing but the
//! bridge — so the relay is a dumb byte pipe with no policy of its own.
//!
//! The relay is plain std threads, not tokio: the background registry
//! builds sandboxed commands outside an async context, so both the
//! foreground call and a background task share one handoff/relay path. Its
//! lifetime rides the control channel: the inner keeps its end open for
//! exactly the target command's lifetime, so EOF here tears the relay down
//! as the sandbox dies — no per-call cleanup for the caller to remember.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// The control-channel byte the inner marks its listener with (see
/// `sandbox::inner`).
const BRIDGE_LISTENER: u8 = 1;
const BRIDGE_READY: u8 = 1;

/// How long the host waits for the inner's handoff. A healthy inner
/// answers in milliseconds; the deadline covers a loaded machine before
/// the call fails closed.
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(10);

/// The host end of one managed run's control channel.
pub(super) struct BridgeChannel {
    /// The host end of the socketpair: receives the listener, carries the
    /// ready acknowledgment, then watches for EOF (the inner exits when the
    /// sandboxed command does).
    control: Option<UnixStream>,
    /// The host's copy of the inner end. The fork child clears `CLOEXEC` on
    /// its own copy in the `pre_exec` hook; this copy is dead weight once
    /// the spawn's fork has happened and is closed when the handoff
    /// settles — a successful receive proves the fork, and the deadline
    /// bounds the not-yet-spawned window on the background path.
    inner_copy: Option<RawFd>,
}

impl Drop for BridgeChannel {
    fn drop(&mut self) {
        if let Some(fd) = self.inner_copy.take() {
            // SAFETY: retiring a descriptor this struct owns.
            unsafe { libc::close(fd) };
        }
    }
}

impl BridgeChannel {
    /// Create the socketpair: the host end stays in the parent (`CLOEXEC`,
    /// so it never leaks into unrelated spawns), the inner end is returned
    /// for the `pre_exec` hook and the sentinel environment.
    pub(super) fn pair() -> std::io::Result<(Self, RawFd)> {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: plain descriptor allocation into a two-slot stack array;
        // SOCK_CLOEXEC keeps both ends out of any exec they do not opt into.
        let rc = unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
                fds.as_mut_ptr(),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the descriptor came straight from socketpair and this
        // struct owns it from here.
        let control = unsafe { UnixStream::from_raw_fd(fds[0]) };
        Ok((
            Self {
                control: Some(control),
                inner_copy: Some(fds[1]),
            },
            fds[1],
        ))
    }

    /// Receive the netns listener, validate it, acknowledge readiness, and
    /// hand back the relay. Blocking — the foreground call runs it on the
    /// blocking pool, the background task on its detached thread.
    pub(super) fn handoff(mut self, upstream_port: u16) -> std::io::Result<Relay> {
        let mut control = self
            .control
            .take()
            .ok_or_else(|| std::io::Error::other("control channel already consumed"))?;
        control.set_read_timeout(Some(HANDOFF_TIMEOUT))?;
        let listener = receive_listener(&control)?;
        control.set_write_timeout(Some(HANDOFF_TIMEOUT))?;
        control.write_all(&[BRIDGE_READY])?;
        // The fork is proven by now: the host's copy of the inner end can
        // go (the child carries its own, CLOEXEC cleared in the hook).
        if let Some(fd) = self.inner_copy.take() {
            // SAFETY: retiring a descriptor this struct owns.
            unsafe { libc::close(fd) };
        }
        let _ = control.set_read_timeout(None);
        Ok(Relay {
            listener,
            control,
            upstream: SocketAddr::from(([127, 0, 0, 1], upstream_port)),
        })
    }

    /// Handoff plus relay on a detached OS thread. The background path:
    /// the registry spawns the command itself, so the waiter races it with
    /// the same deadline; a task that never spawns simply times the waiter
    /// out and the call never ran.
    pub(super) fn spawn_detached(self, upstream_port: u16) {
        std::thread::Builder::new()
            .name("manox-sandbox-bridge".into())
            .spawn(move || match self.handoff(upstream_port) {
                Ok(relay) => relay.run(),
                Err(error) => {
                    tracing::warn!(%error, "sandbox bridge handoff failed; the sandboxed call stays without network");
                }
            })
            // A failed spawn of the waiter thread fails the handoff (the
            // inner waits out its deadline); nothing else can do better.
            .map_err(|error| tracing::warn!(%error, "sandbox bridge waiter failed to spawn"))
            .ok();
    }
}

/// One managed run's relay: the netns listener plus the control end whose
/// EOF marks the sandbox's death.
pub(super) struct Relay {
    listener: TcpListener,
    control: UnixStream,
    upstream: SocketAddr,
}

impl Relay {
    /// Run the relay loop until the control channel hits EOF (the inner
    /// exited, so the sandbox is gone). Blocking; run it on a dedicated
    /// thread.
    pub(super) fn run(mut self) {
        let _ = self.listener.set_nonblocking(true);
        let mut polls = [
            libc::pollfd {
                fd: self.listener.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.control.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: poll over the two stack slots; a negative timeout
            // blocks until an fd is ready.
            let ready = unsafe { libc::poll(polls.as_mut_ptr(), 2, -1) };
            if ready < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return;
            }
            if polls[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                // EOF (or a stray byte) from the inner: the sandbox is
                // gone; drain the byte if any and stop.
                let mut byte = [0u8; 1];
                let _ = self.control.read(&mut byte);
                return;
            }
            if polls[0].revents & libc::POLLIN != 0 {
                loop {
                    match self.listener.accept() {
                        Ok((client, _)) => {
                            relay_connection(client, self.upstream);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                        // ECONNABORTED and friends: wait for the next one.
                        Err(_) => break,
                    }
                }
            }
        }
    }

    /// Hand the relay to a detached thread (the foreground path: `exec`
    /// continues draining the command while the relay serves it).
    pub(super) fn spawn_relay_thread(self) {
        let _ = std::thread::Builder::new()
            .name("manox-sandbox-relay".into())
            .spawn(move || self.run());
    }
}

/// Copy one accepted connection to the upstream proxy on a detached
/// thread. A failed upstream dial just closes the client — the trapped
/// client sees the connection die, exactly like a refused proxy hop.
fn relay_connection(client: TcpStream, upstream: SocketAddr) {
    let _ = std::thread::Builder::new()
        .name("manox-sandbox-bridge-conn".into())
        .spawn(move || {
            let Ok(mut upstream_stream) = TcpStream::connect(upstream) else {
                return;
            };
            let mut client = client;
            // Linux accepts inherit the listener's O_NONBLOCK; the copies below
            // are plain blocking splices, so the accepted end goes back to
            // blocking.
            let _ = client.set_nonblocking(false);
            // One direction per thread, shutdown propagated both ways — the
            // shape a byte-level TCP splice needs without async.
            let mut client_write = match client.try_clone() {
                Ok(clone) => clone,
                Err(_) => return,
            };
            let mut upstream_read = match upstream_stream.try_clone() {
                Ok(clone) => clone,
                Err(_) => return,
            };
            let to_client = std::thread::spawn(move || {
                let _ = std::io::copy(&mut upstream_read, &mut client_write);
                let _ = client_write.shutdown(std::net::Shutdown::Write);
            });
            let _ = std::io::copy(&mut client, &mut upstream_stream);
            let _ = upstream_stream.shutdown(std::net::Shutdown::Write);
            let _ = to_client.join();
        });
}

/// Receive the listener descriptor from the inner: one marker byte plus a
/// `SCM_RIGHTS` record carrying exactly one stream socket bound to IPv4
/// loopback on a nonzero port. The received descriptor arrives CLOEXEC.
fn receive_listener(control: &UnixStream) -> std::io::Result<TcpListener> {
    use std::io::IoSliceMut;
    let mut byte = [0u8; 1];
    let mut space = [0u8; rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = rustix::net::RecvAncillaryBuffer::new(&mut space);
    let message = rustix::io::retry_on_intr(|| {
        rustix::net::recvmsg(
            control,
            &mut [IoSliceMut::new(&mut byte)],
            &mut ancillary,
            rustix::net::RecvFlags::CMSG_CLOEXEC,
        )
    })?;
    // Drain everything received: owned descriptors close on drop, so even
    // a rejected message cannot leak one.
    let mut descriptors = Vec::new();
    let mut unexpected = false;
    for entry in ancillary.drain() {
        match entry {
            rustix::net::RecvAncillaryMessage::ScmRights(received) => descriptors.extend(received),
            _ => unexpected = true,
        }
    }
    let shape_ok = message.bytes == 1
        && byte == [BRIDGE_LISTENER]
        && !message.flags.intersects(rustix::net::RecvFlags::TRUNC)
        && !unexpected;
    if !shape_ok || descriptors.len() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid netns listener handoff",
        ));
    }
    let listener = TcpListener::from(descriptors.remove(0));
    // Shape checks: a listening TCP socket bound to loopback on a real
    // port. The descriptor came over our own channel, but the check is
    // cheap and pins the contract.
    let listens = rustix::net::sockopt::get_socket_acceptconn(&listener).unwrap_or(false);
    if !listens {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "netns handoff is not a listening socket",
        ));
    }
    match listener.local_addr()? {
        SocketAddr::V4(addr) if addr.ip().is_loopback() && addr.port() != 0 => Ok(listener),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "netns listener must bind IPv4 loopback on an ephemeral port",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::mpsc;

    /// The full handoff choreography in one process: the inner side runs on
    /// a thread doing exactly what `inner_main` does (claim the raw
    /// descriptor, bind loopback, send, await the ready acknowledgment,
    /// hold the control end for the run), then the relay serves a client
    /// round-trip and dies with the inner's EOF.
    #[test]
    fn bridge_round_trip_relays_until_the_inner_exit_eof() {
        let (channel, inner_fd) = BridgeChannel::pair().unwrap();
        // The host retires its copy of the inner descriptor when the
        // handoff settles; a separate process would keep its own fd table,
        // so the in-process stand-in needs a duplicate to survive it.
        let inner_fd = unsafe { libc::dup(inner_fd) };
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let stub = std::thread::spawn(move || {
            let (mut sock, _) = upstream.accept().unwrap();
            let mut request = [0u8; 4];
            sock.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"ping");
            sock.write_all(b"okok").unwrap();
        });

        // The inner side. It parks on a channel after the handshake so its
        // control end stays open — exactly the real lifetime, where the
        // inner holds the end until the sandboxed command finishes.
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let inner = std::thread::spawn(move || {
            // SAFETY: the descriptor came from the socketpair; this thread
            // owns it for the test's duration.
            let mut control = unsafe { UnixStream::from_raw_fd(inner_fd) };
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            super::super::inner::send_listener(&control, &listener).unwrap();
            drop(listener);
            let mut ready = [0u8; 1];
            control.read_exact(&mut ready).unwrap();
            assert_eq!(ready, [BRIDGE_READY], "host acked the handoff");
            release_rx.recv().expect("the test is still alive");
            // `control` drops here: the inner exited, the channel closed.
        });

        let relay = channel.handoff(upstream_port).unwrap();
        let netns_port = relay.listener.local_addr().unwrap().port();
        let relay_thread = std::thread::spawn(move || relay.run());

        // A trapped client round-trips through the bridge and the stub.
        let mut client = TcpStream::connect(("127.0.0.1", netns_port)).unwrap();
        client.write_all(b"ping").unwrap();
        let mut answer = [0u8; 4];
        client.read_exact(&mut answer).unwrap();
        assert_eq!(&answer, b"okok", "bytes flow both directions");
        drop(client);
        stub.join().unwrap();

        // The inner exits: its control end drops, the relay sees EOF and
        // stops — the relay never outlives its sandbox.
        release_tx.send(()).unwrap();
        inner.join().unwrap();
        relay_thread
            .join()
            .expect("the relay terminates on the control EOF");
    }

    /// A rejected handoff shape: garbage on the channel fails the handoff
    /// instead of yielding a relay (fail-closed, and the received
    /// descriptors do not leak).
    #[test]
    fn handoff_rejects_a_shapeless_message() {
        let (channel, inner_fd) = BridgeChannel::pair().unwrap();
        // The channel retires its copy of the inner descriptor; the
        // in-process stand-in claims a duplicate (a real inner runs in a
        // separate process with its own table).
        let inner_fd = unsafe { libc::dup(inner_fd) };
        let writer = std::thread::spawn(move || {
            // SAFETY: the duplicated descriptor is owned by this thread.
            let mut control = unsafe { UnixStream::from_raw_fd(inner_fd) };
            // The right marker byte but no descriptor: the shape check
            // must refuse it.
            control.write_all(&[BRIDGE_LISTENER]).unwrap();
        });
        let error = match channel.handoff(1) {
            Ok(_) => panic!("a message without a descriptor cannot become a relay"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData, "{error}");
        writer.join().unwrap();
    }
}
