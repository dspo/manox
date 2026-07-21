//! Per-connection logic: parse the first request, decide allowed/denied,
//! either pump bytes through to the upstream or close (with an explanatory
//! 511 for policy denials).

use crate::proxy::{DenyReason, ProxyEvent, RequestMethod, RequestOutcome, RuntimeState};
use anyhow::{Context as _, Result, anyhow, bail};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs as _};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use url::Url;

/// Buffer size for each direction of bidir copy.
const PUMP_BUFFER_SIZE: usize = 64 * 1024;

/// Cap on request/response header bytes.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// How long to wait for the client's request headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for outbound TCP connects.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Top-level entry from the listener thread.
pub(crate) fn handle(client: TcpStream, state: Arc<RuntimeState>) -> Result<()> {
    if let Err(error) = client.set_nodelay(true) {
        log::debug!("[http_proxy] failed to set TCP_NODELAY: {error}");
    }
    let mut client = client;
    if let Err(error) = client.set_read_timeout(Some(HEADER_READ_TIMEOUT)) {
        log::debug!("[http_proxy] failed to set header read timeout: {error}");
    }

    let (header_buf, header_end) = read_request_headers(&mut client)?;
    if let Err(error) = client.set_read_timeout(None) {
        log::debug!("[http_proxy] failed to clear client read timeout: {error}");
    }

    let request = ParsedRequest::parse(&header_buf[..header_end])?;
    let leftover_body = header_buf[header_end..].to_vec();

    match request {
        ParsedRequest::Connect { host, port } => {
            handle_connect(client, host, port, leftover_body, state)
        }
        ParsedRequest::Http {
            method,
            host,
            port,
            request_bytes,
        } => handle_http_forward(
            client,
            method,
            host,
            port,
            request_bytes,
            leftover_body,
            state,
        ),
    }
}

/// Reads request headers (until `\r\n\r\n`) from the client.
fn read_request_headers(client: &mut TcpStream) -> Result<(Vec<u8>, usize)> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let mut searched = 0usize;
    loop {
        let n = client.read(&mut tmp)?;
        if n == 0 {
            bail!("client closed before sending complete request headers");
        }
        buf.extend_from_slice(&tmp[..n]);
        let scan_start = searched.saturating_sub(3);
        if let Some(end) = find_double_crlf(&buf[scan_start..]) {
            return Ok((buf, scan_start + end));
        }
        searched = buf.len();
        if buf.len() > MAX_HEADER_BYTES {
            bail!("request headers exceed {MAX_HEADER_BYTES} bytes");
        }
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Parsed first request from the client.
enum ParsedRequest {
    Connect {
        host: String,
        port: u16,
    },
    Http {
        method: String,
        host: String,
        port: u16,
        request_bytes: Vec<u8>,
    },
}

impl ParsedRequest {
    fn parse(headers: &[u8]) -> Result<Self> {
        let mut header_storage = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut header_storage);
        let status = req.parse(headers).context("malformed HTTP request")?;
        if !status.is_complete() {
            bail!("incomplete HTTP request after \\r\\n\\r\\n boundary");
        }

        let method = req
            .method
            .ok_or_else(|| anyhow!("missing HTTP method"))?
            .to_string();
        let target = req.path.ok_or_else(|| anyhow!("missing request target"))?;

        if method.eq_ignore_ascii_case("CONNECT") {
            let (host, port) = parse_authority_form(target)?;
            Ok(ParsedRequest::Connect { host, port })
        } else if target.starts_with("http://") || target.starts_with("https://") {
            let (host, port, url) = parse_absolute_form_target(target)?;
            let request_bytes = build_origin_form_request(&method, &url, req.version, req.headers);
            Ok(ParsedRequest::Http {
                method,
                host,
                port,
                request_bytes,
            })
        } else {
            let host_hdr = req
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case("host"))
                .ok_or_else(|| anyhow!("origin-form request missing Host: header"))?;
            let value = std::str::from_utf8(host_hdr.value).context("Host: not valid UTF-8")?;
            let (host, port) = parse_host_header(value)?;
            Ok(ParsedRequest::Http {
                method,
                host,
                port,
                request_bytes: headers.to_vec(),
            })
        }
    }
}

