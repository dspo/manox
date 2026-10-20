//! DNS entry point of the `web_fetch` transport guard: every hostname the tool
//! connects to must resolve exclusively to public unicast addresses, and the
//! connection is pinned to that validated answer set (reqwest only ever dials
//! addresses produced by the resolver, so validation and pinning are the same
//! mechanism). IP-literal URLs bypass the resolver entirely and are checked
//! before the request starts (see `core`).
//!
//! Two address spellings need more than a range check, because the address the
//! host connects to is not the destination that answers:
//!
//! - **IPv4-mapped and well-known-DNS64 IPv6** carry the real IPv4 destination
//!   in their low 32 bits, so the embedded address is what gets validated.
//! - **A network's own DNS64 prefix** (RFC 7050's `ipv4only.arpa` names it) is
//!   discovered on the fly and every synthesized answer is validated the same
//!   way, so an IPv6-only network cannot be used to reach a private IPv4
//!   destination. Every other transition/translation prefix stays refused
//!   outright: 6to4, Teredo, the RFC 6052 well-known prefix and the rest of
//!   IANA's special-purpose space have no validated destination to pin.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::str::FromStr;
use std::sync::Arc;

use parking_lot::Mutex;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use super::core::WebFetchError;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Shared slot where the resolver and the redirect policy record the
/// structured error that ends the request; `core` reads it back when reqwest
/// surfaces the failure as an opaque transport error.
pub(super) type Slot = Arc<Mutex<Option<WebFetchError>>>;

/// RFC 6052 prefix lengths that may carry an IPv4 destination through NAT64.
const RFC6052_PREFIX_LENGTHS: [u8; 6] = [32, 40, 48, 56, 64, 96];

/// RFC 7050's reserved discovery hostname: only a DNS64 synthesizes AAAA
/// answers for it, and those answers carry the network's own prefix.
const IPV4ONLY_DISCOVERY_HOST: &str = "ipv4only.arpa";

/// The two IPv4 addresses `ipv4only.arpa` names, which is how a synthesized
/// answer is recognized as one.
const IPV4ONLY_SENTINELS: [Ipv4Addr; 2] =
    [Ipv4Addr::new(192, 0, 0, 170), Ipv4Addr::new(192, 0, 0, 171)];

/// A public address is global unicast: not loopback, private, link-local,
/// shared/CGNAT, multicast, reserved, documentation, benchmarking, or one of
/// the transition/translation prefixes. Implemented as an explicit rejection
/// table rather than `Ipv4Addr::is_global` so the exact policy is pinned by
/// tests.
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
        || a == 192 && b == 31 && c == 196 // AS112 direct delegation (RFC 7535)
        || a == 192 && b == 52 && c == 193 // AMT (RFC 7450)
        || a == 192 && b == 88 && c == 99 // 6to4 relay anycast (deprecated)
        || a == 192 && b == 168 // private
        || a == 192 && b == 175 && c == 48 // AS112 direct delegation (RFC 7534)
        || a == 198 && b & 0xfe == 18 // benchmarking 198.18.0.0/15
        || a == 198 && b == 51 && c == 100 // documentation TEST-NET-2
        || a == 203 && b == 0 && c == 113 // documentation TEST-NET-3
        || a >= 224) // multicast 224/4 + reserved 240/4 incl. broadcast
}

fn is_public_ipv6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // Addresses whose effective destination is an embedded IPv4 address are
    // judged by that address instead of by the wrapper.
    if ipv4_mapped(s) {
        return is_public_ipv4(embedded_ipv4(s));
    }
    if well_known_dns64(s) {
        return is_public_ipv4(embedded_ipv4(s));
    }
    !(s[0..6] == [0, 0, 0, 0, 0, 0] // ::/96: unspecified, loopback, v4-compatible
        || s[0] == 0x100 && s[1..4] == [0, 0, 0] // 100::/64 discard-only
        || s[0] == 0x2001 && s[1] & 0xfe00 == 0 // 2001::/23: Teredo, benchmarking, AMT, AS112
        || s[0] == 0x2001 && [0x10, 0x20, 0x30].contains(&(s[1] & 0xfff0)) // deprecated, ORCHIDv2, drone
        || s[0] == 0x2001 && s[1] == 0xdb8 // documentation
        || s[0] == 0x2002 // 6to4 (RFC 3056)
        || s[0] == 0x2620 && s[1] == 0x4f && s[2] == 0x8000 // AS112 v6 (RFC 7535)
        || s[0..4] == [0, 0, 0, 0] && s[4] == 0xffff && s[5] == 0 // IPv4-translated (RFC 6145)
        || s[0] & 0xfe00 == 0xfc00 // unique local fc00::/7
        || s[0] & 0xffc0 == 0xfe80 // link-local fe80::/10
        || s[0] & 0xff00 == 0xff00) // multicast ff00::/8
}

