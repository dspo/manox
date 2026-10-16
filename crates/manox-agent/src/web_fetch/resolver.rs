//! DNS entry point of the `web_fetch` transport guard: every hostname the
//! tool connects to must resolve exclusively to public unicast addresses, and
//! the connection is pinned to that validated answer set (reqwest only ever
//! dials addresses produced by the resolver, so validation and pinning are the
//! same mechanism). IP-literal URLs bypass the resolver entirely and are
//! checked before the request starts (see `core`).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use parking_lot::Mutex;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use super::core::WebFetchError;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Shared slot where the resolver and the redirect policy record the
/// structured error that ends the request; `core` reads it back when reqwest
/// surfaces the failure as an opaque transport error.
pub(super) type Slot = Arc<Mutex<Option<WebFetchError>>>;

/// A public address is global unicast: not loopback, private, link-local,
/// shared/CGNAT, multicast, reserved, documentation, or benchmarking space.
/// Implemented as an explicit rejection table rather than `Ipv4Addr::
/// is_global` so the exact policy is pinned by tests.
pub(super) fn is_public_unicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_ipv4(v4),
        IpAddr::V6(v6) => is_public_ipv6(v6),
    }
}

fn is_public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0 // 0.0.0.0/8 "this network"
        || a == 10 // private
        || a == 100 && b & 0xc0 == 64 // shared / CGNAT 100.64.0.0/10
        || a == 127 // loopback
        || a == 169 && b == 254 // link-local
        || a == 172 && b & 0xf0 == 16 // private 172.16.0.0/12
        || a == 192 && b == 0 && c == 0 // IETF protocol assignments
        || a == 192 && b == 0 && c == 2 // documentation TEST-NET-1
        || a == 192 && b == 88 && c == 99 // 6to4 relay anycast (deprecated)
        || a == 192 && b == 168 // private
        || a == 198 && b & 0xfe == 18 // benchmarking 198.18.0.0/15
        || a == 198 && b == 51 && c == 100 // documentation TEST-NET-2
        || a == 203 && b == 0 && c == 113 // documentation TEST-NET-3
        || a >= 224) // multicast 224/4 + reserved 240/4 incl. broadcast
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // Embedded-IPv4 spaces validate the embedded address instead of the
    // wrapper. Active-prefix DNS64 discovery is deferred: hosts behind a
    // non-well-known NAT64 prefix are unreachable, an accepted gap.
    if s[0..5] == [0, 0, 0, 0, 0] && s[5] == 0xffff {
        return is_public_ipv4(embedded_ipv4(s));
    }
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return is_public_ipv4(embedded_ipv4(s));
    }
    !(s[0..6] == [0, 0, 0, 0, 0, 0] // ::/96: unspecified, loopback, v4-compatible
        || s[0] == 0x100 && s[1..4] == [0, 0, 0] // 100::/64 discard-only
        || s[0] == 0x2001 && s[1] == 0xdb8 // documentation
        || s[0] & 0xfe00 == 0xfc00 // unique local fc00::/7
        || s[0] & 0xffc0 == 0xfe80 // link-local fe80::/10
        || s[0] & 0xff00 == 0xff00) // multicast ff00::/8
}

fn embedded_ipv4(s: [u16; 8]) -> Ipv4Addr {
    Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8)
}

/// Blocking getaddrinfo resolver equivalent to reqwest's (crate-private)
/// `GaiResolver`: port 0 is a placeholder — the connector replaces it with
/// the URI's effective port.
#[derive(Default)]
pub(super) struct SystemResolver;

impl Resolve for SystemResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> =
                tokio::task::spawn_blocking(move || (host.as_str(), 0).to_socket_addrs())
                    .await
                    .map_err(|e| -> BoxError { format!("dns worker join failed: {e}").into() })?
                    .map_err(|e| -> BoxError { format!("dns resolution failed: {e}").into() })?
                    .collect();
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Wraps an inner resolver with the public-unicast policy: resolution
/// succeeds only when every answered address is public, and the caller only
/// ever sees (and therefore only ever dials) the validated set.
pub(super) struct PublicUnicastResolver {
    inner: Arc<dyn Resolve>,
    blocked: Slot,
}

