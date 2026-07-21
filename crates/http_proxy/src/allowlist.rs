//! Hostname allowlist for the proxy's policy decisions.
//!
//! Patterns are either exact hostnames (`github.com`) or leading-`*.`
//! subdomain wildcards (`*.example.com` matches `a.example.com` and
//! `a.b.example.com`, but **not** `example.com` itself). No middle wildcards,
//! no regex, no port matching.
//!
//! Hostnames are matched case-insensitively (DNS hostnames are case-insensitive
//! per RFC 1035 §2.3.3) with any trailing dot stripped. Internationalized
//! domain names supplied as UTF-8 are auto-converted to A-label (Punycode)
//! form so callers don't have to think about it.
//!
//! IP literals (IPv4 / IPv6 / `localhost`) are rejected when constructing a
//! `HostPattern` — hostname-based allowlisting cannot meaningfully apply to
//! them, and accepting them would be a policy footgun.

use std::fmt;
use std::net::IpAddr;
use thiserror::Error;

/// A hostname pattern accepted by the allowlist.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum HostPattern {
    /// Matches exactly one hostname (case-insensitive, trailing-dot tolerant).
    Exact(String),
    /// Matches subdomains of the contained hostname. `*.example.com` matches
    /// `a.example.com` and `a.b.example.com`, but not `example.com` itself.
    Subdomain(String),
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HostPattern::Exact(host) => write!(f, "{host}"),
            HostPattern::Subdomain(parent) => write!(f, "*.{parent}"),
        }
    }
}

/// Errors returned when parsing a host pattern from a user-provided string.
#[derive(Debug, Error)]
pub enum HostPatternError {
    /// The pattern was empty after trimming.
    #[error("host pattern cannot be empty")]
    Empty,
    /// The pattern contained an IP literal (IPv4, IPv6) or a local hostname
    /// (`localhost`, `*.localhost`). Hostname-based allowlisting cannot
    /// meaningfully apply to the local machine.
    #[error(
        "host pattern '{0}' is an IP literal or local hostname; only remote hostnames are allowed"
    )]
    IpLiteral(String),
    /// The pattern contained a wildcard somewhere other than as a leading
    /// label. We support `*.foo.com` but not `foo.*.com`, `*.com`, etc.
    #[error("host pattern '{0}' has a wildcard somewhere other than the leading label")]
    InvalidWildcard(String),
    /// The pattern was syntactically invalid (e.g., empty labels, contained
    /// characters that no IDN-form hostname could contain).
    #[error("host pattern '{pattern}' is not a valid hostname: {reason}")]
    Invalid { pattern: String, reason: String },
}

impl HostPattern {
    /// Parse a host pattern from a user-provided string.
    ///
    /// Accepts ASCII hostnames or UTF-8 IDN forms (auto-converted to Punycode),
    /// case-insensitively. A leading `*.` indicates a subdomain wildcard.
    /// Trailing dots are stripped.
    ///
    /// Rejects IP literals, middle/trailing wildcards, empty labels, and
    /// hostnames `idna` declines to convert.
    pub fn parse(input: &str) -> Result<Self, HostPatternError> {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(HostPatternError::Empty);
        }

        let (is_subdomain, rest) = if let Some(rest) = trimmed.strip_prefix("*.") {
            (true, rest)
        } else {
            (false, trimmed)
        };

        if rest.contains('*') {
            return Err(HostPatternError::InvalidWildcard(input.to_string()));
        }

        let without_trailing_dot = rest.strip_suffix('.').unwrap_or(rest);
        if without_trailing_dot.is_empty() {
            return Err(HostPatternError::Empty);
        }

        let ascii =
            idna::domain_to_ascii(without_trailing_dot).map_err(|e| HostPatternError::Invalid {
                pattern: input.to_string(),
                reason: format!("idna conversion failed: {e}"),
            })?;

        if ascii.is_empty() {
            return Err(HostPatternError::Empty);
        }

        let ip_check_input = ascii
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(&ascii);
        if ip_check_input.parse::<IpAddr>().is_ok() {
            return Err(HostPatternError::IpLiteral(input.to_string()));
        }

        if ascii.starts_with('.') || ascii.ends_with('.') || ascii.contains("..") {
            return Err(HostPatternError::Invalid {
                pattern: input.to_string(),
                reason: "empty label".to_string(),
            });
        }

        let canonical = ascii.to_ascii_lowercase();

        if canonical == "localhost" || canonical.ends_with(".localhost") {
            return Err(HostPatternError::IpLiteral(input.to_string()));
        }

