//! The sandbox network policy: the domain allowlist, the in-process CONNECT
//! proxy, and the route one confined call runs under.
//!
//! Three states, driven by the session's `networkAccess` switch plus the
//! configured allowlist (`~/.manox/settings.toml` `[sandbox.network] allow`):
//!
//! - **off** — [`NetworkRoute::Disabled`]: no proxy, no allowance (the
//!   enforcing families close the network themselves).
//! - **on, allowlist empty** — [`NetworkRoute::Direct`]: the historical full
//!   open; no proxy runs.
//! - **on, allowlist non-empty** — [`NetworkRoute::Managed`]: a loopback
//!   HTTP CONNECT proxy starts for the session, and the trapped env carries
//!   the proxy variables. macOS reaches it through the seatbelt profile
//!   (only that port plus loopback allowed); Linux runs the sandbox in an
//!   isolated network namespace whose only egress is the listener the inner
//!   helper hands back and this module's proxy serves behind the host
//!   bridge (`sandbox::bridge`). A client that ignores the proxy env finds
//!   no non-loopback path either way and fails closed at the OS layer.
//!
//! The proxy judges each CONNECT target against the allowlist — exact
//! hosts, `*.example.com` (any subdomain), `**.example.com` (apex plus
//! subdomains), and IP literals. A hostname that resolves to a private,
//! loopback, or link-local address is refused even when allowlisted (an
//! allowlist entry is not an SSRF grant into the host's own services); an
//! explicitly allowlisted IP literal is honored. On a platform without a
//! managed transport (`sandbox::managed_transport_ready`), an allowlist
//! fails closed to `Disabled` — never an open route against the configured
//! intent.
//!
//! The policy is per-session state hanging off the approval gate; dropping
//! the gate aborts the proxy task and closes the listener.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::NetworkRoute;
use manox_harness::sandbox::NetworkAccess;

/// The pattern verdicts a CONNECT target can receive. The private-address
/// guard is not a verdict here: it runs at tunnel time, where a name's
/// actual resolution is known (an explicit IP-literal entry is honored as
/// granted — the user named the address — while a name resolving into the
/// host's own space is refused regardless of its pattern).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HostVerdict {
    /// A pattern (or explicit literal) admits the target.
    Allow,
    /// No allowlist pattern matches.
    NotAllowlisted,
}

/// One compiled allowlist: the configured patterns (settings) plus the
/// session's approved amendments. Cheap to clone — the judge context hands
/// out snapshots so a mid-session amendment swaps the set atomically.
#[derive(Debug, Clone, Default)]
pub(crate) struct DomainAllowlist {
    patterns: Vec<Pattern>,
}

/// One compiled pattern. `**.` expands to `apex OR any-subdomain`; `*.`
/// expands to any-subdomain only; a bare pattern matches exactly. The
/// global `*` is rejected at construction — an allow-everything entry is
/// what the direct route is for, not a look-alike.
#[derive(Debug, Clone)]
enum Pattern {
    Exact(String),
    Subdomains(String),
    ApexAndSubdomains(String),
    Ip(IpAddr),
}

