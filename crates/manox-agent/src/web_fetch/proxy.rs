//! Proxy route decision for `web_fetch`: which of the two transport modes a
//! URL runs under, decided once per call.
//!
//! Sources are consulted in a fixed order: the environment first
//! (`http_proxy` / `https_proxy` / `all_proxy`, lowercase spelling first),
//! then the OS-level system proxy where the platform publishes one, then
//! direct. A proxied hop cannot be address-validated locally — the proxy
//! resolves the origin — so the route decides both the resolver stack and
//! whether the DNS guard applies at all. Redirects stay same-origin, so one
//! decision covers every hop of a chain. The URL-hygiene rules, the
//! IP-literal check, and every cap apply in both modes.
//!
//! The rules follow the convention the rest of the ecosystem reads: a scheme's
//! own variable wins (lowercase spelling first), `ALL_PROXY` is the fallback
//! and HTTPS also falls back to the HTTP proxy, and the bypass list always
//! carries the loopback entries — a proxy on this machine must never be asked
//! to reach this machine's own loopback services, which are exactly what the
//! address policy refuses.
//!
//! A value this tool cannot route abandons its source, not the chain: a
//! garbage environment value leaves the environment's other fallbacks
//! untouched (the user named that scheme explicitly) but the system proxy —
//! a source the user also configured — still answers.

use url::Url;

use super::system::SystemProxy;

/// Loopback entries merged into every bypass list. `::1` and `[::1]` are both
/// listed because a bare `::1` reads as host `:` port `1` to port-splitting
/// matchers.
const LOOPBACK_NO_PROXY: [&str; 4] = ["localhost", "127.0.0.1", "::1", "[::1]"];

/// Which transport mode a fetch runs under.
pub(super) enum Route {
    /// No proxy applies: the destination is resolved, validated, and pinned
    /// locally, and every hop of a redirect chain is re-validated.
    Direct,
    /// The proxy dials the origin, so there is no local address set to validate
    /// or pin. URL validation, the IP-literal check, the same-origin redirect
    /// policy, and every cap stay in force.
    Proxied(Url),
}

/// What one environment variable supplied.
enum Candidate {
    Accepted(String),
    Rejected,
    Absent,
}

/// The environment's proxy slots plus the flags marking a named-but-unroutable
/// value, which abandons the environment source for that scheme.
struct EnvPolicy {
    http: Option<String>,
    http_blocked: bool,
    https: Option<String>,
    https_blocked: bool,
    all: Option<String>,
}

impl EnvPolicy {
    fn http_chain(&self) -> Option<String> {
        if self.http_blocked {
            return None;
        }
        self.http.clone().or_else(|| self.all.clone())
    }

    fn https_chain(&self) -> Option<String> {
        if self.https_blocked {
            return None;
        }
        self.https
            .clone()
            .or_else(|| self.all.clone())
            .or_else(|| self.http.clone())
    }
}

/// The environment names a route decision reads, lowercase first.
pub(super) trait EnvLookup {
    fn get(&self, name: &str) -> Option<String>;
}

struct ProcessEnv;

impl EnvLookup for ProcessEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// What the OS-level lookup supplies; `None` where the platform publishes no
/// usable static proxy (none configured, PAC-only, or the reader failed).
pub(super) trait SystemLookup {
    fn lookup(&self) -> Option<SystemProxy>;
}

struct ProcessSystem;

impl SystemLookup for ProcessSystem {
    fn lookup(&self) -> Option<SystemProxy> {
        super::system::read()
    }
}

/// Decide the route for one URL under this process's environment and the
/// system proxy.
pub(super) fn route_for(url: &Url) -> Route {
    resolve_route(url, &ProcessEnv, &ProcessSystem)
}

fn resolve_route(url: &Url, env: &dyn EnvLookup, system: &dyn SystemLookup) -> Route {
    let env_policy = resolve_env_policy(env);
    let system = system.lookup();
    let Some(proxy) = select_proxy(url.scheme(), &env_policy, system.as_ref()) else {
        return Route::Direct;
    };
    let host = url.host_str().unwrap_or_default();
    let (bypass, exclude_simple_hostnames) = bypass_list(env, system.as_ref());
    if is_loopback_host(host) || bypasses_proxy(&bypass, exclude_simple_hostnames, url) {
        return Route::Direct;
    }
    match Url::parse(&proxy) {
        Ok(proxy) => Route::Proxied(proxy),
        Err(_) => Route::Direct,
    }
}