        Ok(if is_subdomain {
            HostPattern::Subdomain(canonical)
        } else {
            HostPattern::Exact(canonical)
        })
    }

    /// Returns true if this pattern matches the given hostname. The hostname
    /// should already be in ASCII (Punycode) form — that's how it arrives in
    /// `CONNECT`/`Host:` lines on the wire.
    pub fn matches(&self, host: &str) -> bool {
        let canonical = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
        match self {
            HostPattern::Exact(pattern) => canonical == *pattern,
            HostPattern::Subdomain(parent) => canonical
                .strip_suffix(parent.as_str())
                .map(|prefix| prefix.ends_with('.') && !prefix.is_empty())
                .unwrap_or(false),
        }
    }

    /// Returns true if granting this pattern subsumes granting `other` — i.e.
    /// every host `other` would permit is also permitted by `self`.
    ///
    /// Used to decide whether an already-granted network permission covers a
    /// newly requested one (so the user isn't re-prompted).
    pub fn covers(&self, other: &HostPattern) -> bool {
        match (self, other) {
            (HostPattern::Exact(a), HostPattern::Exact(b)) => a == b,
            (HostPattern::Exact(_), HostPattern::Subdomain(_)) => false,
            (HostPattern::Subdomain(parent), HostPattern::Exact(host)) => {
                is_subdomain_of(host, parent)
            }
            (HostPattern::Subdomain(parent), HostPattern::Subdomain(child)) => {
                child == parent || is_subdomain_of(child, parent)
            }
        }
    }
}

/// Whether `host` is a strict subdomain of `parent`. Both are expected to be
/// in canonical form (lowercase, no trailing dot), as produced by
/// [`HostPattern::parse`]. `is_subdomain_of("a.example.com", "example.com")`
/// is true; `is_subdomain_of("example.com", "example.com")` is false.
fn is_subdomain_of(host: &str, parent: &str) -> bool {
    host.strip_suffix(parent)
        .and_then(|prefix| prefix.strip_suffix('.'))
        .is_some_and(|label| !label.is_empty())
}

/// Set of host patterns.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    patterns: Vec<HostPattern>,
    /// When true, every host is allowed regardless of `patterns`. The proxy
    /// still observes traffic and emits events; it just doesn't deny.
    allow_any: bool,
}

impl Allowlist {
    /// An allowlist that denies everything. Equivalent to `Allowlist::default()`.
    pub fn empty() -> Self {
        Self::default()
    }

    /// An allowlist that allows any host. When used with the proxy, traffic is
    /// still observed, but no host policy check is applied.
    pub fn any() -> Self {
        Self {
            patterns: Vec::new(),
            allow_any: true,
        }
    }

    /// Build from an iterator of patterns.
    pub fn from_patterns<I: IntoIterator<Item = HostPattern>>(patterns: I) -> Self {
        Self {
            patterns: patterns.into_iter().collect(),
            allow_any: false,
        }
    }

    /// Returns `true` if the host should be allowed through.
    pub fn allows(&self, host: &str) -> bool {
        if self.allow_any {
            return true;
        }
        self.patterns.iter().any(|p| p.matches(host))
    }

    /// Patterns in this allowlist (for diagnostics / system prompt rendering).
    pub fn patterns(&self) -> &[HostPattern] {
        &self.patterns
    }

    /// Whether this allowlist permits any host without a policy check.
    pub fn allows_any(&self) -> bool {
        self.allow_any
    }

    /// Whether this allowlist denies every host — i.e. it grants nothing,
    /// so no network plumbing is needed at all.
    pub fn is_deny_all(&self) -> bool {
        !self.allow_any && self.patterns.is_empty()
    }
}