impl PublicUnicastResolver {
    pub(super) fn new(inner: Arc<dyn Resolve>, blocked: Slot) -> Self {
        Self { inner, blocked }
    }
}

impl Resolve for PublicUnicastResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        let inner = Arc::clone(&self.inner);
        let blocked = Arc::clone(&self.blocked);
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = inner.resolve(name).await?.collect();
            if addrs.is_empty() {
                return Err(format!("empty DNS answer for {host}").into());
            }
            if let Some(bad) = addrs.iter().find(|a| !is_public_unicast(a.ip())) {
                *blocked.lock() = Some(WebFetchError::BlockedUrl(format!(
                    "host {host} resolves to non-public address {bad}"
                )));
                return Err(format!("non-public address {bad} for {host}").into());
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn v6(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn public_addresses_pass() {
        for ip in [
            v4("1.1.1.1"),
            v4("8.8.8.8"),
            v4("93.184.216.34"),
            v4("9.255.255.255"),
            v4("100.63.255.255"), // just below CGNAT: public
        ] {
            assert!(is_public_unicast(ip), "{ip}");
        }
        for ip in [
            v6("2606:4700:4700::1111"),
            v6("2001:4860:4860::8888"),
            v6("2a00:1450:4001:81b::200e"),
        ] {
            assert!(is_public_unicast(ip), "{ip}");
        }
    }

    #[test]
    fn ipv4_special_space_is_rejected() {
        for ip in [
            v4("0.1.2.3"),         // this network
            v4("10.0.0.1"),        // private
            v4("100.64.0.0"),      // shared / CGNAT start
            v4("100.127.255.255"), // shared / CGNAT end
            v4("127.0.0.1"),       // loopback
            v4("169.254.1.1"),     // link-local
            v4("172.16.0.1"),      // private
            v4("172.31.255.255"),  // private end
            v4("192.0.0.1"),       // IETF protocol assignments
            v4("192.0.2.1"),       // documentation
            v4("192.88.99.1"),     // 6to4 relay anycast
            v4("192.168.1.1"),     // private
            v4("198.18.0.1"),      // benchmarking
            v4("198.19.255.255"),  // benchmarking end
            v4("198.51.100.7"),    // documentation
            v4("203.0.113.7"),     // documentation
            v4("224.0.0.1"),       // multicast
            v4("240.0.0.1"),       // reserved
            v4("255.255.255.255"), // broadcast
        ] {
            assert!(!is_public_unicast(ip), "{ip} should be rejected");
        }
    }

    #[test]
    fn ipv6_special_space_is_rejected() {
        for ip in [
            v6("::"),                 // unspecified
            v6("::1"),                // loopback
            v6("::1.2.3.4"),          // v4-compatible (deprecated)
            v6("::ffff:127.0.0.1"),   // v4-mapped loopback → embed rejected
            v6("::ffff:10.0.0.1"),    // v4-mapped private
            v6("64:ff9b::127.0.0.1"), // DNS64 of loopback
            v6("100::1"),             // discard-only
            v6("2001:db8::1"),        // documentation
            v6("fc00::1"),            // unique local
            v6("fd12:3456::1"),       // unique local
            v6("fe80::1"),            // link-local
            v6("ff02::1"),            // multicast
        ] {
            assert!(!is_public_unicast(ip), "{ip} should be rejected");
        }
        // The DNS64 wrapper of a public IPv4 address stays reachable.
        assert!(is_public_unicast(v6("64:ff9b::1.1.1.1")));
        assert!(is_public_unicast(v6("::ffff:1.1.1.1")));
    }
}