fn resolve_env_policy(env: &dyn EnvLookup) -> EnvPolicy {
    let all = match candidate(env, "all_proxy") {
        Candidate::Accepted(value) => Some(value),
        Candidate::Rejected | Candidate::Absent => None,
    };
    let (http, http_blocked) = match candidate(env, "http_proxy") {
        Candidate::Accepted(value) => (Some(value), false),
        Candidate::Rejected => (None, true),
        Candidate::Absent => (None, false),
    };
    let (https, https_blocked) = match candidate(env, "https_proxy") {
        Candidate::Accepted(value) => (Some(value), false),
        Candidate::Rejected => (None, true),
        Candidate::Absent => (None, false),
    };
    EnvPolicy {
        http,
        http_blocked,
        https,
        https_blocked,
        all,
    }
}

/// The proxy the decision lands on: the environment chain first, then the
/// system proxy's scheme-specific listener, the other web scheme's listener
/// (an HTTPS fetch through the HTTP proxy matches the environment's own
/// convention), and finally the system SOCKS listener. The first candidate
/// that parses as a proxy URL wins.
fn select_proxy(scheme: &str, env: &EnvPolicy, system: Option<&SystemProxy>) -> Option<String> {
    let mut candidates: Vec<String> = Vec::new();
    match scheme {
        "http" => candidates.extend(env.http_chain()),
        "https" => candidates.extend(env.https_chain()),
        _ => return None,
    }
    if let Some(system) = system {
        let order = match scheme {
            "http" => [system.http.clone(), system.socks.clone()],
            _ => [system.https.clone(), system.http.clone()],
        };
        candidates.extend(order.into_iter().flatten());
        if scheme == "https" {
            candidates.extend(system.socks.clone());
        }
    }
    candidates
        .into_iter()
        .find(|candidate| Url::parse(candidate).is_ok())
}

/// Classify one proxy candidate. Only URL-parseable `http:`/`https:`/`socks5:`
/// / `socks5h:` proxy values can be routed; a SOCKS4 or PAC value is rejected
/// rather than silently ignored.
fn candidate(env: &dyn EnvLookup, lower: &str) -> Candidate {
    match read_env(env, lower) {
        None => Candidate::Absent,
        Some(value) => match Url::parse(&value) {
            Ok(parsed) if matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h") => {
                Candidate::Accepted(value)
            }
            _ => Candidate::Rejected,
        },
    }
}

