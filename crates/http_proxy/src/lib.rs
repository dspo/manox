//! In-process HTTP/HTTPS proxy that enforces a hostname allowlist.
//!
//! Spawned per thread that runs sandboxed commands with a restricted network
//! policy. The OS-level sandbox (macOS seatbelt) is configured to permit
//! network only to this proxy's port; everything the sandboxed command tries
//! to reach the network for has to come through here.
//!
//! The proxy:
//!
//! - Speaks HTTP CONNECT for HTTPS tunnels and HTTP forward proxying for plain
//!   HTTP. Other protocols cannot reach it (the seatbelt rule limits the
//!   sandboxed process to this one TCP destination, and this proxy only speaks
//!   HTTP).
//! - Checks the destination hostname against an allowlist of exact hostnames
//!   and leading-`*.` subdomain wildcards. Unless the allowlist allows any
//!   host, IP-literal targets are denied, and hostnames whose DNS resolves
//!   only into loopback / private / link-local space are denied too
//!   (DNS-rebinding protection — the proxy runs outside the sandbox, so it
//!   must not reopen the local network the seatbelt rule closed off).
//! - Pins each TCP connection to the destination approved for its first
//!   request.
//! - Reports per-connection events (allowed, denied, completed) over an mpsc
//!   supplied by the caller.
//!
//! ## Trust assumptions
//!
//! The proxy's sole client is model-driven code running inside the sandbox —
//! exactly the party the sandbox distrusts — and the proxy itself runs inside
//! the editor process. It therefore caps request header sizes and concurrent
//! connections, and bounds connect waits with timeouts, so a malicious command
//! can't exhaust the process's memory, threads, or file descriptors through
//! it.

mod allowlist;
mod proxy;

pub use allowlist::{Allowlist, HostPattern, HostPatternError};
pub use proxy::{DenyReason, ProxyConfig, ProxyEvent, ProxyHandle, RequestMethod, RequestOutcome};