impl DomainAllowlist {
    /// Compile `patterns` (settings + session amendments). Invalid entries
    /// (empty, the global `*`) are skipped rather than failing the session —
    /// a hand-edited settings.toml must not take the proxy down; the
    /// remaining patterns still bound the tunnel surface.
    pub(crate) fn compile(patterns: &[String]) -> Self {
        let mut compiled = Vec::new();
        for raw in patterns {
            let trimmed = raw.trim();
            let lowered = normalize_host(trimmed);
            if lowered.is_empty() || lowered == "*" {
                continue;
            }
            if let Some(rest) = lowered
                .strip_prefix("**.")
                .or_else(|| lowered.strip_prefix("*."))
            {
                if rest.is_empty() || rest.contains('*') {
                    continue;
                }
                if lowered.starts_with("**.") {
                    compiled.push(Pattern::ApexAndSubdomains(rest.to_string()));
                } else {
                    compiled.push(Pattern::Subdomains(rest.to_string()));
                }
                continue;
            }
            if lowered.contains('*') {
                continue;
            }
            if let Ok(ip) = lowered.parse::<IpAddr>() {
                compiled.push(Pattern::Ip(ip));
            } else {
                compiled.push(Pattern::Exact(lowered));
            }
        }
        Self { patterns: compiled }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Judge a CONNECT target host (already port-stripped, case-insensitive).
    /// IP literals judge directly; names match by pattern — the private-
    /// address guard runs at TUNNEL time (resolution happens on the way to
    /// the upstream, where the resolved IP is checked), so the pattern match
    /// alone answers here.
    pub(crate) fn judge(&self, host: &str) -> HostVerdict {
        let host = normalize_host(host);
        if host.is_empty() {
            return HostVerdict::NotAllowlisted;
        }
        // An IP-literal target: an explicit literal entry admits it (the
        // user named this address), anything else is a miss. The private
        // ranges still refuse below at tunnel time unless the literal
        // itself was allowlisted.
        if let Ok(ip) = host.parse::<IpAddr>() {
            return if self
                .patterns
                .iter()
                .any(|p| matches!(p, Pattern::Ip(allowed) if *allowed == ip))
            {
                HostVerdict::Allow
            } else {
                HostVerdict::NotAllowlisted
            };
        }
        let matched = self.patterns.iter().any(|pattern| match pattern {
            Pattern::Exact(exact) => host == *exact,
            Pattern::Subdomains(domain) | Pattern::ApexAndSubdomains(domain) => {
                host.ends_with(domain)
                    && host.len() > domain.len()
                    && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
            }
            Pattern::Ip(_) => false,
        });
        let apex_matched = self.patterns.iter().any(|pattern| match pattern {
            Pattern::ApexAndSubdomains(domain) => host == *domain,
            _ => false,
        });
        if matched || apex_matched {
            HostVerdict::Allow
        } else {
            HostVerdict::NotAllowlisted
        }
    }

    /// Whether `host` (a resolved IP or an allowlisted IP literal) falls
    /// into the host's own address space — loopback, private, link-local,
    /// or unspecified. Judged for every tunnel target after resolution.
    pub(crate) fn is_private_ip(ip: IpAddr) -> bool {
        match ip {
            IpAddr::V4(v4) => {
                v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || v4.is_broadcast()
                    // 100.64/10 — carrier-grade NAT, the shared address
                    // space in front of the host's own networks.
                    || v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64
            }
            IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_unicast_link_local()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
            }
        }
    }
}

/// Lowercase, trim, strip one bracketed form and a trailing port, drop the
/// trailing dot of fully-qualified names. IPv6 literals lose their brackets
/// so a `**.`-free exact entry `[::1]` and `::1` compile the same.
pub(crate) fn normalize_host(host: &str) -> String {
    let mut host = host.trim().to_ascii_lowercase();
    if host.starts_with('[')
        && let Some(end) = host.find(']')
    {
        let inner = host[1..end].to_string();
        return inner;
    }
    if host.matches(':').count() == 1
        && let Some((head, port)) = host.split_once(':')
        && port.chars().all(|c| c.is_ascii_digit())
        && !port.is_empty()
    {
        host = head.to_string();
    }
    host.trim_end_matches('.').to_string()
}

/// The per-session proxy process: the bound listener, its accept loop, and
/// the shutdown signal. Dropping the handle cancels the loop and closes the
/// listener (the session gate owns the policy; session death is proxy
/// death).
pub(crate) struct ProxyServer {
    pub(crate) port: u16,
    shutdown: CancellationToken,
    accept_loop: tokio::task::JoinHandle<()>,
}

impl Drop for ProxyServer {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.accept_loop.abort();
    }
}

/// The approval seam for allowlist misses (the amendment path). Sits behind
/// a `Weak`-free Arc installed by the engine after the gate exists; `None`
/// means every miss is refused outright.
#[async_trait::async_trait]
pub(crate) trait HostApprover: Send + Sync {
    /// Ask the user about one non-allowlisted host. The outcome vocabulary
    /// mirrors the confirmation card: this connection only, this session,
    /// this host forever (settings), or no.
    async fn request(&self, host: &str, port: u16) -> HostDecision;
}

/// The amendment outcomes one network approval can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostDecision {
    AllowOnce,
    AllowSession,
    AllowPersistent,
    Deny,
}