/// Read one variable in the lowercase-then-uppercase order, treating a blank
/// value as unset.
fn read_env(env: &dyn EnvLookup, lower: &str) -> Option<String> {
    for name in [lower.to_string(), lower.to_ascii_uppercase()] {
        if let Some(value) = env.get(&name) {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// The union of every source's bypass entries, plus the simple-hostname flag.
/// The loopback constants join any list that is not the universal wildcard.
fn bypass_list(env: &dyn EnvLookup, system: Option<&SystemProxy>) -> (Vec<String>, bool) {
    let mut entries = split_entries(&read_env(env, "no_proxy").unwrap_or_default());
    let mut exclude_simple_hostnames = false;
    if let Some(system) = system {
        entries.extend(system.bypass.iter().cloned());
        exclude_simple_hostnames |= system.exclude_simple_hostnames;
    }
    if !entries.iter().any(|entry| entry == "*") {
        for entry in LOOPBACK_NO_PROXY {
            if !entries
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(entry))
            {
                entries.push(entry.to_string());
            }
        }
    }
    (entries, exclude_simple_hostnames)
}

fn split_entries(no_proxy: &str) -> Vec<String> {
    no_proxy
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// Whether the bypass list exempts one URL. An entry may be an exact host, a
/// suffix (`example.com`, `.example.com`, `*.example.com`), a prefix wildcard
/// (`127.*`), or a CIDR range (`192.168.0.0/16`, matched against literal
/// destination addresses only — a name has no resolution to decide with here).
/// An entry may carry a `:port` to narrow it, and `*` bypasses everything.
fn bypasses_proxy(entries: &[String], exclude_simple_hostnames: bool, url: &Url) -> bool {
    let host = normalize_host(url.host_str().unwrap_or_default());
    if exclude_simple_hostnames && !host.contains('.') && !host.contains(':') {
        return true;
    }
    let port = url.port_or_known_default();
    for raw in entries {
        let entry = raw.to_ascii_lowercase();
        if entry == "*" {
            return true;
        }
        if let Some((network, prefix)) = cidr_parts(&entry)
            && cidr_contains(network, prefix, &host)
        {
            return true;
        }
        let (entry_host, entry_port) = split_host_port(&entry);
        if let Some(entry_port) = entry_port
            && entry_port.parse::<u16>().ok() != port
        {
            continue;
        }
        if prefix_wildcard_matches(entry_host, &host) {
            return true;
        }
        let candidate = entry_host
            .trim_start_matches("*.")
            .trim_start_matches('.')
            .trim_end_matches('.');
        if candidate.is_empty() {
            continue;
        }
        if host == candidate || host.ends_with(&format!(".{candidate}")) {
            return true;
        }
    }
    false
}

/// Split a CIDR entry into its network address and prefix length; `None` when
/// the entry is not CIDR notation over an IP address.
fn cidr_parts(entry: &str) -> Option<(&str, u8)> {
    let (network, prefix) = entry.rsplit_once('/')?;
    let prefix: u8 = prefix.parse().ok()?;
    network
        .parse::<std::net::IpAddr>()
        .is_ok()
        .then_some((network, prefix))
}

/// Whether a literal destination address sits inside the CIDR network.
fn cidr_contains(network: &str, prefix: u8, host: &str) -> bool {
    use std::net::IpAddr;
    let Ok(network) = network.parse::<IpAddr>() else {
        return false;
    };
    let Ok(host) = host.parse::<IpAddr>() else {
        return false;
    };
    match (network, host) {
        (IpAddr::V4(network), IpAddr::V4(host)) => {
            let prefix = prefix.min(32);
            let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            u32::from(network) & mask == u32::from(host) & mask
        }
        (IpAddr::V6(network), IpAddr::V6(host)) => {
            let prefix = prefix.min(128);
            let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            u128::from(network) & mask == u128::from(host) & mask
        }
        _ => false,
    }
}

/// The `prefix.*` form (Windows `ProxyOverride`): the literal prefix and
/// every name under it.
fn prefix_wildcard_matches(entry_host: &str, host: &str) -> bool {
    let Some(prefix) = entry_host.trim().strip_suffix(".*") else {
        return false;
    };
    let prefix = prefix.trim_end_matches('.');
    if prefix.is_empty() {
        return true;
    }
    host == prefix || host.starts_with(&format!("{prefix}."))
}

/// Split one bypass entry into host and optional port. Only a single-colon
/// entry splits, so a bare IPv6 literal keeps its colons.
fn split_host_port(entry: &str) -> (&str, Option<&str>) {
    if entry.starts_with('[')
        && let Some(close) = entry.find(']')
    {
        let host = &entry[1..close];
        let rest = &entry[close + 1..];
        return match rest.strip_prefix(':') {
            Some(port) => (host, Some(port)),
            None => (host, None),
        };
    }
    match entry.split_once(':') {
        Some((host, port)) if !port.contains(':') => (host, Some(port)),
        _ => (entry, None),
    }
}

fn normalize_host(host: &str) -> String {
    host.trim_matches(|c| c == '[' || c == ']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Whether a host names this machine. The bypass list carries four literal
/// loopback entries, so these checks cover the rest of `127.0.0.0/8`, the
/// unspecified address, and the IPv4-mapped spellings of both.
fn is_loopback_host(hostname: &str) -> bool {
    let host = normalize_host(hostname);
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if host == "::1" || host == "::" || host == "0.0.0.0" {
        return true;
    }
    let Some(mapped) = host.strip_prefix("::ffff:") else {
        return is_loopback_ipv4(&host);
    };
    if let Some(high) = mapped.split(':').next()
        && !mapped.contains('.')
        && let Ok(high) = u16::from_str_radix(high, 16)
    {
        return high >> 8 == 127;
    }
    is_loopback_ipv4(mapped)
}

fn is_loopback_ipv4(host: &str) -> bool {
    let octets: Vec<&str> = host.split('.').collect();
    octets.len() == 4
        && octets[0] == "127"
        && octets.iter().all(|octet| octet.parse::<u8>().is_ok())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::super::system::SystemProxy;
    use super::*;

    struct MapEnv(HashMap<String, String>);

    impl MapEnv {
        fn new(entries: &[(&str, &str)]) -> Self {
            Self(
                entries
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .collect(),
            )
        }
    }

    impl EnvLookup for MapEnv {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    #[derive(Default)]
    struct MapSystem(Option<SystemProxy>);

    impl SystemLookup for MapSystem {
        fn lookup(&self) -> Option<SystemProxy> {
            self.0.clone()
        }
    }

    fn system(http: Option<&str>, https: Option<&str>, socks: Option<&str>) -> MapSystem {
        MapSystem(Some(SystemProxy {
            http: http.map(str::to_string),
            https: https.map(str::to_string),
            socks: socks.map(str::to_string),
            ..SystemProxy::default()
        }))
    }

    fn url(input: &str) -> Url {
        Url::parse(input).expect("test URL parses")
    }

    fn proxied_with(env: &MapEnv, system: &MapSystem, input: &str) -> Option<String> {
        match resolve_route(&url(input), env, system) {
            Route::Proxied(proxy) => Some(proxy.to_string()),
            Route::Direct => None,
        }
    }

    fn proxied(env: &MapEnv, input: &str) -> Option<String> {
        proxied_with(env, &MapSystem::default(), input)
    }

    #[test]
    fn scheme_specific_proxies_and_all_proxy_fallback() {
        let env = MapEnv::new(&[
            ("http_proxy", "http://http-proxy.test:8080"),
            ("https_proxy", "http://https-proxy.test:8443"),
        ]);
        assert_eq!(
            proxied(&env, "http://example.com/").as_deref(),
            Some("http://http-proxy.test:8080/")
        );
        assert_eq!(
            proxied(&env, "https://example.com/").as_deref(),
            Some("http://https-proxy.test:8443/")
        );

        let all = MapEnv::new(&[("all_proxy", "http://all.test:1080")]);
        assert_eq!(
            proxied(&all, "http://example.com/").as_deref(),
            Some("http://all.test:1080/")
        );
        assert_eq!(
            proxied(&all, "https://example.com/").as_deref(),
            Some("http://all.test:1080/")
        );

        // HTTPS falls back to the HTTP proxy last, matching the reference.
        let http_only = MapEnv::new(&[("http_proxy", "http://http-proxy.test:8080")]);
        assert_eq!(
            proxied(&http_only, "https://example.com/").as_deref(),
            Some("http://http-proxy.test:8080/")
        );
    }

    #[test]
    fn lowercase_name_wins_and_blank_is_unset() {
        let env = MapEnv::new(&[
            ("http_proxy", "http://lower.test:1"),
            ("HTTP_PROXY", "http://upper.test:2"),
        ]);
        assert_eq!(
            proxied(&env, "http://example.com/").as_deref(),
            Some("http://lower.test:1/")
        );

        let blank = MapEnv::new(&[("http_proxy", "   "), ("HTTP_PROXY", "http://upper.test:2")]);
        assert_eq!(
            proxied(&blank, "http://example.com/").as_deref(),
            Some("http://upper.test:2/")
        );

        let empty = MapEnv::new(&[("http_proxy", "")]);
        assert_eq!(proxied(&empty, "http://example.com/"), None);
    }

    #[test]
    fn socks_proxy_values_are_routable() {
        for value in ["socks5://127.0.0.1:6153", "socks5h://127.0.0.1:6153"] {
            let env = MapEnv::new(&[("all_proxy", value)]);
            assert_eq!(
                proxied(&env, "https://example.com/").as_deref(),
                Some(value),
                "{value} routes"
            );
        }
    }

    #[test]
    fn unroutable_value_abandons_the_environment_source() {
        let env = MapEnv::new(&[
            ("http_proxy", "socks4://127.0.0.1:1080"),
            ("all_proxy", "http://all.test:1080"),
        ]);
        // The HTTP slot was named but cannot be routed: no fallback to ALL_PROXY.
        assert_eq!(proxied(&env, "http://example.com/"), None);
        // HTTPS keeps its own fallback chain.
        assert_eq!(
            proxied(&env, "https://example.com/").as_deref(),
            Some("http://all.test:1080/")
        );

        let unparseable = MapEnv::new(&[("http_proxy", "not a url")]);
        assert_eq!(proxied(&unparseable, "http://example.com/"), None);
    }

    #[test]
    fn system_proxy_fills_absent_environment_slots() {
        let env = MapEnv::new(&[]);
        let system = system(Some("http://127.0.0.1:6152"), None, None);
        assert_eq!(
            proxied_with(&env, &system, "http://example.com/").as_deref(),
            Some("http://127.0.0.1:6152/")
        );
        // The https chain falls back to the system's http listener.
        assert_eq!(
            proxied_with(&env, &system, "https://example.com/").as_deref(),
            Some("http://127.0.0.1:6152/")
        );
    }

    #[test]
    fn environment_proxy_wins_over_system() {
        let env = MapEnv::new(&[("http_proxy", "http://env.test:8080")]);
        let system = system(Some("http://sys.test:6152"), None, None);
        assert_eq!(
            proxied_with(&env, &system, "http://example.com/").as_deref(),
            Some("http://env.test:8080/")
        );
    }

    #[test]
    fn https_system_chain_prefers_https_then_http_then_socks() {
        let env = MapEnv::new(&[]);
        let full = system(
            Some("http://sys-http.test:6152"),
            Some("http://sys-https.test:8443"),
            Some("socks5h://sys-socks.test:6153"),
        );
        assert_eq!(
            proxied_with(&env, &full, "https://example.com/").as_deref(),
            Some("http://sys-https.test:8443/")
        );
        let no_https = system(
            Some("http://sys-http.test:6152"),
            None,
            Some("socks5h://sys-socks.test:6153"),
        );
        assert_eq!(
            proxied_with(&env, &no_https, "https://example.com/").as_deref(),
            Some("http://sys-http.test:6152/")
        );
        let socks_only = system(None, None, Some("socks5h://sys-socks.test:6153"));
        assert_eq!(
            proxied_with(&env, &socks_only, "https://example.com/").as_deref(),
            Some("socks5h://sys-socks.test:6153")
        );
    }

    #[test]
    fn http_never_falls_back_to_system_https() {
        let env = MapEnv::new(&[]);
        let https_only = system(None, Some("http://sys-https.test:8443"), None);
        assert_eq!(
            proxied_with(&env, &https_only, "http://example.com/"),
            None,
            "the http scheme must not borrow a proxy it was not named for"
        );
    }

    #[test]
    fn rejected_environment_value_still_reaches_the_system_proxy() {
        let env = MapEnv::new(&[("http_proxy", "not a url")]);
        let system = system(Some("http://127.0.0.1:6152"), None, None);
        assert_eq!(
            proxied_with(&env, &system, "http://example.com/").as_deref(),
            Some("http://127.0.0.1:6152/")
        );
    }

    #[test]
    fn bypass_list_exempts_matching_hosts() {
        let env = MapEnv::new(&[
            ("http_proxy", "http://proxy.test:8080"),
            ("no_proxy", "example.com"),
        ]);
        assert_eq!(proxied(&env, "http://example.com/"), None);
        assert_eq!(
            proxied(&env, "http://api.example.com/"),
            None,
            "subdomains match"
        );
        assert_eq!(
            proxied(&env, "http://notexample.com/").as_deref(),
            Some("http://proxy.test:8080/")
        );
        assert_eq!(
            proxied(&env, "http://example.com.evil.test/").as_deref(),
            Some("http://proxy.test:8080/")
        );

        let wildcard = MapEnv::new(&[("http_proxy", "http://proxy.test:8080"), ("no_proxy", "*")]);
        assert_eq!(proxied(&wildcard, "http://anything.test/"), None);

        let dotted = MapEnv::new(&[
            ("http_proxy", "http://proxy.test:8080"),
            ("no_proxy", ".internal.test"),
        ]);
        assert_eq!(proxied(&dotted, "http://host.internal.test/"), None);
    }

    #[test]
    fn bypass_entry_port_is_respected() {
        let env = MapEnv::new(&[
            ("http_proxy", "http://proxy.test:8080"),
            ("no_proxy", "example.com:8080"),
        ]);
        assert_eq!(proxied(&env, "http://example.com:8080/"), None);
        assert_eq!(
            proxied(&env, "http://example.com:9090/").as_deref(),
            Some("http://proxy.test:8080/")
        );
        assert_eq!(
            proxied(&env, "http://example.com/").as_deref(),
            Some("http://proxy.test:8080/"),
            "port must match"
        );
    }

    #[test]
    fn cidr_bypass_matches_literal_addresses_only() {
        let env = MapEnv::new(&[
            ("http_proxy", "http://proxy.test:8080"),
            ("no_proxy", "192.168.0.0/16, fc00::/7"),
        ]);
        assert_eq!(proxied(&env, "http://192.168.1.5/"), None);
        assert_eq!(proxied(&env, "http://[fc00::1]/"), None);
        assert_eq!(
            proxied(&env, "http://192.169.0.1/").as_deref(),
            Some("http://proxy.test:8080/")
        );
        // A name cannot be matched against a CIDR without resolving it.
        assert_eq!(
            proxied(&env, "http://intranet.test/").as_deref(),
            Some("http://proxy.test:8080/")
        );
    }

    #[test]
    fn prefix_wildcard_bypass_matches_the_prefix_tree() {
        let env = MapEnv::new(&[
            ("http_proxy", "http://proxy.test:8080"),
            ("no_proxy", "127.*, 192.168.*"),
        ]);
        assert_eq!(proxied(&env, "http://127.0.0.1/"), None);
        assert_eq!(proxied(&env, "http://192.168.1.20/"), None);
        assert_eq!(
            proxied(&env, "http://192.169.1.20/").as_deref(),
            Some("http://proxy.test:8080/")
        );
    }

    #[test]
    fn simple_hostname_flag_bypasses_bare_names() {
        let env = MapEnv::new(&[("http_proxy", "http://proxy.test:8080")]);
        let system = MapSystem(Some(SystemProxy {
            http: Some("http://proxy.test:8080".to_string()),
            exclude_simple_hostnames: true,
            ..SystemProxy::default()
        }));
        assert_eq!(proxied_with(&env, &system, "http://intranet/"), None);
        assert_eq!(
            proxied_with(&env, &system, "http://intranet.corp/").as_deref(),
            Some("http://proxy.test:8080/")
        );
        // Loopback literals carry no dot either; the proxy never dials them.
        assert_eq!(proxied_with(&env, &system, "http://[::1]/"), None);
    }

    #[test]
    fn loopback_never_goes_through_a_proxy() {
        let env = MapEnv::new(&[("http_proxy", "http://proxy.test:8080")]);
        for host in [
            "http://localhost/",
            "http://service.localhost/",
            "http://127.0.0.1/",
            "http://127.9.9.9/",
            "http://0.0.0.0/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[::ffff:7f00:1]/",
        ] {
            assert_eq!(proxied(&env, host), None, "{host} must be direct");
        }
        assert_eq!(
            proxied(&env, "http://[2606:4700::1111]/").as_deref(),
            Some("http://proxy.test:8080/"),
            "a public IPv6 host is still proxied"
        );
    }

    #[test]
    fn loopback_entries_are_merged_into_every_bypass_list() {
        let env = MapEnv::new(&[("no_proxy", "example.com")]);
        let (entries, _) = bypass_list(&env, None);
        assert!(entries.contains(&"example.com".to_string()));
        for entry in LOOPBACK_NO_PROXY {
            assert!(entries.contains(&entry.to_string()), "{entry} missing");
        }

        let wildcard = MapEnv::new(&[("no_proxy", "*")]);
        let (entries, _) = bypass_list(&wildcard, None);
        assert_eq!(entries, vec!["*"]);

        let system = system(Some("http://sys.test:1"), None, None);
        let (merged, _) = bypass_list(&env, system.0.as_ref());
        assert!(merged.contains(&"example.com".to_string()));
        assert_eq!(
            split_entries("a.test,,  b.test  c.test"),
            vec!["a.test", "b.test", "c.test"]
        );
    }

    #[test]
    fn non_http_schemes_are_direct() {
        let env = MapEnv::new(&[("all_proxy", "http://all.test:1080")]);
        assert!(matches!(
            resolve_route(&url("ftp://example.com/"), &env, &MapSystem::default()),
            Route::Direct
        ));
    }
}