/// Parse `host:port` (CONNECT authority-form). Port is required.
fn parse_authority_form(input: &str) -> Result<(String, u16)> {
    let (host_part, port_part) = input
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("CONNECT target '{input}' must include a port"))?;
    let port: u16 = port_part
        .parse()
        .with_context(|| format!("CONNECT target '{input}' has an invalid port"))?;
    let parsed = Url::parse(&format!("http://{host_part}"))
        .with_context(|| format!("parsing CONNECT target '{input}'"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("CONNECT target '{input}' has no host"))?
        .to_string();
    Ok((host, port))
}

/// Parse an absolute-form HTTP request target like `http://foo.com/path`.
fn parse_absolute_form_target(target: &str) -> Result<(String, u16, Url)> {
    let parsed =
        Url::parse(target).with_context(|| format!("parsing absolute-form target '{target}'"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("absolute-form target '{target}' has no host"))?
        .to_string();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow!("absolute-form target '{target}' has no port"))?;
    Ok((host, port, parsed))
}

/// Rewrite an absolute-form proxy request into origin-form for the origin
/// server. Strips `Proxy-*` headers (which are addressed to us and may carry
/// credentials).
fn build_origin_form_request(
    method: &str,
    url: &Url,
    version: Option<u8>,
    headers: &[httparse::Header],
) -> Vec<u8> {
    let mut target = url.path().to_string();
    if let Some(query) = url.query() {
        target.push('?');
        target.push_str(query);
    }
    let host_value = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        (None, _) => String::new(),
    };
    let minor_version = version.unwrap_or(1);

    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(format!("{method} {target} HTTP/1.{minor_version}\r\n").as_bytes());
    out.extend_from_slice(format!("Host: {host_value}\r\n").as_bytes());
    for header in headers {
        let name = header.name;
        if name.eq_ignore_ascii_case("host")
            || name
                .get(.."proxy-".len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("proxy-"))
        {
            continue;
        }
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(header.value);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// Parse a `Host:` header value into `(host, port)`. Default port is 80.
fn parse_host_header(value: &str) -> Result<(String, u16)> {
    let value = value.trim();
    if value.is_empty() {
        bail!("empty Host header");
    }
    let parsed = Url::parse(&format!("http://{value}"))
        .with_context(|| format!("parsing Host header '{value}'"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("Host header '{value}' has no host"))?
        .to_string();
    let port = parsed.port().unwrap_or(80);
    Ok((host, port))
}

/// Normalize a hostname for allowlist matching.
fn normalize_host(host: &str) -> String {
    let stripped = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    stripped.trim_end_matches('.').to_string()
}

/// Whether a hostname is an IP literal.
fn is_ip_literal(host: &str) -> bool {
    let stripped = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    stripped.parse::<IpAddr>().is_ok()
}

/// The policy check shared by CONNECT and HTTP forward.
fn policy_denial(host: &str, port: u16, state: &RuntimeState) -> Option<DenyReason> {
    if state.allowlist.allows_any() {
        return None;
    }
    if is_ip_literal(host) {
        return Some(DenyReason::IpLiteralRejected {
            target: format!("{host}:{port}"),
        });
    }
    if !state.allowlist.allows(host) {
        return Some(DenyReason::HostNotInAllowlist {
            host: host.to_string(),
        });
    }
    None
}

/// How an approved request will reach its destination.
enum Route {
    /// Connect directly to these resolved-and-vetted addresses.
    Direct(Vec<SocketAddr>),
}

enum RouteFailure {
    Denied(DenyReason),
    Error(anyhow::Error),
}

/// Decide how to reach `host:port`, resolving and vetting addresses for
/// direct connections. DNS rebinding protection: filter resolved addresses
/// that point into loopback / private / link-local space.
fn plan_route(host: &str, port: u16, state: &RuntimeState) -> Result<Route, RouteFailure> {
    let resolved: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|error| RouteFailure::Error(anyhow!("resolving {host}:{port}: {error}")))?
        .collect();
    if resolved.is_empty() {
        return Err(RouteFailure::Error(anyhow!(
            "{host}:{port} did not resolve to any address"
        )));
    }

    let vetted: Vec<SocketAddr> = if state.allowlist.allows_any() {
        resolved
    } else {
        resolved
            .into_iter()
            .filter(|addr| !is_forbidden_ip(addr.ip()))
            .collect()
    };
    if vetted.is_empty() {
        return Err(RouteFailure::Denied(DenyReason::ResolvedToForbiddenIp {
            host: host.to_string(),
        }));
    }
    Ok(Route::Direct(vetted))
}

/// Whether a resolved address is in loopback / private / link-local space —
/// destinations a hostname allowlist must never reach.
fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_ipv4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_forbidden_ipv4(v4);
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

fn is_forbidden_ipv4(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || (octets[0] == 100 && (octets[1] & 0xc0) == 64)
}

fn handle_connect(
    mut client: TcpStream,
    host: String,
    port: u16,
    leftover_body: Vec<u8>,
    state: Arc<RuntimeState>,
) -> Result<()> {
    let normalized = normalize_host(&host);

    if let Some(reason) = policy_denial(&normalized, port, &state) {
        return deny_request(
            &mut client,
            &state,
            normalized,
            port,
            RequestMethod::Connect,
            reason,
        );
    }

    let route = match plan_route(&normalized, port, &state) {
        Ok(route) => route,
        Err(RouteFailure::Denied(reason)) => {
            return deny_request(
                &mut client,
                &state,
                normalized,
                port,
                RequestMethod::Connect,
                reason,
            );
        }
        Err(RouteFailure::Error(error)) => {
            log::debug!("[http_proxy] routing failed for CONNECT {normalized}:{port}: {error:#}");
            return Ok(());
        }
    };

    emit(
        &state,
        ProxyEvent::RequestAttempt {
            host: normalized.clone(),
            port,
            method: RequestMethod::Connect,
            outcome: RequestOutcome::Allowed,
        },
    );

    let mut upstream = match open_route(&route, &normalized, port) {
        Ok(stream) => stream,
        Err(error) => {
            log::debug!(
                "[http_proxy] upstream open failed for CONNECT {normalized}:{port}: {error:#}"
            );
            return Ok(());
        }
    };

    if let Err(error) = upstream.set_nodelay(true) {
        log::debug!("[http_proxy] failed to set TCP_NODELAY on upstream: {error}");
    }

    client.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
    if !leftover_body.is_empty() {
        upstream.write_all(&leftover_body)?;
    }

    let started = Instant::now();
    let (pumped_to_remote, pumped_from_remote) = pump_bidir(client, upstream);

    emit(
        &state,
        ProxyEvent::RequestCompleted {
            host: normalized,
            port,
            method: RequestMethod::Connect,
            bytes_to_remote: pumped_to_remote + leftover_body.len() as u64,
            bytes_from_remote: pumped_from_remote,
            duration_ms: started.elapsed().as_millis() as u64,
        },
    );

    Ok(())
}

fn handle_http_forward(
    mut client: TcpStream,
    method: String,
    host: String,
    port: u16,
    request_bytes: Vec<u8>,
    leftover_body: Vec<u8>,
    state: Arc<RuntimeState>,
) -> Result<()> {
    let normalized = normalize_host(&host);

    if let Some(reason) = policy_denial(&normalized, port, &state) {
        return deny_request(
            &mut client,
            &state,
            normalized,
            port,
            RequestMethod::Http(method),
            reason,
        );
    }

    let route = match plan_route(&normalized, port, &state) {
        Ok(route) => route,
        Err(RouteFailure::Denied(reason)) => {
            return deny_request(
                &mut client,
                &state,
                normalized,
                port,
                RequestMethod::Http(method),
                reason,
            );
        }
        Err(RouteFailure::Error(error)) => {
            log::debug!("[http_proxy] routing failed for {method} {normalized}:{port}: {error:#}");
            return Ok(());
        }
    };

    emit(
        &state,
        ProxyEvent::RequestAttempt {
            host: normalized.clone(),
            port,
            method: RequestMethod::Http(method.clone()),
            outcome: RequestOutcome::Allowed,
        },
    );

    let mut upstream = match open_route(&route, &normalized, port) {
        Ok(stream) => stream,
        Err(error) => {
            log::debug!(
                "[http_proxy] upstream open failed for {method} {normalized}:{port}: {error:#}"
            );
            return Ok(());
        }
    };

    if let Err(error) = upstream.set_nodelay(true) {
        log::debug!("[http_proxy] failed to set TCP_NODELAY on upstream: {error}");
    }

    upstream.write_all(&request_bytes)?;
    if !leftover_body.is_empty() {
        upstream.write_all(&leftover_body)?;
    }

    let started = Instant::now();
    let (pumped_to_remote, pumped_from_remote) = pump_bidir(client, upstream);
    let to_remote = pumped_to_remote + request_bytes.len() as u64 + leftover_body.len() as u64;

    emit(
        &state,
        ProxyEvent::RequestCompleted {
            host: normalized,
            port,
            method: RequestMethod::Http(method),
            bytes_to_remote: to_remote,
            bytes_from_remote: pumped_from_remote,
            duration_ms: started.elapsed().as_millis() as u64,
        },
    );

    Ok(())
}

/// Open the connection that will carry this request's bytes to the origin.
fn open_route(route: &Route, host: &str, port: u16) -> Result<TcpStream> {
    match route {
        Route::Direct(addrs) => connect_to_any(addrs, host, port),
    }
}

/// Connect to the first address that accepts, with a per-attempt timeout.
fn connect_to_any(addrs: &[SocketAddr], host: &str, port: u16) -> Result<TcpStream> {
    let mut last_error = None;
    for addr in addrs {
        match TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    match last_error {
        Some(error) => Err(anyhow!("connect to {host}:{port}: {error}")),
        None => Err(anyhow!("no addresses to connect to for {host}:{port}")),
    }
}

/// Send a 511 response with an explanatory body. Closes the connection.
fn deny_request(
    client: &mut TcpStream,
    state: &RuntimeState,
    host: String,
    port: u16,
    method: RequestMethod,
    reason: DenyReason,
) -> Result<()> {
    emit(
        state,
        ProxyEvent::RequestAttempt {
            host,
            port,
            method,
            outcome: RequestOutcome::Denied {
                reason: reason.clone(),
            },
        },
    );

    let body = format!(
        "Request blocked by the sandbox network policy.\n\n  \
         Reason: {}\n\n  \
         This is not a network or server failure — it's a policy decision.\n  \
         To proceed, ask the user to approve the host or run with `unsandboxed: true`.\n",
        reason.human_explanation()
    );
    let response = format!(
        "HTTP/1.1 511 Network Authentication Required\r\n\
         Via: 1.1 manox-sandbox-proxy\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len(),
    );
    client.write_all(response.as_bytes())?;
    Ok(())
}

fn emit(state: &RuntimeState, event: ProxyEvent) {
    let _ = state.events.unbounded_send(event);
}

/// Bidirectional byte pump. The client→remote direction runs on a spawned
/// thread; remote→client runs on the current thread.
fn pump_bidir(client: TcpStream, upstream: TcpStream) -> (u64, u64) {
    let client_read = match client.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::debug!("[http_proxy] failed to clone client socket: {e}");
            return (0, 0);
        }
    };
    let upstream_read = match upstream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log::debug!("[http_proxy] failed to clone upstream socket: {e}");
            return (0, 0);
        }
    };
    let client_write = client;
    let upstream_write = upstream;

    let to_remote_handle = match thread::Builder::new()
        .name("http-proxy-pump-out".to_string())
        .stack_size(128 * 1024)
        .spawn(move || copy_one_way(client_read, upstream_write))
    {
        Ok(handle) => handle,
        Err(error) => {
            log::warn!("[http_proxy] failed to spawn pump thread: {error}");
            return (0, 0);
        }
    };
    let from_remote = copy_one_way(upstream_read, client_write);
    let to_remote = to_remote_handle.join().unwrap_or_else(|_| {
        log::warn!("[http_proxy] pump thread panicked");
        0
    });
    (to_remote, from_remote)
}

