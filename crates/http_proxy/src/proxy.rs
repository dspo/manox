//! The proxy itself: listener, connection handlers.
//!
//! All synchronous, thread-per-connection. `ProxyHandle::spawn` binds a
//! `std::net::TcpListener` on `127.0.0.1:0` and returns once the listener
//! is bound and the listener thread has been spawned. Drop the handle to
//! shut everything down — the listener thread stops accepting new
//! connections; in-flight connection threads finish on their own when
//! either side closes.

mod connection;

use crate::allowlist::Allowlist;
use anyhow::{Context as _, Result};
use futures::channel::mpsc;
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

/// Cap on concurrently handled connections. Each connection costs the
/// editor process two threads and two pump buffers; the cap keeps a
/// runaway (or malicious) sandboxed command from exhausting the process's
/// thread/fd budget.
const MAX_CONCURRENT_CONNECTIONS: usize = 256;

/// Configuration for spawning a proxy.
#[derive(Debug, Clone)]
pub struct ProxyConfig {
    /// Hosts the proxy will allow to be reached.
    pub allowlist: Allowlist,
    /// Where the proxy reports per-connection events. Use
    /// [`mpsc::unbounded`] so connection threads (which are sync) never
    /// block on send.
    pub events: mpsc::UnboundedSender<ProxyEvent>,
}

/// A request method seen by the proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestMethod {
    Connect,
    Http(String),
}

impl RequestMethod {
    pub fn as_str(&self) -> &str {
        match self {
            RequestMethod::Connect => "CONNECT",
            RequestMethod::Http(method) => method.as_str(),
        }
    }
}

/// Outcome of a single connection's policy decision.
#[derive(Debug, Clone)]
pub enum RequestOutcome {
    Allowed,
    Denied { reason: DenyReason },
}

/// Why an attempted connection was denied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    HostNotInAllowlist { host: String },
    IpLiteralRejected { target: String },
    ResolvedToForbiddenIp { host: String },
}

impl DenyReason {
    pub fn human_explanation(&self) -> String {
        match self {
            DenyReason::HostNotInAllowlist { host } => {
                format!("host '{host}' is not in this conversation's network allowlist")
            }
            DenyReason::IpLiteralRejected { target } => format!(
                "target '{target}' is an IP literal; only hostnames are permitted by sandbox policy"
            ),
            DenyReason::ResolvedToForbiddenIp { host } => format!(
                "host '{host}' resolves only to loopback/private/link-local addresses, \
                 which sandbox policy blocks"
            ),
        }
    }
}

/// Events emitted by the proxy as it handles connections.
#[derive(Debug, Clone)]
pub enum ProxyEvent {
    /// Sent once after the listener is bound. Always the first event.
    Ready { port: u16 },

    /// Emitted at policy-decision time, before bytes flow to the upstream.
    RequestAttempt {
        host: String,
        port: u16,
        method: RequestMethod,
        outcome: RequestOutcome,
    },

    /// Emitted after an `Allowed` connection finishes.
    RequestCompleted {
        host: String,
        port: u16,
        method: RequestMethod,
        bytes_to_remote: u64,
        bytes_from_remote: u64,
        duration_ms: u64,
    },
}

/// Handle to a running proxy. Drop to stop the listener; in-flight
/// connection threads finish on their own as soon as either side closes.
pub struct ProxyHandle {
    port: u16,
    /// Listener thread sees this flip to `true` after `accept` returns and
    /// then exits.
    shutdown: Arc<AtomicBool>,
    /// Joined on drop to make shutdown deterministic in tests.
    listener_thread: Option<thread::JoinHandle<()>>,
}

impl ProxyHandle {
    /// Spawns the proxy: binds a listener on `127.0.0.1:0`, spawns the
    /// listener thread, sends a `Ready` event, and returns. The returned
    /// port is what callers should use for `HTTPS_PROXY`/`HTTP_PROXY` env
    /// vars and for the seatbelt rule narrowing `localhost:<port>`.
    pub fn spawn(config: ProxyConfig) -> Result<ProxyHandle> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .context("failed to bind proxy listener on 127.0.0.1:0")?;
        let port = listener
            .local_addr()
            .context("failed to read proxy local addr")?
            .port();

        let _ = config.events.unbounded_send(ProxyEvent::Ready { port });

        let shutdown = Arc::new(AtomicBool::new(false));
        let runtime_state = Arc::new(RuntimeState {
            allowlist: config.allowlist,
            events: config.events,
            active_connections: AtomicUsize::new(0),
        });

        let listener_thread = thread::Builder::new()
            .name("http-proxy-listener".to_string())
            .stack_size(128 * 1024)
            .spawn({
                let shutdown = shutdown.clone();
                move || run_listener(listener, runtime_state, shutdown)
            })
            .context("failed to spawn proxy listener thread")?;

        Ok(ProxyHandle {
            port,
            shutdown,
            listener_thread: Some(listener_thread),
        })
    }

    /// The loopback TCP port clients should use for proxy environment variables.
    pub fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Wake the blocked `accept()` by connecting to ourselves: the listener
        // accepts the connection, sees the shutdown flag, breaks the loop.
        let _ = TcpStream::connect((Ipv4Addr::LOCALHOST, self.port));
        if let Some(thread) = self.listener_thread.take()
            && thread.join().is_err()
        {
            log::warn!("[http_proxy] listener thread panicked");
        }
    }
}

/// State shared across all connection threads for a single proxy instance.
pub(crate) struct RuntimeState {
    pub(crate) allowlist: Allowlist,
    pub(crate) events: mpsc::UnboundedSender<ProxyEvent>,
    pub(crate) active_connections: AtomicUsize,
}

/// Decrements the active-connection count when a connection thread finishes.
struct ConnectionSlot(Arc<RuntimeState>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.active_connections.fetch_sub(1, Ordering::SeqCst);
    }
}

fn run_listener(listener: TcpListener, state: Arc<RuntimeState>, shutdown: Arc<AtomicBool>) {
    for stream in listener.incoming() {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match stream {
            Ok(stream) => spawn_connection(stream, &state),
            Err(e) => {
                log::warn!("[http_proxy] accept failed: {e}");
            }
        }
    }
}

fn spawn_connection(stream: TcpStream, state: &Arc<RuntimeState>) {
    let previous = state.active_connections.fetch_add(1, Ordering::SeqCst);
    if previous >= MAX_CONCURRENT_CONNECTIONS {
        state.active_connections.fetch_sub(1, Ordering::SeqCst);
        log::warn!(
            "[http_proxy] dropping connection: {MAX_CONCURRENT_CONNECTIONS} connections already active"
        );
        drop(stream);
        return;
    }
    let slot = ConnectionSlot(state.clone());
    let state = state.clone();
    let result = thread::Builder::new()
        .name("http-proxy-conn".to_string())
        .stack_size(128 * 1024)
        .spawn(move || {
            let _slot = slot;
            if let Err(error) = connection::handle(stream, state) {
                log::debug!("[http_proxy] connection handler error: {error}");
            }
        });
    if let Err(error) = result {
        log::warn!("[http_proxy] failed to spawn connection thread: {error}");
    }
}
