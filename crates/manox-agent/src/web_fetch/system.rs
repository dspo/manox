//! System-level proxy configuration for `web_fetch`: what the OS publishes
//! as its fetch proxy, read once per route decision (see [`super::proxy`]).
//!
//! Each platform adapter reduces its native store to one [`SystemProxy`]:
//! static per-scheme proxy URLs (already scheme-prefixed and routable), the
//! bypass list, and the simple-hostname flag. A PAC-only configuration is
//! deliberately reported as absent — script evaluation is out of scope, and
//! inventing a static proxy would route around the user's declared policy.
//!
//! The pure parsing helpers (Windows `ProxyServer` / `ProxyOverride` forms,
//! GNOME GVariant output) are compiled on every target so their tests run in
//! the single Linux CI leg.

/// The system proxy configuration the OS currently publishes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SystemProxy {
    /// Proxy for plain-HTTP fetches, as a routable proxy URL.
    pub(crate) http: Option<String>,
    /// Proxy for HTTPS fetches, as a routable proxy URL.
    pub(crate) https: Option<String>,
    /// SOCKS listener, routed with proxy-side name resolution (`socks5h`).
    pub(crate) socks: Option<String>,
    /// Raw bypass entries: exact hosts, `.suffix` / `*.suffix`, `prefix.*`
    /// wildcards, and CIDR ranges.
    pub(crate) bypass: Vec<String>,
    /// Bypass single-label hostnames (`<local>` / ExcludeSimpleHostnames).
    pub(crate) exclude_simple_hostnames: bool,
}

impl SystemProxy {
    /// Whether the configuration names at least one usable proxy.
    fn is_empty(&self) -> bool {
        self.http.is_none() && self.https.is_none() && self.socks.is_none()
    }
}

/// One `host`/`port` pair turned into a routable proxy URL. IPv6 literals
/// take the bracket form `Url::parse` needs.
#[cfg_attr(
    not(any(target_os = "macos", target_os = "windows", target_os = "linux")),
    allow(dead_code)
)]
fn proxy_url(scheme: &str, host: &str, port: u16) -> Option<String> {
    let host = host.trim();
    if host.is_empty() || port == 0 {
        return None;
    }
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    Some(format!("{scheme}://{host}:{port}"))
}