/// Copy bytes from `from` to `to` until EOF on the read side or write failure.
fn copy_one_way(mut from: impl Read, mut to: impl WriteAndShutdown) -> u64 {
    let mut total = 0u64;
    let mut buf = vec![0u8; PUMP_BUFFER_SIZE];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if to.write_all(&buf[..n]).is_err() {
            break;
        }
        total += n as u64;
    }
    let _ = to.shutdown(Shutdown::Write);
    total
}

trait WriteAndShutdown: Write {
    fn shutdown(&self, how: Shutdown) -> std::io::Result<()>;
}

impl WriteAndShutdown for TcpStream {
    fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
        TcpStream::shutdown(self, how)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::{Allowlist, HostPattern};
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn plan_route_denies_host_resolving_to_loopback() {
        let state = runtime_state(Allowlist::from_patterns([
            HostPattern::parse("github.com").unwrap()
        ]));
        match plan_route("localhost", 80, &state) {
            Err(RouteFailure::Denied(DenyReason::ResolvedToForbiddenIp { host })) => {
                assert_eq!(host, "localhost");
            }
            Ok(_) => panic!("expected denial"),
            Err(RouteFailure::Denied(reason)) => panic!("unexpected deny reason: {reason:?}"),
            Err(RouteFailure::Error(error)) => panic!("expected denial, got error: {error}"),
        }
    }