impl FromIterator<HostPattern> for Allowlist {
    fn from_iter<I: IntoIterator<Item = HostPattern>>(iter: I) -> Self {
        Self::from_patterns(iter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exact_hostname() {
        let p = HostPattern::parse("github.com").unwrap();
        assert_eq!(p, HostPattern::Exact("github.com".into()));
    }

    #[test]
    fn parse_is_case_insensitive() {
        let p = HostPattern::parse("GitHub.COM").unwrap();
        assert_eq!(p, HostPattern::Exact("github.com".into()));
    }

    #[test]
    fn parse_strips_trailing_dot() {
        let p = HostPattern::parse("example.com.").unwrap();
        assert_eq!(p, HostPattern::Exact("example.com".into()));
    }

    #[test]
    fn parse_subdomain_wildcard() {
        let p = HostPattern::parse("*.example.com").unwrap();
        assert_eq!(p, HostPattern::Subdomain("example.com".into()));
    }

    #[test]
    fn parse_idn_to_punycode() {
        let p = HostPattern::parse("münchen.de").unwrap();
        assert_eq!(p, HostPattern::Exact("xn--mnchen-3ya.de".into()));
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(matches!(
            HostPattern::parse("").unwrap_err(),
            HostPatternError::Empty
        ));
        assert!(matches!(
            HostPattern::parse("   ").unwrap_err(),
            HostPatternError::Empty
        ));
        assert!(matches!(
            HostPattern::parse("*.").unwrap_err(),
            HostPatternError::Empty
        ));
    }

    #[test]
    fn parse_rejects_ip_literals() {
        for ip in ["1.2.3.4", "127.0.0.1", "0.0.0.0", "::1", "[::1]", "fe80::1"] {
            assert!(
                matches!(
                    HostPattern::parse(ip).unwrap_err(),
                    HostPatternError::IpLiteral(_)
                ),
                "expected IpLiteral for {ip}"
            );
        }
    }

    #[test]
    fn parse_rejects_localhost() {
        for pattern in [
            "localhost",
            "LOCALHOST",
            "localhost.",
            "foo.localhost",
            "*.localhost",
            "a.b.localhost",
        ] {
            assert!(
                matches!(
                    HostPattern::parse(pattern).unwrap_err(),
                    HostPatternError::IpLiteral(_)
                ),
                "expected IpLiteral for {pattern}"
            );
        }
    }

    #[test]
    fn parse_rejects_middle_wildcards() {
        for pat in ["foo.*.com", "*.foo.*", "*.*", "foo.*"] {
            assert!(
                matches!(
                    HostPattern::parse(pat).unwrap_err(),
                    HostPatternError::InvalidWildcard(_)
                ),
                "expected InvalidWildcard for {pat}"
            );
        }
    }

    #[test]
    fn parse_rejects_empty_labels() {
        let err = HostPattern::parse("foo..bar").unwrap_err();
        assert!(matches!(err, HostPatternError::Invalid { .. }));
    }

    #[test]
    fn matches_exact_is_case_insensitive() {
        let p = HostPattern::parse("github.com").unwrap();
        assert!(p.matches("github.com"));
        assert!(p.matches("GITHUB.COM"));
        assert!(p.matches("GitHub.com"));
        assert!(p.matches("github.com."));
        assert!(!p.matches("example.com"));
        assert!(!p.matches("a.github.com"));
    }

    #[test]
    fn matches_subdomain_wildcard() {
        let p = HostPattern::parse("*.example.com").unwrap();
        assert!(p.matches("a.example.com"));
        assert!(p.matches("a.b.example.com"));
        assert!(p.matches("A.B.EXAMPLE.COM"));
        assert!(!p.matches("example.com"));
        assert!(!p.matches("notexample.com"));
        assert!(!p.matches("malicious-example.com"));
    }

    #[test]
    fn matches_idn_via_punycode() {
        let p = HostPattern::parse("münchen.de").unwrap();
        assert!(p.matches("xn--mnchen-3ya.de"));
    }

    #[test]
    fn allowlist_allows_member() {
        let list = Allowlist::from_patterns(vec![
            HostPattern::parse("github.com").unwrap(),
            HostPattern::parse("*.npmjs.org").unwrap(),
        ]);
        assert!(list.allows("github.com"));
        assert!(list.allows("registry.npmjs.org"));
        assert!(!list.allows("example.com"));
        assert!(!list.allows("npmjs.org"));
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        let list = Allowlist::empty();
        assert!(!list.allows("anything.com"));
    }

    #[test]
    fn any_allowlist_allows_everything() {
        let list = Allowlist::any();
        assert!(list.allows("anything.com"));
        assert!(list.allows("evil.org"));
        assert!(list.allows_any());
    }

    #[test]
    fn display_round_trips_through_parse() {
        for input in ["github.com", "*.example.com", "xn--mnchen-3ya.de"] {
            let pattern = HostPattern::parse(input).unwrap();
            let rendered = pattern.to_string();
            assert_eq!(HostPattern::parse(&rendered).unwrap(), pattern);
        }
        assert_eq!(
            HostPattern::parse("GitHub.com").unwrap().to_string(),
            "github.com"
        );
        assert_eq!(
            HostPattern::parse("*.NPMJS.org").unwrap().to_string(),
            "*.npmjs.org"
        );
    }

    #[test]
    fn covers_exact_only_matches_identical() {
        let github = HostPattern::parse("github.com").unwrap();
        assert!(github.covers(&HostPattern::parse("github.com").unwrap()));
        assert!(!github.covers(&HostPattern::parse("api.github.com").unwrap()));
        assert!(!github.covers(&HostPattern::parse("*.github.com").unwrap()));
        assert!(!github.covers(&HostPattern::parse("example.com").unwrap()));
    }

    #[test]
    fn covers_subdomain_wildcard_subsumes_descendants() {
        let wildcard = HostPattern::parse("*.github.com").unwrap();
        assert!(wildcard.covers(&HostPattern::parse("api.github.com").unwrap()));
        assert!(wildcard.covers(&HostPattern::parse("a.b.github.com").unwrap()));
        assert!(wildcard.covers(&HostPattern::parse("*.api.github.com").unwrap()));
        assert!(wildcard.covers(&HostPattern::parse("*.github.com").unwrap()));
        assert!(!wildcard.covers(&HostPattern::parse("github.com").unwrap()));
        assert!(!wildcard.covers(&HostPattern::parse("notgithub.com").unwrap()));
        let narrow = HostPattern::parse("*.api.github.com").unwrap();
        assert!(!narrow.covers(&HostPattern::parse("*.github.com").unwrap()));
    }
}