/// Shared judge context the per-connection tasks read: the live allowlist
/// snapshot, the approver, and the ask serializer (one pending network card
/// at a time — the client surfaces a single open confirmation, so two
/// concurrent misses must queue, not race).
pub(crate) struct JudgeCtx {
    allowlist: std::sync::RwLock<Arc<DomainAllowlist>>,
    approver: Mutex<Option<Arc<dyn HostApprover>>>,
    ask_serial: tokio::sync::Mutex<()>,
    session_allow: Mutex<Vec<String>>,
    settings_allow: Mutex<Vec<String>>,
}

impl JudgeCtx {
    fn new(settings_allow: Vec<String>) -> Self {
        Self {
            allowlist: std::sync::RwLock::new(Arc::new(DomainAllowlist::compile(&settings_allow))),
            approver: Mutex::new(None),
            ask_serial: tokio::sync::Mutex::new(()),
            session_allow: Mutex::new(Vec::new()),
            settings_allow: Mutex::new(settings_allow),
        }
    }

    fn allowlist(&self) -> Arc<DomainAllowlist> {
        self.allowlist.read().unwrap().clone()
    }

    /// Recompile from the current settings + session sets (a re-enable
    /// rereads the file so between-switch edits apply).
    fn recompile(&self, settings_allow: Vec<String>) {
        *self.settings_allow.lock().unwrap() = settings_allow;
        let combined: Vec<String> = self
            .settings_allow
            .lock()
            .unwrap()
            .iter()
            .chain(self.session_allow.lock().unwrap().iter())
            .cloned()
            .collect();
        *self.allowlist.write().unwrap() = Arc::new(DomainAllowlist::compile(&combined));
    }

    fn install_approver(&self, approver: Arc<dyn HostApprover>) {
        *self.approver.lock().unwrap() = Some(approver);
    }

    /// Admit `host` for the session (and, persistently, on disk). The
    /// in-memory sets gain the host immediately so the next tunnel skips
    /// the ask; the settings write rides the caller's persistence choice.
    fn admit(&self, host: &str) {
        let mut session = self.session_allow.lock().unwrap();
        let normalized = normalize_host(host);
        if session.iter().any(|h| normalize_host(h) == normalized) {
            return;
        }
        session.push(host.to_string());
        drop(session);
        let combined: Vec<String> = self
            .settings_allow
            .lock()
            .unwrap()
            .iter()
            .chain(self.session_allow.lock().unwrap().iter())
            .cloned()
            .collect();
        *self.allowlist.write().unwrap() = Arc::new(DomainAllowlist::compile(&combined));
    }
}

/// One CONNECT request's parsed target.
fn parse_connect_request(head: &[u8]) -> Option<(String, u16)> {
    let head = std::str::from_utf8(head).ok()?;
    let request_line = head.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?;
    if !method.eq_ignore_ascii_case("CONNECT") {
        return None;
    }
    let authority = parts.next()?;
    let (host, port) = authority.rsplit_once(':')?;
    let port = port.parse().ok()?;
    Some((normalize_host(host), port))
}

/// Serve one proxied connection: read the request head, judge, then either
/// relay the tunnel or answer with a refusal the client sees.
async fn serve_connection(mut client: tokio::net::TcpStream, ctx: Arc<JudgeCtx>) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    // Read to the end of the request headers (bounded: a malformed client
    // cannot pin memory).
    loop {
        match client.read(&mut byte).await {
            Ok(0) => return,
            Ok(_) => {
                head.push(byte[0]);
                if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") || head.len() > 8192 {
                    break;
                }
            }
            Err(_) => return,
        }
    }
    let Some((host, port)) = parse_connect_request(&head) else {
        let _ = client
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\n\r\n")
            .await;
        return;
    };
    let verdict = ctx.allowlist().judge(&host);
    let allowed = match verdict {
        HostVerdict::Allow => true,
        HostVerdict::NotAllowlisted => ask_about_host(&ctx, &host, port).await,
    };
    if !allowed {
        let _ = client
            .write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")
            .await;
        let _ = client.shutdown().await;
        return;
    }
    // Dial the upstream HERE, in the trusted host process. A name target
    // resolves first so the private-address guard sees the resolved IP
    // before any byte flows — a hostname that lands in loopback/private
    // space is refused even when its pattern matched (an allowlist entry is
    // not an SSRF grant into the host's own services). An IP-literal target
    // skips the guard: reaching it at all required an explicit literal
    // entry, which IS the user naming the address.
    let mut upstream = if let Ok(literal) = host.parse::<std::net::IpAddr>() {
        match tokio::net::TcpStream::connect((literal, port)).await {
            Ok(stream) => stream,
            Err(_) => {
                let _ = client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }
        }
    } else {
        let target = format!("{host}:{port}");
        let addrs = match tokio::net::lookup_host(&target).await {
            Ok(addrs) => addrs.collect::<Vec<_>>(),
            Err(_) => {
                let _ = client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }
        };
        let Some(upstream_addr) = addrs
            .iter()
            .find(|addr| !DomainAllowlist::is_private_ip(addr.ip()))
        else {
            let _ = client
                .write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")
                .await;
            let _ = client.shutdown().await;
            return;
        };
        match tokio::net::TcpStream::connect(upstream_addr).await {
            Ok(stream) => stream,
            Err(_) => {
                let _ = client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
                    .await;
                return;
            }
        }
    };
    if client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .is_err()
    {
        return;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    let _ = client.shutdown().await;
    let _ = upstream.shutdown().await;
}