/// `::ffff:0:0/96`, the IPv4-mapped form URL hosts and DNS answers use.
fn ipv4_mapped(segments: [u16; 8]) -> bool {
    segments[0..5] == [0, 0, 0, 0, 0] && segments[5] == 0xffff
}

/// `64:ff9b::/96`, the RFC 6052 well-known prefix DNS64 uses when it has no
/// network-specific one. Accepted here by its embedded address: the synthesized
/// address is the only route to that IPv4 destination on such a network, and
/// pinning it is exactly as strong as pinning the IPv4 answer would be.
fn well_known_dns64(segments: [u16; 8]) -> bool {
    segments[0] == 0x64 && segments[1] == 0xff9b && segments[2..6] == [0, 0, 0, 0]
}

fn embedded_ipv4(segments: [u16; 8]) -> Ipv4Addr {
    Ipv4Addr::new(
        (segments[6] >> 8) as u8,
        segments[6] as u8,
        (segments[7] >> 8) as u8,
        segments[7] as u8,
    )
}

/// One DNS64 prefix this network synthesizes answers under: the prefix octets
/// plus the RFC 6052 length they occupy.
struct Nat64Prefix {
    octets: Vec<u8>,
    length: u8,
}

/// The IPv4 address an RFC 6052 mapping embeds, or `None` when the address has
/// the reserved octet set (which makes it a different mapping).
fn nat64_embedded_ipv4(octets: &[u8; 16], length: u8) -> Option<Ipv4Addr> {
    if length == 96 {
        return Some(Ipv4Addr::new(
            octets[12], octets[13], octets[14], octets[15],
        ));
    }
    if octets[8] != 0 {
        return None;
    }
    let prefix_octets = (length / 8) as usize;
    let leading = 8 - prefix_octets;
    let mut ipv4 = [0u8; 4];
    ipv4[..leading].copy_from_slice(&octets[prefix_octets..8]);
    ipv4[leading..].copy_from_slice(&octets[9..13 - leading]);
    Some(Ipv4Addr::from(ipv4))
}

