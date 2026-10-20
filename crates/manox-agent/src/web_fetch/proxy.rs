//! Environment-proxy route decision for `web_fetch`: which of the two
//! transport modes a URL runs under, decided once per call.
//!
//! A proxied fetch cannot be address-validated locally — the proxy resolves the
//! origin — so the route decides both the resolver stack and whether the DNS
//! guard applies at all. Redirects stay same-origin, so one decision covers
//! every hop of a chain. The URL-hygiene rules, the IP-literal check, and every
//! cap apply in both modes.
//!
//! The rules follow the convention the rest of the ecosystem reads: a scheme's
//! own variable wins (lowercase spelling first), `ALL_PROXY` is the fallback and
//! HTTPS also falls back to the HTTP proxy, a value this tool cannot route
//! leaves that scheme direct rather than falling back to another scheme's proxy
//! the user never named for it, and the bypass list always carries the loopback
//! entries — a proxy on this machine must never be asked to reach this
//! machine's own loopback services, which are exactly what the address policy
//! refuses.

use url::Url;

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

/// What one environment variable supplied. A rejected candidate keeps its own
/// scheme direct: the user named a proxy there, so falling back to another
/// scheme's proxy would route the request somewhere they did not ask for.
enum Candidate {
    Accepted(String),
    Rejected,
    Absent,
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

/// One resolved proxy policy: per-scheme proxy URLs plus the effective bypass list.
struct ProxyPolicy {
    http: Option<String>,
    https: Option<String>,
    no_proxy: String,
}

/// Decide the route for one URL under this process's environment.
pub(super) fn route_for(url: &Url) -> Route {
    resolve_route(url, &ProcessEnv)
}

fn resolve_route(url: &Url, env: &dyn EnvLookup) -> Route {
    let policy = resolve_policy(env);
    let proxy = match url.scheme() {
        "http" => policy.http,
        "https" => policy.https,
        _ => None,
    };
    let Some(proxy) = proxy else {
        return Route::Direct;
    };
    let host = url.host_str().unwrap_or_default();
    if is_loopback_host(host) || bypasses_proxy(&policy.no_proxy, url) {
        return Route::Direct;
    }
    match Url::parse(&proxy) {
        Ok(proxy) => Route::Proxied(proxy),
        Err(_) => Route::Direct,
    }
}

fn resolve_policy(env: &dyn EnvLookup) -> ProxyPolicy {
    let all = candidate(env, "all_proxy");
    let all_value = match &all {
        Candidate::Accepted(value) => Some(value.clone()),
        Candidate::Rejected | Candidate::Absent => None,
    };
    let http = pick(candidate(env, "http_proxy"), [all_value.clone()]);
    let https = pick(candidate(env, "https_proxy"), [all_value, http.clone()]);
    ProxyPolicy {
        http,
        https,
        no_proxy: with_loopback(read_env(env, "no_proxy")),
    }
}

/// One scheme's proxy: its own variable, then the fallbacks — but only when the
/// scheme's own slot was absent, so a rejected value keeps that scheme direct.
fn pick(own: Candidate, fallbacks: impl IntoIterator<Item = Option<String>>) -> Option<String> {
    match own {
        Candidate::Accepted(value) => Some(value),
        Candidate::Rejected => None,
        Candidate::Absent => fallbacks.into_iter().flatten().next(),
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

/// Classify one proxy candidate. Only `http:`/`https:` proxy URLs can be
/// routed; a SOCKS or PAC value is rejected rather than silently ignored.
fn candidate(env: &dyn EnvLookup, lower: &str) -> Candidate {
    match read_env(env, lower) {
        None => Candidate::Absent,
        Some(value) => match Url::parse(&value) {
            Ok(parsed) if parsed.scheme() == "http" || parsed.scheme() == "https" => {
                Candidate::Accepted(value)
            }
            _ => Candidate::Rejected,
        },
    }
}

/// Merge the loopback entries into a bypass list, preserving the caller's order.
fn with_loopback(no_proxy: Option<String>) -> String {
    let mut entries: Vec<String> = split_entries(&no_proxy.unwrap_or_default());
    if entries.iter().any(|entry| entry == "*") {
        return "*".to_string();
    }
    let present: Vec<String> = entries
        .iter()
        .map(|entry| entry.to_ascii_lowercase())
        .collect();
    for entry in LOOPBACK_NO_PROXY {
        if !present.contains(&entry.to_string()) {
            entries.push(entry.to_string());
        }
    }
    entries.join(",")
}

fn split_entries(no_proxy: &str) -> Vec<String> {
    no_proxy
        .split(|c: char| c == ',' || c.is_whitespace())
        .map(|entry| entry.trim().to_string())
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// Whether the bypass list exempts one URL. An entry matches its host together
/// with every subdomain under it, may carry a `:port`, and `*` bypasses
/// everything. CIDR notation is not matched, matching the reference policy.
fn bypasses_proxy(no_proxy: &str, url: &Url) -> bool {
    let host = normalize_host(url.host_str().unwrap_or_default());
    let port = url.port_or_known_default();
    for raw in split_entries(no_proxy) {
        let entry = raw.to_ascii_lowercase();
        if entry == "*" {
            return true;
        }
        let (entry_host, entry_port) = split_host_port(&entry);
        if let Some(entry_port) = entry_port
            && entry_port.parse::<u16>().ok() != port
        {
            continue;
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

    fn url(input: &str) -> Url {
        Url::parse(input).expect("test URL parses")
    }

    fn proxied(env: &MapEnv, input: &str) -> Option<String> {
        match resolve_route(&url(input), env) {
            Route::Proxied(proxy) => Some(proxy.to_string()),
            Route::Direct => None,
        }
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
    fn unroutable_value_keeps_its_scheme_direct() {
        let env = MapEnv::new(&[
            ("http_proxy", "socks5://127.0.0.1:1080"),
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
        let merged = with_loopback(Some("example.com".to_string()));
        assert!(merged.contains("example.com"));
        for entry in LOOPBACK_NO_PROXY {
            assert!(merged.contains(entry), "{entry} missing from {merged}");
        }
        assert_eq!(with_loopback(Some("*".to_string())), "*");
        assert_eq!(
            split_entries("a.test,,  b.test  c.test"),
            vec!["a.test", "b.test", "c.test"]
        );
    }

    #[test]
    fn non_http_schemes_are_direct() {
        let env = MapEnv::new(&[("all_proxy", "http://all.test:1080")]);
        assert!(matches!(
            resolve_route(&url("ftp://example.com/"), &env),
            Route::Direct
        ));
    }
}