/// The amendment ask for a non-allowlisted host, serialized so at most one
/// card is pending. Without an approver the miss is a bare refusal.
async fn ask_about_host(ctx: &Arc<JudgeCtx>, host: &str, port: u16) -> bool {
    let Some(approver) = ctx.approver.lock().unwrap().clone() else {
        return false;
    };
    let _guard = ctx.ask_serial.lock().await;
    // The allowlist may have moved while the ask queued (an earlier card
    // admitted this very host).
    if ctx.allowlist().judge(host) == HostVerdict::Allow {
        return true;
    }
    match approver.request(host, port).await {
        HostDecision::AllowOnce => true,
        HostDecision::AllowSession => {
            ctx.admit(host);
            true
        }
        HostDecision::AllowPersistent => {
            if let Err(error) = crate::settings::add_sandbox_network_allow(host) {
                tracing::warn!(%error, host, "failed to persist the network allowlist amendment");
            }
            ctx.admit(host);
            true
        }
        HostDecision::Deny => false,
    }
}

/// The per-session policy object: the running proxy and the judge context.
/// Cloned freely (Arc internals); the approval gate owns the instance. The
/// ACCESS truth lives in the gate's sync cell (the switch must be readable
/// and writable synchronously, and a route answer must never lag the
/// switch); this object owns the proxy lifecycle half.
#[derive(Clone)]
pub struct NetworkPolicy {
    inner: Arc<Inner>,
}

struct Inner {
    running: Mutex<Option<Arc<ProxyServer>>>,
    ctx: Arc<JudgeCtx>,
}