/// The IPv4 address an answer reaches through one of the network's own DNS64
/// prefixes, when it is an answer under such a prefix.
fn nat64_translation(ip: IpAddr, prefixes: &[Nat64Prefix]) -> Option<Ipv4Addr> {
    let IpAddr::V6(v6) = ip else {
        return None;
    };
    let octets = v6.octets();
    let prefix = prefixes
        .iter()
        .find(|prefix| octets.starts_with(&prefix.octets))?;
    nat64_embedded_ipv4(&octets, prefix.length)
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
            // Only an IPv6 answer can reach a destination through NAT64, so the
            // discovery query is spent only where it can matter.
            let prefixes = if addrs.iter().any(SocketAddr::is_ipv6) {
                discover_nat64_prefixes(&inner).await?
            } else {
                Vec::new()
            };
            for addr in &addrs {
                if !is_public_unicast(addr.ip()) {
                    *blocked.lock() = Some(WebFetchError::BlockedUrl(format!(
                        "host {host} resolves to non-public address {addr}"
                    )));
                    return Err(format!("non-public address {addr} for {host}").into());
                }
                if let Some(translated) = nat64_translation(addr.ip(), &prefixes)
                    && !is_public_unicast(IpAddr::V4(translated))
                {
                    *blocked.lock() = Some(WebFetchError::BlockedUrl(format!(
                        "host {host} resolves through NAT64 to non-public IPv4 {translated}"
                    )));
                    return Err(
                        format!("non-public NAT64 destination {translated} for {host}").into(),
                    );
                }
            }
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Discover the network's active DNS64 prefixes (RFC 7050) through the
/// unguarded resolver: the reserved hostname is answered only by a DNS64, and
/// a synthesized answer embeds the prefix it was built with.
///
/// A discovery query that fails fails the fetch: without the prefix set there
/// is no way to tell an ordinary IPv6 destination from one that translates to
/// a private IPv4 address.
async fn discover_nat64_prefixes(inner: &Arc<dyn Resolve>) -> Result<Vec<Nat64Prefix>, BoxError> {
    let name = Name::from_str(IPV4ONLY_DISCOVERY_HOST)
        .map_err(|e| -> BoxError { format!("invalid discovery hostname: {e}").into() })?;
    let answers = inner
        .resolve(name)
        .await
        .map_err(|e| -> BoxError { format!("DNS64 prefix discovery failed: {e}").into() })?;
    let mut prefixes: Vec<Nat64Prefix> = Vec::new();
    for addr in answers {
        let IpAddr::V6(v6) = addr.ip() else {
            continue;
        };
        let octets = v6.octets();
        for length in RFC6052_PREFIX_LENGTHS {
            let Some(embedded) = nat64_embedded_ipv4(&octets, length) else {
                continue;
            };
            if !IPV4ONLY_SENTINELS.contains(&embedded) {
                continue;
            }
            let octets = octets[..(length / 8) as usize].to_vec();
            if !prefixes
                .iter()
                .any(|prefix| prefix.length == length && prefix.octets == octets)
            {
                prefixes.push(Nat64Prefix { octets, length });
            }
        }
    }
    Ok(prefixes)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

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
            v4("192.31.196.1"),    // AS112 direct delegation
            v4("192.52.193.1"),    // AMT
            v4("192.88.99.1"),     // 6to4 relay anycast
            v4("192.168.1.1"),     // private
            v4("192.175.48.1"),    // AS112 direct delegation
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
            v6("::ffff:0:7f00:1"),    // RFC 6145 translation of loopback
            v6("64:ff9b::127.0.0.1"), // DNS64 of loopback
            v6("100::1"),             // discard-only
            v6("2001::1"),            // Teredo / 2001::/23
            v6("2001:2::1"),          // benchmarking
            v6("2001:3::1"),          // AMT
            v6("2001:4:112::1"),      // AS112 v6
            v6("2001:10::1"),         // deprecated ORCHID
            v6("2001:20::1"),         // ORCHIDv2
            v6("2001:30::1"),         // drone remote-id
            v6("2001:db8::1"),        // documentation
            v6("2002:0a00:0001::1"),  // 6to4 of 10.0.0.1
            v6("2002:7f00:0001::1"),  // 6to4 of 127.0.0.1
            v6("2620:4f:8000::1"),    // AS112 v6
            v6("fc00::1"),            // unique local
            v6("fd12:3456::1"),       // unique local
            v6("fe80::1"),            // link-local
            v6("ff02::1"),            // multicast
        ] {
            assert!(!is_public_unicast(ip), "{ip} should be rejected");
        }
        // The wrappers of a public IPv4 address stay reachable: they are the
        // only route to it on a translated or IPv6-only network.
        assert!(is_public_unicast(v6("64:ff9b::1.1.1.1")));
        assert!(is_public_unicast(v6("::ffff:1.1.1.1")));
    }

    /// RFC 6052 §2.3: prefix 2001:db8::/PL with IPv4 192.0.2.33, for every
    /// prefix length this resolver discovers.
    #[test]
    fn nat64_embedded_ipv4_follows_rfc6052_layouts() {
        for (address, length) in [
            ("2001:db8:c000:221::", 32),
            ("2001:db8:1c0:2:21::", 40),
            ("2001:db8:122:c000:2:2100::", 48),
            ("2001:db8:122:3c0:0:221::", 56),
            ("2001:db8:122:344:c0:2:2100::", 64),
            ("2001:db8:122:344::c000:221", 96),
        ] {
            let octets = Ipv6Addr::from_str(address).unwrap().octets();
            assert_eq!(
                nat64_embedded_ipv4(&octets, length),
                Some(Ipv4Addr::new(192, 0, 2, 33)),
                "{address}/{length}"
            );
        }
        // Below /96 the reserved octet must be clear; a mapping that sets it
        // carries no address this resolver can read.
        let mut octets = Ipv6Addr::from_str("2001:db8:122:c000:2:2100::")
            .unwrap()
            .octets();
        octets[8] = 1;
        assert_eq!(nat64_embedded_ipv4(&octets, 48), None);
    }

    struct StaticResolver {
        map: HashMap<String, Vec<SocketAddr>>,
    }

    impl Resolve for StaticResolver {
        fn resolve(&self, name: Name) -> Resolving {
            let host = name.as_str().to_string();
            let addrs = self.map.get(host.as_str()).cloned().unwrap_or_default();
            Box::pin(async move {
                if addrs.is_empty() {
                    return Err(format!("no static mapping for {host}").into());
                }
                Ok(Box::new(addrs.into_iter()) as Addrs)
            })
        }
    }

    /// Test answers are written as a bare address or `address:port`.
    fn parse_addr(input: &str) -> SocketAddr {
        input
            .parse()
            .or_else(|_| input.parse::<IpAddr>().map(|ip| SocketAddr::new(ip, 0)))
            .expect("test address parses")
    }

    fn unicast_resolver(entries: &[(&str, &[&str])]) -> PublicUnicastResolver {
        let mut map = HashMap::new();
        for (host, addrs) in entries {
            map.insert(
                (*host).to_string(),
                addrs.iter().map(|addr| parse_addr(addr)).collect(),
            );
        }
        PublicUnicastResolver::new(Arc::new(StaticResolver { map }), Arc::new(Mutex::new(None)))
    }

    async fn resolve_host(
        resolver: &PublicUnicastResolver,
        host: &str,
    ) -> Result<Vec<SocketAddr>, String> {
        resolver
            .resolve(Name::from_str(host).unwrap())
            .await
            .map(|addrs| addrs.collect())
            .map_err(|e| e.to_string())
    }

    /// A synthesized answer under the network's own prefix routes to the IPv4
    /// address it embeds, so that address decides.
    #[tokio::test]
    async fn network_nat64_prefix_is_discovered_and_validated() {
        let resolver = unicast_resolver(&[
            ("ipv4only.arpa", &["2a00:1:2:3:4:5:c000:aa"]),
            ("nat64-private.test", &["2a00:1:2:3:4:5:a00:1"]),
            ("nat64-public.test", &["2a00:1:2:3:4:5:101:101"]),
            ("plain.test", &["2606:4700:4700::1111"]),
        ]);
        let err = resolve_host(&resolver, "nat64-private.test")
            .await
            .unwrap_err();
        assert!(err.contains("NAT64"), "{err}");
        let ok = resolve_host(&resolver, "nat64-public.test")
            .await
            .expect("public translation");
        assert_eq!(ok.len(), 1);
        let ok = resolve_host(&resolver, "plain.test")
            .await
            .expect("no NAT64 involved");
        assert_eq!(ok.len(), 1);
    }

    /// Without a discovered prefix there is nothing to compare against, so the
    /// answer set is judged on its own — the well-known prefix is judged by its
    /// embedded address instead.
    #[tokio::test]
    async fn well_known_dns64_answers_are_judged_by_their_embedded_address() {
        let resolver = unicast_resolver(&[
            ("ipv4only.arpa", &["192.0.0.170"]),
            ("dns64.test", &["64:ff9b::101:101"]),
            ("dns64-private.test", &["64:ff9b::a00:1"]),
        ]);
        assert_eq!(
            resolve_host(&resolver, "dns64.test")
                .await
                .expect("public")
                .len(),
            1
        );
        let err = resolve_host(&resolver, "dns64-private.test")
            .await
            .unwrap_err();
        assert!(err.contains("non-public"), "{err}");
    }

    /// A discovery query that fails fails the fetch: an unvalidated prefix set
    /// cannot tell an ordinary IPv6 answer from a translated one.
    #[tokio::test]
    async fn nat64_discovery_failure_fails_ipv6_resolution() {
        let resolver = unicast_resolver(&[("ipv6-only.test", &["2606:4700:4700::1111"])]);
        let err = resolve_host(&resolver, "ipv6-only.test").await.unwrap_err();
        assert!(err.contains("discovery failed"), "{err}");
        // An IPv4-only answer never spends the discovery query.
        let ipv4_only = unicast_resolver(&[("ipv4-only.test", &["93.184.216.34"])]);
        assert_eq!(
            resolve_host(&ipv4_only, "ipv4-only.test")
                .await
                .expect("public")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn hostname_resolving_to_private_space_is_blocked() {
        let resolver = unicast_resolver(&[("private.test", &["127.0.0.1:80"])]);
        let err = resolve_host(&resolver, "private.test").await.unwrap_err();
        assert!(err.contains("non-public"), "{err}");
    }
}