    #[test]
    fn plan_route_allows_loopback_when_allowlist_allows_any() {
        let state = runtime_state(Allowlist::any());
        match plan_route("localhost", 80, &state) {
            Ok(Route::Direct(addrs)) => {
                assert!(!addrs.is_empty());
                assert!(addrs.iter().all(|addr| addr.ip().is_loopback()));
            }
            Err(RouteFailure::Denied(reason)) => panic!("unexpected denial: {reason:?}"),
            Err(RouteFailure::Error(error)) => panic!("unexpected error: {error}"),
        }
    }

    fn runtime_state(allowlist: Allowlist) -> RuntimeState {
        let (events, _receiver) = futures::channel::mpsc::unbounded();
        RuntimeState {
            allowlist,
            events,
            active_connections: AtomicUsize::new(0),
        }
    }

    #[test]
    fn parse_authority_form_basic() {
        let (h, p) = parse_authority_form("github.com:443").unwrap();
        assert_eq!(h, "github.com");
        assert_eq!(p, 443);
    }

    #[test]
    fn parse_authority_form_ipv6() {
        let (h, p) = parse_authority_form("[::1]:443").unwrap();
        assert_eq!(h, "[::1]");
        assert_eq!(p, 443);
    }

    #[test]
    fn parse_authority_form_requires_port() {
        assert!(parse_authority_form("github.com").is_err());
    }