impl NetworkPolicy {
    /// A fresh policy: no proxy. The allowlist is read at construction and
    /// recompiled per enable, so settings edits between switches apply.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                running: Mutex::new(None),
                ctx: Arc::new(JudgeCtx::new(crate::settings::sandbox_network_allow())),
            }),
        }
    }

    /// A policy with an explicit allowlist (tests: the live smoke must not
    /// depend on the developer's real settings.toml). The hosts ride the
    /// session-amendment set, so an enable's settings re-read cannot wipe
    /// them — the same placement a user-approved host amendment gets.
    #[cfg(test)]
    pub fn for_allowlist(allow: Vec<String>) -> Self {
        let policy = Self::new();
        for host in allow {
            policy.inner.ctx.admit(&host);
        }
        policy
    }

    /// The face one confined call runs under, given the gate's access cell.
    /// A missing proxy where the managed shape requires one answers
    /// `Disabled` — fail-closed, never open (the transient bind window
    /// included).
    pub fn route_for(&self, access: NetworkAccess) -> NetworkRoute {
        match access {
            NetworkAccess::Off => NetworkRoute::Disabled,
            NetworkAccess::On => match self.inner.running.lock().unwrap().as_ref() {
                Some(proxy) => NetworkRoute::Managed {
                    proxy_port: proxy.port,
                },
                None => {
                    if self.inner.ctx.allowlist().is_empty() {
                        NetworkRoute::Direct
                    } else {
                        NetworkRoute::Disabled
                    }
                }
            },
        }
    }

    /// Install the amendment approver (once, at engine spawn).
    pub(crate) fn set_approver(&self, approver: Arc<dyn HostApprover>) {
        self.inner.ctx.install_approver(approver);
    }

    /// Apply the switch's proxy half: stop the proxy on off; with an
    /// on-switch and a non-empty allowlist, ensure the proxy is running
    /// where the platform can carry the managed route (a running proxy
    /// keeps serving — the next enable re-reads the file). Returns after
    /// the end-state is in place.
    pub async fn set_enabled(&self, access: NetworkAccess) {
        if access == NetworkAccess::Off {
            *self.inner.running.lock().unwrap() = None; // Drop aborts the loop.
            return;
        }
        if self.inner.running.lock().unwrap().is_some() {
            return;
        }
        // Every enable re-reads the configured set (plus the session's
        // amendments), so a settings edit between switches applies.
        self.inner
            .ctx
            .recompile(crate::settings::sandbox_network_allow());
        if self.inner.ctx.allowlist().is_empty() {
            return; // Direct route: no proxy.
        }
        if !super::managed_transport_ready() {
            // No managed transport where the policy wants one: no proxy
            // binds, so `route_for` answers Disabled — fail-closed, never
            // an open route against the configured intent.
            tracing::warn!(
                "managed network transport unavailable on this platform; sandbox network stays closed"
            );
            return;
        }
        match ProxyServer::bind(self.inner.ctx.clone()).await {
            Ok(server) => {
                *self.inner.running.lock().unwrap() = Some(Arc::new(server));
            }
            Err(error) => {
                // No proxy where the policy wants one: the route answers
                // Disabled (fail-closed), never open.
                tracing::warn!(%error, "managed network proxy failed to bind; sandbox network stays closed");
            }
        }
    }
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl ProxyServer {
    /// Bind the loopback listener and spawn the accept loop.
    async fn bind(ctx: Arc<JudgeCtx>) -> std::io::Result<ProxyServer> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        let shutdown = CancellationToken::new();
        let task_shutdown = shutdown.clone();
        let accept_loop = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    _ = task_shutdown.cancelled() => break,
                    accepted = listener.accept() => accepted,
                };
                let Ok((client, _)) = accepted else {
                    break;
                };
                let ctx = ctx.clone();
                let conn_shutdown = task_shutdown.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = conn_shutdown.cancelled() => {}
                        _ = serve_connection(client, ctx) => {}
                    }
                });
            }
        });
        Ok(ProxyServer {
            port,
            shutdown,
            accept_loop,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowlist(patterns: &[&str]) -> DomainAllowlist {
        DomainAllowlist::compile(&patterns.iter().map(|p| p.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn host_normalization_strips_case_port_brackets_and_dot() {
        assert_eq!(normalize_host("Crates.IO"), "crates.io");
        assert_eq!(normalize_host("crates.io."), "crates.io");
        assert_eq!(normalize_host("crates.io:443"), "crates.io");
        assert_eq!(normalize_host("[::1]"), "::1");
        assert_eq!(normalize_host("  EXAMPLE.com  "), "example.com");
    }

    #[test]
    fn wildcard_semantics_match_the_documented_trio() {
        let list = allowlist(&["*.example.com", "**.openai.com", "exact.org"]);
        // `*.` = subdomains only, never the apex.
        assert_eq!(list.judge("a.example.com"), HostVerdict::Allow);
        assert_eq!(list.judge("deep.a.example.com"), HostVerdict::Allow);
        assert_eq!(list.judge("example.com"), HostVerdict::NotAllowlisted);
        // `**.` = apex plus subdomains.
        assert_eq!(list.judge("openai.com"), HostVerdict::Allow);
        assert_eq!(list.judge("api.openai.com"), HostVerdict::Allow);
        // Exact.
        assert_eq!(list.judge("exact.org"), HostVerdict::Allow);
        assert_eq!(list.judge("noexact.org"), HostVerdict::NotAllowlisted);
        // A sibling suffix never matches.
        assert_eq!(list.judge("evilexample.com"), HostVerdict::NotAllowlisted);
        assert_eq!(
            list.judge("example.com.evil.io"),
            HostVerdict::NotAllowlisted
        );
        // A subdomain of an exact entry is not covered.
        assert_eq!(list.judge("a.exact.org"), HostVerdict::NotAllowlisted);
    }

    #[test]
    fn global_wildcard_never_compiles() {
        let list = allowlist(&["*"]);
        assert!(list.is_empty());
        assert_eq!(list.judge("anything.io"), HostVerdict::NotAllowlisted);
    }

    #[test]
    fn ip_literals_judge_directly() {
        let list = allowlist(&["1.2.3.4", "[::1]", "crates.io"]);
        assert_eq!(list.judge("1.2.3.4"), HostVerdict::Allow);
        assert_eq!(list.judge("::1"), HostVerdict::Allow);
        assert_eq!(list.judge("5.6.7.8"), HostVerdict::NotAllowlisted);
        // An allowlisted NAME never admits a bare IP target.
        assert_eq!(list.judge("127.0.0.1"), HostVerdict::NotAllowlisted);
    }

    #[test]
    fn private_ranges_are_recognized() {
        for ip in [
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.9",
            "169.254.1.1",
            "0.0.0.0",
            "::1",
            "fe80::1",
            "fc00::1",
        ] {
            assert!(
                DomainAllowlist::is_private_ip(ip.parse().unwrap()),
                "{ip} is private"
            );
        }
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700::1111"] {
            assert!(
                !DomainAllowlist::is_private_ip(ip.parse().unwrap()),
                "{ip} is public"
            );
        }
    }

    #[test]
    fn connect_requests_parse_their_authority() {
        let request = b"CONNECT crates.io:443 HTTP/1.1\r\nHost: crates.io:443\r\n\r\n";
        assert_eq!(
            parse_connect_request(request),
            Some(("crates.io".to_string(), 443))
        );
        let get = b"GET / HTTP/1.1\r\n\r\n";
        assert_eq!(parse_connect_request(get), None);
        let noport = b"CONNECT crates.io HTTP/1.1\r\n\r\n";
        assert_eq!(parse_connect_request(noport), None);
    }

    /// A live proxy round: allowlisted host tunnels to a local stub, a miss
    /// is refused with 403, and a name resolving into loopback space is
    /// refused even though its pattern matched (the SSRF guard).
    #[tokio::test(flavor = "multi_thread")]
    async fn proxy_tunnels_allowlisted_and_refuses_misses() {
        // The upstream stub: a plain HTTP responder on a loopback port.
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = upstream.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                        .await;
                });
            }
        });

        // "myhost.test" resolves to loopback — the pattern allowlists it,
        // but the resolved-IP guard must refuse it. The public name uses a
        // hosts-independent path: judge-level testing covers the pattern,
        // this test proves the plumbing end to end.
        let ctx = Arc::new(JudgeCtx::new(vec![format!("port-{upstream_port}.invalid")]));
        let proxy = ProxyServer::bind(ctx.clone()).await.unwrap();
        let port = proxy.port;

        // A miss (no approver installed) is refused.
        let mut miss = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        miss.write_all(b"CONNECT unknown.invalid:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut answer = String::new();
        miss.read_to_string(&mut answer).await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");

        // An explicitly allowlisted IP literal tunnels to the stub — the
        // literal path skips the private guard (the entry IS the user
        // naming the address), proving the relay loop end to end.
        let ctx2 = Arc::new(JudgeCtx::new(vec!["127.0.0.1".to_string()]));
        let proxy2 = ProxyServer::bind(ctx2.clone()).await.unwrap();
        let mut tunneled = tokio::net::TcpStream::connect(("127.0.0.1", proxy2.port))
            .await
            .unwrap();
        tunneled
            .write_all(format!("CONNECT 127.0.0.1:{upstream_port} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            let n = tunneled.read(&mut byte).await.unwrap();
            assert!(n > 0, "proxy closed before answering");
            head.push(byte[0]);
        }
        assert!(
            head.starts_with(b"HTTP/1.1 200"),
            "literal-allowlisted target tunnels: {}",
            String::from_utf8_lossy(&head)
        );
        tunneled.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        let mut body = String::new();
        tunneled.read_to_string(&mut body).await.unwrap();
        assert!(body.contains("ok"), "{body}");

        // A private-shaped TARGET that is not allowlisted as a literal
        // stays refused even though a `**.`-pattern could be read to cover
        // it: loopback cannot sneak in by name.
        let ctx3 = Arc::new(JudgeCtx::new(vec!["**.localhost".to_string()]));
        let proxy3 = ProxyServer::bind(ctx3.clone()).await.unwrap();
        let mut sneaky = tokio::net::TcpStream::connect(("127.0.0.1", proxy3.port))
            .await
            .unwrap();
        sneaky
            .write_all(b"CONNECT secret.localhost:80 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut answer = String::new();
        sneaky.read_to_string(&mut answer).await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");

        drop(proxy);
        drop(proxy2);
        drop(proxy3);
    }

    /// A canned approver for the ask-path tests.
    struct CannedApprover(std::sync::Mutex<Vec<HostDecision>>);

    #[async_trait::async_trait]
    impl HostApprover for CannedApprover {
        async fn request(&self, _host: &str, _port: u16) -> HostDecision {
            let mut verdicts = self.0.lock().unwrap();
            if verdicts.is_empty() {
                HostDecision::Deny
            } else {
                verdicts.remove(0)
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_approved_miss_tunnels_and_amends_the_allowlist() {
        let ctx = Arc::new(JudgeCtx::new(vec![]));
        ctx.install_approver(Arc::new(CannedApprover(
            vec![HostDecision::AllowSession].into(),
        )));
        // The upstream stub.
        let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = upstream.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut sock, _) = upstream.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await;
        });
        // The literal loopback target is NOT allowlisted: the ask fires,
        // the canned Session verdict amends the set, and the tunnel then
        // proceeds (the literal path skips the private guard — the user
        // named the address).
        let proxy = ProxyServer::bind(ctx.clone()).await.unwrap();
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", proxy.port))
            .await
            .unwrap();
        client
            .write_all(format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            let n = client.read(&mut byte).await.unwrap();
            assert!(n > 0, "proxy closed before answering");
            head.push(byte[0]);
        }
        assert!(
            head.starts_with(b"HTTP/1.1 200"),
            "the approved miss tunnels: {}",
            String::from_utf8_lossy(&head)
        );
        // The relay is live end to end: the request lands on the stub and
        // its response comes back through the tunnel.
        client.write_all(b"GET / HTTP/1.1\r\n\r\n").await.unwrap();
        let mut body = String::new();
        client.read_to_string(&mut body).await.unwrap();
        assert!(body.contains("ok"), "{body}");
        // The session set gained the host: a second tunnel skips the ask.
        assert_eq!(ctx.allowlist().judge("127.0.0.1"), HostVerdict::Allow);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_denied_miss_refuses_without_amending() {
        let ctx = Arc::new(JudgeCtx::new(vec![]));
        ctx.install_approver(Arc::new(CannedApprover(vec![].into())));
        let proxy = ProxyServer::bind(ctx.clone()).await.unwrap();
        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", proxy.port))
            .await
            .unwrap();
        client
            .write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut answer = String::new();
        client.read_to_string(&mut answer).await.unwrap();
        assert!(answer.starts_with("HTTP/1.1 403"), "{answer}");
        assert_eq!(
            ctx.allowlist().judge("127.0.0.1"),
            HostVerdict::NotAllowlisted
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_amendment_admits_the_host_for_the_next_tunnel() {
        let ctx = Arc::new(JudgeCtx::new(vec![]));
        assert_eq!(ctx.allowlist().judge("x.test"), HostVerdict::NotAllowlisted);
        ctx.admit("x.test");
        assert_eq!(ctx.allowlist().judge("x.test"), HostVerdict::Allow);
        // Admitting twice is a no-op (single entry).
        ctx.admit("x.test");
        assert_eq!(ctx.session_allow.lock().unwrap().len(), 1);
    }

    #[test]
    fn policy_route_fails_closed_without_a_proxy() {
        let policy = NetworkPolicy::new();
        // The switch alone is not enough: with an allowlist configured but
        // no bound proxy the managed shape answers disabled, never open.
        assert_eq!(
            policy.route_for(NetworkAccess::On),
            NetworkRoute::Direct,
            "an empty allowlist is the direct route"
        );
        let with_list = NetworkPolicy {
            inner: Arc::new(Inner {
                running: Mutex::new(None),
                ctx: Arc::new(JudgeCtx::new(vec!["crates.io".to_string()])),
            }),
        };
        assert_eq!(
            with_list.route_for(NetworkAccess::On),
            NetworkRoute::Disabled,
            "allowlist present but proxy not bound: fail closed"
        );
        assert_eq!(
            with_list.route_for(NetworkAccess::Off),
            NetworkRoute::Disabled
        );
    }
}