/// Read the OS proxy configuration, or `None` where the platform publishes
/// no usable static proxy.
pub(crate) fn read() -> Option<SystemProxy> {
    #[cfg(target_os = "macos")]
    {
        macos::read()
    }
    #[cfg(target_os = "windows")]
    {
        windows::read()
    }
    #[cfg(target_os = "linux")]
    {
        linux::read()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        None
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use core_foundation::array::{CFArray, CFArrayRef};
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::CFString;
    use system_configuration::dynamic_store::SCDynamicStoreBuilder;

    use super::{SystemProxy, proxy_url};

    pub(super) fn read() -> Option<SystemProxy> {
        let store = SCDynamicStoreBuilder::new("manox-web-fetch").build()?;
        let dict = store.get_proxies()?;
        from_proxies_dict(&dict)
    }

    /// Enable flags arrive as either a boolean or a 0/1 number depending on
    /// which SystemConfiguration surface wrote them.
    fn flag(value: &CFType) -> bool {
        if let Some(b) = value.downcast::<CFBoolean>() {
            return b == CFBoolean::true_value();
        }
        value
            .downcast::<CFNumber>()
            .and_then(|n| n.to_i64())
            .unwrap_or(0)
            != 0
    }

    fn listener(
        dict: &CFDictionary<CFString, CFType>,
        host_key: &str,
        port_key: &str,
        scheme: &str,
    ) -> Option<String> {
        let host = dict
            .find(CFString::new(host_key))
            .and_then(|value| value.downcast::<CFString>())
            .map(|s| s.to_string())?;
        let port = dict
            .find(CFString::new(port_key))
            .and_then(|value| value.downcast::<CFNumber>())
            .and_then(|n| n.to_i64())
            .and_then(|p| u16::try_from(p).ok())?;
        proxy_url(scheme, &host, port)
    }

    /// Reduces the merged proxy dictionary — the same one `scutil --proxy`
    /// prints — to the fetch-relevant fields.
    fn from_proxies_dict(dict: &CFDictionary<CFString, CFType>) -> Option<SystemProxy> {
        // PAC governs every destination; honoring static entries alongside
        // the script would route around the declared policy.
        if dict
            .find(CFString::new("ProxyAutoConfigEnable"))
            .map(|value| flag(&value))
            .unwrap_or(false)
        {
            return None;
        }
        let enabled = |key: &str| {
            dict.find(CFString::new(key))
                .map(|value| flag(&value))
                .unwrap_or(false)
        };
        let http = enabled("HTTPEnable")
            .then(|| listener(dict, "HTTPProxy", "HTTPPort", "http"))
            .flatten();
        let https = enabled("HTTPSEnable")
            .then(|| listener(dict, "HTTPSProxy", "HTTPSPort", "http"))
            .flatten();
        let socks = enabled("SOCKSEnable")
            .then(|| listener(dict, "SOCKSProxy", "SOCKSPort", "socks5h"))
            .flatten();
        let bypass =
            dict.find(CFString::new("ExceptionsList"))
                .map(|value| {
                    // SystemConfiguration stores this entry as an array of strings;
                    // the cast restores the typed view the dictionary erased.
                    let array = unsafe {
                        CFArray::<CFType>::wrap_under_get_rule(
                            value.as_concrete_TypeRef() as CFArrayRef
                        )
                    };
                    array
                        .iter()
                        .filter_map(|item| item.downcast::<CFString>().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
        let proxy = SystemProxy {
            http,
            https,
            socks,
            bypass,
            exclude_simple_hostnames: dict
                .find(CFString::new("ExcludeSimpleHostnames"))
                .map(|value| flag(&value))
                .unwrap_or(false),
        };
        if proxy.is_empty() { None } else { Some(proxy) }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn proxies_dict(pairs: Vec<(&str, CFType)>) -> CFDictionary<CFString, CFType> {
            let pairs: Vec<(CFString, CFType)> = pairs
                .into_iter()
                .map(|(key, value)| (CFString::new(key), value))
                .collect();
            CFDictionary::from_CFType_pairs(&pairs)
        }

        fn number(value: i64) -> CFType {
            CFNumber::from(value).as_CFType()
        }

        fn string(value: &str) -> CFType {
            CFString::new(value).as_CFType()
        }

        #[test]
        fn static_http_and_https_entries_are_reduced() {
            let dict = proxies_dict(vec![
                ("HTTPEnable", number(1)),
                ("HTTPProxy", string("127.0.0.1")),
                ("HTTPPort", number(6152)),
                ("HTTPSEnable", number(1)),
                ("HTTPSProxy", string("proxy.local")),
                ("HTTPSPort", number(8443)),
                (
                    "ExceptionsList",
                    CFArray::from_CFTypes(&[string("192.168.0.0/16"), string("*.local")])
                        .as_CFType(),
                ),
            ]);
            let proxy = from_proxies_dict(&dict).expect("static entries");
            assert_eq!(proxy.http.as_deref(), Some("http://127.0.0.1:6152"));
            assert_eq!(proxy.https.as_deref(), Some("http://proxy.local:8443"));
            assert_eq!(proxy.socks, None);
            assert_eq!(proxy.bypass, vec!["192.168.0.0/16", "*.local"]);
            assert!(!proxy.exclude_simple_hostnames);
        }

        #[test]
        fn enabled_socks_routes_with_proxy_side_resolution() {
            let dict = proxies_dict(vec![
                ("SOCKSEnable", CFBoolean::true_value().as_CFType()),
                ("SOCKSProxy", string("127.0.0.1")),
                ("SOCKSPort", number(6153)),
            ]);
            let proxy = from_proxies_dict(&dict).expect("socks entry");
            assert_eq!(proxy.socks.as_deref(), Some("socks5h://127.0.0.1:6153"));
            assert_eq!(proxy.http, None);
            assert_eq!(proxy.https, None);
        }

        #[test]
        fn pac_only_configuration_is_absent() {
            let dict = proxies_dict(vec![
                ("ProxyAutoConfigEnable", number(1)),
                (
                    "ProxyAutoConfigURLString",
                    string("http://pac.test/wpad.dat"),
                ),
            ]);
            assert_eq!(from_proxies_dict(&dict), None);
        }

        #[test]
        fn disabled_scheme_keys_are_absent_configuration() {
            let dict = proxies_dict(vec![
                ("HTTPEnable", number(0)),
                (
                    "ExcludeSimpleHostnames",
                    CFBoolean::true_value().as_CFType(),
                ),
            ]);
            assert_eq!(from_proxies_dict(&dict), None);
        }

        /// SystemConfiguration keeps stale listener keys next to a disabled
        /// scheme; only the enable flag decides.
        #[test]
        fn stale_keys_of_a_disabled_scheme_are_ignored() {
            let dict = proxies_dict(vec![
                ("HTTPEnable", number(0)),
                ("HTTPProxy", string("127.0.0.1")),
                ("HTTPPort", number(6152)),
            ]);
            assert_eq!(from_proxies_dict(&dict), None);
        }
    }
}

#[cfg(target_os = "windows")]
mod windows {
    use windows_registry::CURRENT_USER;

    use super::{SystemProxy, parse_windows_proxy_override, parse_windows_proxy_server};

    /// The WinINET settings browsers and system tooling read.
    const SETTINGS_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

    pub(super) fn read() -> Option<SystemProxy> {
        let key = CURRENT_USER.open(SETTINGS_KEY).ok()?;
        if key.get_u32("ProxyEnable").unwrap_or(0) == 0 {
            return None;
        }
        // A PAC overrides static entries; script evaluation is out of scope.
        if key.get_string("AutoConfigURL").is_ok() {
            return None;
        }
        let server = key.get_string("ProxyServer").ok()?;
        let (http, https, socks) = parse_windows_proxy_server(&server);
        let (bypass, exclude_simple_hostnames) = key
            .get_string("ProxyOverride")
            .map(|value| parse_windows_proxy_override(&value))
            .unwrap_or_default();
        let proxy = SystemProxy {
            http,
            https,
            socks,
            bypass,
            exclude_simple_hostnames,
        };
        if proxy.is_empty() { None } else { Some(proxy) }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::process::Command;

    use super::{
        SystemProxy, parse_gvariant_port, parse_gvariant_string, parse_gvariant_string_array,
        proxy_url,
    };

    /// One GSettings read; a missing `gsettings` or schema means the desktop
    /// publishes no proxy configuration.
    fn get(schema: &str, key: &str) -> Option<String> {
        let output = Command::new("gsettings")
            .args(["get", schema, key])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let text = text.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    }

    pub(super) fn read() -> Option<SystemProxy> {
        // Only the manual mode names static entries; 'auto' is PAC territory.
        if parse_gvariant_string(&get("org.gnome.system.proxy", "mode")?).as_deref()
            != Some("manual")
        {
            return None;
        }
        // Proxy authentication is not supported here; abstain rather than
        // produce confusing 407 responses.
        if get("org.gnome.system.proxy.http", "use-authentication").as_deref() == Some("true") {
            return None;
        }
        let host_port = |schema: &str| -> Option<String> {
            let host = parse_gvariant_string(&get(schema, "host")?)?;
            let port = parse_gvariant_port(&get(schema, "port")?)?;
            proxy_url("http", &host, port)
        };
        let http = host_port("org.gnome.system.proxy.http");
        // GLib semantics: an unset https entry inherits the http listener.
        let https = host_port("org.gnome.system.proxy.https").or_else(|| http.clone());
        let bypass = get("org.gnome.system.proxy", "ignore-hosts")
            .map(|raw| parse_gvariant_string_array(&raw))
            .unwrap_or_default();
        let proxy = SystemProxy {
            http,
            https,
            socks: None,
            bypass,
            exclude_simple_hostnames: false,
        };
        if proxy.is_empty() { None } else { Some(proxy) }
    }
}

/// Windows `ProxyServer` forms: a bare `host:port` serving both web schemes,
/// or a `;`-separated `protocol=host:port` map (`socks=` among them).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_windows_proxy_server(value: &str) -> (Option<String>, Option<String>, Option<String>) {
    if !value.contains('=') {
        let bare = windows_host_port_url("http", value);
        return (bare.clone(), bare, None);
    }
    let mut http = None;
    let mut https = None;
    let mut socks = None;
    for part in value.split(';') {
        let Some((proto, addr)) = part.split_once('=') else {
            continue;
        };
        let proto = proto.trim().to_ascii_lowercase();
        let scheme = match proto.as_str() {
            // https entries are CONNECT proxies, not TLS-wrapped ones.
            "http" | "https" => "http",
            "socks" => "socks5h",
            _ => continue,
        };
        let url = windows_host_port_url(scheme, addr);
        match proto.as_str() {
            "http" => http = url,
            "https" => https = url,
            _ => socks = url,
        }
    }
    (http, https, socks)
}

/// One Windows `host:port` entry as a proxy URL; `None` when unparseable.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn windows_host_port_url(scheme: &str, entry: &str) -> Option<String> {
    let entry = entry.trim();
    let (host, port) = entry.rsplit_once(':')?;
    let host = host.trim().trim_matches(|c| c == '[' || c == ']');
    let port: u16 = port.trim().parse().ok()?;
    proxy_url(scheme, host, port)
}

/// Windows `ProxyOverride` → bypass entries plus the `<local>` flag.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn parse_windows_proxy_override(value: &str) -> (Vec<String>, bool) {
    let mut entries = Vec::new();
    let mut exclude_simple_hostnames = false;
    for part in value.split(';') {
        let entry = part.trim();
        if entry.is_empty() {
            continue;
        }
        if entry.eq_ignore_ascii_case("<local>") {
            exclude_simple_hostnames = true;
        } else {
            entries.push(entry.to_string());
        }
    }
    (entries, exclude_simple_hostnames)
}

/// A GVariant-printed string (`'manual'`) without its quoting; a bare
/// unquoted token passes through. Empty values are absent.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_gvariant_string(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let value = raw
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(raw);
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// A GVariant string array: `@as []` or `['localhost', '127.0.0.0/8']`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_gvariant_string_array(raw: &str) -> Vec<String> {
    let inner = raw.trim().trim_start_matches("@as").trim();
    let inner = inner
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(inner);
    inner
        .split(',')
        .map(|item| item.trim().trim_matches('\''))
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// A GVariant-printed integer port (`8080`, tolerating the `uint32 8080`
/// spelling).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_gvariant_port(raw: &str) -> Option<u16> {
    let token = raw.split_whitespace().last()?;
    let port: u16 = token
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()?;
    // Zero is the schema's "unset" spelling.
    (port != 0).then_some(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_url_shapes_scheme_host_port() {
        assert_eq!(
            proxy_url("http", "127.0.0.1", 6152).as_deref(),
            Some("http://127.0.0.1:6152")
        );
        assert_eq!(
            proxy_url("socks5h", "::1", 6153).as_deref(),
            Some("socks5h://[::1]:6153")
        );
        assert_eq!(proxy_url("http", "proxy.local", 0), None);
        assert_eq!(proxy_url("http", "  ", 8080), None);
    }

    #[test]
    fn windows_bare_proxy_server_serves_both_web_schemes() {
        let (http, https, socks) = parse_windows_proxy_server("127.0.0.1:6152");
        assert_eq!(http.as_deref(), Some("http://127.0.0.1:6152"));
        assert_eq!(https.as_deref(), Some("http://127.0.0.1:6152"));
        assert_eq!(socks, None);
    }

    #[test]
    fn windows_proxy_server_map_is_split_per_protocol() {
        let (http, https, socks) = parse_windows_proxy_server(
            "http=10.0.0.1:8080;https=10.0.0.1:8443;socks=10.0.0.2:1080",
        );
        assert_eq!(http.as_deref(), Some("http://10.0.0.1:8080"));
        assert_eq!(https.as_deref(), Some("http://10.0.0.1:8443"));
        assert_eq!(socks.as_deref(), Some("socks5h://10.0.0.2:1080"));
    }

    #[test]
    fn windows_proxy_server_garbage_is_absent() {
        assert_eq!(parse_windows_proxy_server("not a host"), (None, None, None));
        let (http, _, _) = parse_windows_proxy_server("http=host:65536;socks=x");
        assert_eq!(http, None);
    }

    #[test]
    fn windows_proxy_override_splits_entries_and_local_flag() {
        let (entries, local) = parse_windows_proxy_override("localhost; 127.*; *.corp;<local>");
        assert_eq!(entries, vec!["localhost", "127.*", "*.corp"]);
        assert!(local);
        assert_eq!(parse_windows_proxy_override(""), (Vec::new(), false));
    }

    #[test]
    fn gvariant_strings_drop_quoting() {
        assert_eq!(parse_gvariant_string("'manual'").as_deref(), Some("manual"));
        assert_eq!(parse_gvariant_string("8080").as_deref(), Some("8080"));
        assert_eq!(parse_gvariant_string("''"), None);
    }

    #[test]
    fn gvariant_string_arrays_are_flattened() {
        assert_eq!(parse_gvariant_string_array("@as []"), Vec::<String>::new());
        assert_eq!(
            parse_gvariant_string_array("['localhost', '127.0.0.0/8', '::1']"),
            vec!["localhost", "127.0.0.0/8", "::1"]
        );
    }

    #[test]
    fn gvariant_ports_take_digits_only() {
        assert_eq!(parse_gvariant_port("8080"), Some(8080));
        assert_eq!(parse_gvariant_port("uint32 3128"), Some(3128));
        assert_eq!(parse_gvariant_port("0"), None);
    }
}