    #[test]
    fn parse_absolute_form_basic() {
        let (h, p, _) = parse_absolute_form_target("http://example.com/path").unwrap();
        assert_eq!(h, "example.com");
        assert_eq!(p, 80);
    }

    #[test]
    fn parse_absolute_form_with_port() {
        let (h, p, _) = parse_absolute_form_target("http://example.com:8080/").unwrap();
        assert_eq!(h, "example.com");
        assert_eq!(p, 8080);
    }

    #[test]
    fn host_header_default_port() {
        let (h, p) = parse_host_header("example.com").unwrap();
        assert_eq!(h, "example.com");
        assert_eq!(p, 80);
    }

    #[test]
    fn host_header_explicit_port() {
        let (h, p) = parse_host_header("example.com:8080").unwrap();
        assert_eq!(h, "example.com");
        assert_eq!(p, 8080);
    }

    #[test]
    fn detects_ip_literals() {
        assert!(is_ip_literal("1.2.3.4"));
        assert!(is_ip_literal("[::1]"));
        assert!(is_ip_literal("::1"));
        assert!(!is_ip_literal("github.com"));
        assert!(!is_ip_literal("localhost"));
    }

    #[test]
    fn forbidden_ips_cover_local_space() {
        for forbidden in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.100.1.1",
            "0.0.0.0",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(
                is_forbidden_ip(forbidden.parse().unwrap()),
                "{forbidden} should be forbidden"
            );
        }
        for public in ["140.82.112.3", "8.8.8.8", "2606:4700::6810:84e5"] {
            assert!(
                !is_forbidden_ip(public.parse().unwrap()),
                "{public} should be allowed"
            );
        }
    }

    #[test]
    fn parsed_request_recognizes_connect() {
        let req = b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n";
        match ParsedRequest::parse(req).unwrap() {
            ParsedRequest::Connect { host, port } => {
                assert_eq!(host, "example.com");
                assert_eq!(port, 443);
            }
            ParsedRequest::Http { .. } => panic!("expected Connect"),
        }
    }

    #[test]
    fn parsed_request_recognizes_http_absolute_form() {
        let req = b"GET http://example.com/foo HTTP/1.1\r\nHost: example.com\r\n\r\n";
        match ParsedRequest::parse(req).unwrap() {
            ParsedRequest::Http {
                method, host, port, ..
            } => {
                assert_eq!(method, "GET");
                assert_eq!(host, "example.com");
                assert_eq!(port, 80);
            }
            ParsedRequest::Connect { .. } => panic!("expected Http"),
        }
    }

    #[test]
    fn absolute_form_is_rewritten_to_origin_form() {
        let req = b"GET http://example.com/foo?q=1 HTTP/1.1\r\n\
            Host: wrong.example\r\n\
            Proxy-Connection: keep-alive\r\n\
            Proxy-Authorization: Basic c2VjcmV0\r\n\
            User-Agent: test\r\n\r\n";
        match ParsedRequest::parse(req).unwrap() {
            ParsedRequest::Http { request_bytes, .. } => {
                let text = String::from_utf8(request_bytes).unwrap();
                assert!(text.starts_with("GET /foo?q=1 HTTP/1.1\r\n"), "{text}");
                assert!(text.contains("Host: example.com\r\n"), "{text}");
                assert!(!text.contains("wrong.example"), "{text}");
                assert!(!text.to_ascii_lowercase().contains("proxy-"), "{text}");
                assert!(text.contains("User-Agent: test\r\n"), "{text}");
                assert!(text.ends_with("\r\n\r\n"), "{text}");
            }
            ParsedRequest::Connect { .. } => panic!("expected Http"),
        }
    }

    #[test]
    fn parsed_request_recognizes_http_origin_form_via_host_header() {
        let req = b"GET /foo HTTP/1.1\r\nHost: example.com:8080\r\n\r\n";
        match ParsedRequest::parse(req).unwrap() {
            ParsedRequest::Http {
                host,
                port,
                request_bytes,
                ..
            } => {
                assert_eq!(host, "example.com");
                assert_eq!(port, 8080);
                assert_eq!(request_bytes, req.to_vec());
            }
            ParsedRequest::Connect { .. } => panic!("expected Http"),
        }
    }
}
