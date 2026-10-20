//! Fetch core of the `web_fetch` tool: URL validation, the transport guard
//! (public-unicast DNS with connection pinning and NAT64 translation checks,
//! same-origin redirects, hop / byte / char / time caps), charset-aware
//! decoding, and a structured result/error split where a non-2xx status is
//! data, not failure. Contains no model-facing vocabulary — the tool face in
//! `mod` renders these types.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use parking_lot::Mutex;
use reqwest::dns::Resolve;
use reqwest::redirect::Policy;
use tokio_util::sync::CancellationToken;
use url::{Host, Url};

use super::proxy::{self, Route};
use super::resolver::{PublicUnicastResolver, Slot, SystemResolver, is_public_unicast};

/// `Accept` header sent on every request: a fetched document is text, and a
/// server able to negotiate formats should answer with one of these.
const ACCEPT: &str = "text/html,application/xhtml+xml,text/*;q=0.9,application/json;q=0.8";

/// Fixed URL-length cap; long URLs are attack surface without serving any
/// fetch use case. Enforced on the request URL and on every redirect target.
const MAX_URL_LEN: usize = 2048;

/// Deployment-side limits. The model-facing schema exposes none of these —
/// they are transport/resource policy, not call parameters.
pub(crate) struct WebFetchLimits {
    /// Hard cap on the response body in bytes; exceeding it aborts the read.
    pub(crate) max_response_bytes: usize,
    /// Cap on the decoded body in characters; exceeding it truncates.
    pub(crate) max_body_chars: usize,
    /// Maximum same-origin redirect hops.
    pub(crate) max_redirects: usize,
    /// Total request budget (connect + redirects + body).
    pub(crate) timeout: Duration,
    /// Budget for the optional model extraction pass.
    pub(crate) extract_timeout: Duration,
    /// Defensive cap on the fully rendered tool output.
    pub(crate) max_output_chars: usize,
}

impl WebFetchLimits {
    pub(crate) fn production() -> Self {
        Self {
            max_response_bytes: 5_000_000,
            max_body_chars: 100_000,
            max_redirects: 5,
            timeout: Duration::from_secs(30),
            extract_timeout: Duration::from_secs(60),
            max_output_chars: 200_000,
        }
    }
}

/// A completed fetch. `status` is part of the resource state: a 404 page is a
/// successfully fetched 404 page.
#[derive(Debug)]
pub(crate) struct WebFetchResult {
    pub(crate) final_url: String,
    pub(crate) status: u16,
    pub(crate) content_type: String,
    pub(crate) total_bytes: u64,
    pub(crate) body: BodyKind,
    pub(crate) truncated: bool,
}

#[derive(Debug)]
pub(crate) enum BodyKind {
    /// Decoded HTML; rendering to readable text is the tool face's job.
    Html(String),
    Text(String),
}

/// Structured failures with stable, machine-routable code prefixes.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WebFetchError {
    #[error("WEB_INVALID_URL: {0}")]
    InvalidUrl(String),
    #[error("WEB_BLOCKED_URL: {0}")]
    BlockedUrl(String),
    #[error("WEB_FETCH_TOO_LARGE: received {received} bytes, cap is {cap}")]
    TooLarge { received: u64, cap: usize },
    #[error("WEB_FETCH_TIMEOUT: request timed out after {after:?}")]
    Timeout { after: Duration },
    #[error("WEB_REDIRECT_BLOCKED: {0}")]
    RedirectBlocked(String),
    #[error("WEB_UNSUPPORTED_CONTENT_TYPE: {0}")]
    UnsupportedContentType(String),
    #[error("WEB_ABORTED: cancelled")]
    Aborted,
    #[error("WEB_PROVIDER_ERROR: {0}")]
    Transport(String),
}

/// How to treat a response body based on its content type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentKind {
    Html,
    Text,
    Binary,
    /// Header absent — treated as unsupported rather than guessed.
    Missing,
}

pub(crate) fn classify_content_type(content_type: &str) -> ContentKind {
    // Strip parameters (`text/html; charset=utf-8` → `text/html`). The media
    // type itself is case-insensitive, so it is folded before it is matched.
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match mime.as_str() {
        "text/html" | "application/xhtml+xml" => ContentKind::Html,
        "" => ContentKind::Missing,
        _ if mime.starts_with("text/") => ContentKind::Text,
        _ if mime.starts_with("application/json") => ContentKind::Text,
        _ if mime.starts_with("application/xml") => ContentKind::Text,
        _ if mime.starts_with("application/yaml") => ContentKind::Text,
        _ if mime.starts_with("application/javascript") => ContentKind::Text,
        _ if mime.ends_with("+json") || mime.ends_with("+xml") => ContentKind::Text,
        _ => ContentKind::Binary,
    }
}

/// Which resolver stack and pre-flight checks a fetch runs with. `guarded` is
/// the DNS-level policy: a proxied hop cannot carry it (the proxy resolves the
/// origin, and the proxy host itself is usually a local listener the policy
/// would refuse), while URL validation, the IP-literal check, the redirect
/// policy, and every cap hold in both modes.
pub(super) struct Transport {
    inner_resolver: Arc<dyn Resolve>,
    guarded: bool,
}

impl Transport {
    pub(super) fn guarded(inner: Arc<dyn Resolve>) -> Self {
        Self {
            inner_resolver: inner,
            guarded: true,
        }
    }

    fn unguarded(inner: Arc<dyn Resolve>) -> Self {
        Self {
            inner_resolver: inner,
            guarded: false,
        }
    }
}

/// Production entry point: the environment's proxy route picks the transport,
/// then the fetch runs under it.
pub(crate) async fn fetch(
    url: &str,
    limits: &WebFetchLimits,
    cancel: &CancellationToken,
) -> Result<WebFetchResult, WebFetchError> {
    let parsed = validate_url(url)?;
    let (transport, proxy) = match proxy::route_for(&parsed) {
        Route::Direct => (Transport::guarded(Arc::new(SystemResolver)), None),
        Route::Proxied(proxy) => (Transport::unguarded(Arc::new(SystemResolver)), Some(proxy)),
    };
    fetch_parsed(&parsed, limits, cancel, &transport, proxy).await
}

/// Test entry point: the same fetch under an explicit transport and proxy.
#[cfg(test)]
pub(super) async fn fetch_with_transport(
    url: &str,
    limits: &WebFetchLimits,
    cancel: &CancellationToken,
    transport: &Transport,
    proxy: Option<Url>,
) -> Result<WebFetchResult, WebFetchError> {
    let parsed = validate_url(url)?;
    fetch_parsed(&parsed, limits, cancel, transport, proxy).await
}

async fn fetch_parsed(
    parsed: &Url,
    limits: &WebFetchLimits,
    cancel: &CancellationToken,
    transport: &Transport,
    proxy: Option<Url>,
) -> Result<WebFetchResult, WebFetchError> {
    preflight_ip_literal(parsed)?;

    let slot: Slot = Arc::new(Mutex::new(None));
    let resolver: Arc<dyn Resolve> = if transport.guarded {
        Arc::new(PublicUnicastResolver::new(
            Arc::clone(&transport.inner_resolver),
            Arc::clone(&slot),
        ))
    } else {
        Arc::clone(&transport.inner_resolver)
    };
    let policy = same_origin_policy(parsed, limits.max_redirects, Arc::clone(&slot));
    let client = build_client(limits.timeout, resolver, policy, proxy)?;

    let response = tokio::select! {
        r = client.get(parsed.clone()).send() => r,
        _ = cancel.cancelled() => return Err(WebFetchError::Aborted),
    };
    let response = match response {
        Ok(r) => r,
        Err(e) => return Err(map_request_error(e, cancel, &slot, limits.timeout)),
    };

    let final_url = response.url().to_string();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // The body kind and the decoder are settled before the body is read: a
    // refused content type or an unknown charset must not cost a full download.
    let kind = classify_content_type(&content_type);
    match kind {
        ContentKind::Binary => {
            return Err(WebFetchError::UnsupportedContentType(format!(
                "binary content (content-type {content_type}); fetch it via Bash to a file or use the browser tools"
            )));
        }
        ContentKind::Missing => {
            return Err(WebFetchError::UnsupportedContentType(
                "no Content-Type header; refusing to guess how to decode the body".to_string(),
            ));
        }
        ContentKind::Html | ContentKind::Text => {}
    }
    let encoding = charset_encoding(&content_type)?;

    // A declared length over the cap is refused before any read; a stream that
    // grows past it is cut short and flagged instead, so a server that
    // under-reports its length still yields a bounded, usable body.
    let cap = limits.max_response_bytes;
    if let Some(declared) = response.content_length()
        && declared > cap as u64
    {
        return Err(WebFetchError::TooLarge {
            received: declared,
            cap,
        });
    }

    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut truncated_by_bytes = false;
    let mut stream = response.bytes_stream();
    while let Some(chunk_res) = stream.next().await {
        if cancel.is_cancelled() {
            return Err(WebFetchError::Aborted);
        }
        let chunk = chunk_res.map_err(|e| map_request_error(e, cancel, &slot, limits.timeout))?;
        let remaining = cap - buf.len();
        if chunk.len() > remaining {
            buf.extend_from_slice(&chunk[..remaining]);
            truncated_by_bytes = true;
            break;
        }
        buf.extend_from_slice(&chunk);
    }

    let (decoded, _malformed) = encoding.decode_without_bom_handling(&buf);
    let text = decoded.into_owned();

    // Soft char cap: truncated content stays useful, so cut and flag.
    let truncated_by_chars = text.chars().count() > limits.max_body_chars;
    let body_text = if truncated_by_chars {
        text.chars().take(limits.max_body_chars).collect::<String>()
    } else {
        text
    };

    let body = match kind {
        ContentKind::Html => BodyKind::Html(body_text),
        _ => BodyKind::Text(body_text),
    };
    Ok(WebFetchResult {
        final_url,
        status,
        content_type,
        total_bytes: buf.len() as u64,
        body,
        truncated: truncated_by_bytes || truncated_by_chars,
    })
}

fn validate_url(url: &str) -> Result<Url, WebFetchError> {
    let parsed = Url::parse(url)
        .map_err(|e| WebFetchError::InvalidUrl(format!("unparseable URL ({e}): {url}")))?;
    validate_url_parts(&parsed)?;
    Ok(parsed)
}

/// The transport hygiene every request URL must satisfy, whether it was named
/// by the model or arrived as a redirect target.
fn validate_url_parts(parsed: &Url) -> Result<(), WebFetchError> {
    if parsed.as_str().len() > MAX_URL_LEN {
        return Err(WebFetchError::InvalidUrl(format!(
            "URL exceeds {MAX_URL_LEN} characters"
        )));
    }
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(WebFetchError::InvalidUrl(format!(
            "only supports http/https URLs; got scheme {:?}",
            parsed.scheme()
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(WebFetchError::InvalidUrl(
            "embedded credentials are not accepted".to_string(),
        ));
    }
    Ok(())
}

/// IP-literal hosts never reach the resolver, so the public-unicast policy must
/// be applied here, before any connection exists. This check is unconditional:
/// a proxied hop skips the DNS guard (the proxy resolves the origin), but a
/// literal needs no resolution, and handing it to a proxy running on this
/// machine would reach exactly the service the address policy keeps out of
/// reach.
fn preflight_ip_literal(parsed: &Url) -> Result<(), WebFetchError> {
    let host_ip = match parsed.host() {
        Some(Host::Ipv4(ip)) => IpAddr::from(ip),
        Some(Host::Ipv6(ip)) => IpAddr::from(ip),
        _ => return Ok(()),
    };
    if !is_public_unicast(host_ip) {
        return Err(WebFetchError::BlockedUrl(format!(
            "refuses non-public destination {host_ip}"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: String,
    port: u16,
}

fn origin_of(url: &Url) -> Origin {
    Origin {
        scheme: url.scheme().to_string(),
        host: url.host_str().unwrap_or("").to_string(),
        port: url.port_or_known_default().unwrap_or(0),
    }
}

/// Same-origin redirects only: each hop re-resolves through the (guarding)
/// resolver because it is a new request through the same client, and re-earns
/// the URL hygiene the original request passed. Hop accounting matches
/// `Policy::limited`: `previous` includes the original URL, so the Nth redirect
/// sees N entries.
fn same_origin_policy(origin: &Url, max_redirects: usize, slot: Slot) -> Policy {
    let origin = origin_of(origin);
    Policy::custom(move |attempt| {
        if attempt.previous().len() > max_redirects {
            *slot.lock() = Some(WebFetchError::RedirectBlocked(format!(
                "exceeded {max_redirects} redirect hops at {}",
                attempt.url()
            )));
            return attempt.error("too many redirects");
        }
        if let Err(error) = validate_url_parts(attempt.url()) {
            *slot.lock() = Some(error);
            return attempt.error("redirect target fails URL validation");
        }
        if origin_of(attempt.url()) != origin {
            *slot.lock() = Some(WebFetchError::RedirectBlocked(format!(
                "cross-origin redirect to {}",
                attempt.url()
            )));
            return attempt.error("cross-origin redirect blocked");
        }
        attempt.follow()
    })
}

fn build_client(
    timeout: Duration,
    resolver: Arc<dyn Resolve>,
    policy: Policy,
    proxy: Option<Url>,
) -> Result<reqwest::Client, WebFetchError> {
    // `no_proxy` keeps the client hermetic: the route decision read the
    // environment once, so reqwest must not read it again per request.
    let mut default_headers = reqwest::header::HeaderMap::new();
    default_headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static(ACCEPT),
    );
    let mut builder = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(policy)
        .no_proxy()
        .user_agent(format!("manox/{}", env!("CARGO_PKG_VERSION")))
        .default_headers(default_headers)
        .dns_resolver2(resolver);
    if let Some(proxy) = proxy {
        let proxy = reqwest::Proxy::all(proxy.as_str())
            .map_err(|e| WebFetchError::Transport(format!("proxy {proxy} is unusable: {e}")))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| WebFetchError::Transport(format!("client build failed: {e}")))
}

fn map_request_error(
    e: reqwest::Error,
    cancel: &CancellationToken,
    slot: &Slot,
    timeout: Duration,
) -> WebFetchError {
    if let Some(err) = slot.lock().take() {
        return err;
    }
    if cancel.is_cancelled() {
        return WebFetchError::Aborted;
    }
    if e.is_timeout() {
        return WebFetchError::Timeout { after: timeout };
    }
    WebFetchError::Transport(format!("request failed: {e}"))
}

/// Charset comes only from the Content-Type header; the default is UTF-8 and an
/// unrecognized label is an error rather than a silent fallback.
fn charset_encoding(content_type: &str) -> Result<&'static encoding_rs::Encoding, WebFetchError> {
    let Some(label) = charset_label(content_type) else {
        return Ok(encoding_rs::UTF_8);
    };
    encoding_rs::Encoding::for_label(label.as_bytes()).ok_or_else(|| {
        WebFetchError::UnsupportedContentType(format!(
            "unknown charset {label:?} in content-type {content_type}"
        ))
    })
}

/// The `charset` parameter of a Content-Type header. Parameter names are
/// case-insensitive and may be spaced out (`CHARSET = "gbk"`), both of which a
/// server is entitled to send.
fn charset_label(content_type: &str) -> Option<String> {
    content_type.split(';').skip(1).find_map(|parameter| {
        let (name, value) = parameter.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| value.trim().trim_matches('"').to_string())
    })
}
#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::super::test_server::{self, SERVER_HOST, ScriptedResponse, TestServer};
    use super::*;

    fn limits() -> WebFetchLimits {
        WebFetchLimits::production()
    }

    /// No DNS guard: for tests whose refusal happens before any connection.
    fn unguarded_transport() -> Transport {
        Transport::unguarded(Arc::new(SystemResolver))
    }

    /// Answers every name with one scripted server's socket.
    #[derive(Clone)]
    struct LoopbackResolver {
        addr: SocketAddr,
    }

    impl Resolve for LoopbackResolver {
        fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            let addr = self.addr;
            Box::pin(async move {
                let addrs = vec![addr];
                Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
            })
        }
    }

    fn server_transport(server: &TestServer) -> Transport {
        Transport::unguarded(Arc::new(LoopbackResolver { addr: server.addr }))
    }

    #[tokio::test]
    async fn url_validation_rejects_bad_input() {
        let long = format!("http://example.com/{}", "a".repeat(2049));
        for (url, needle) in [
            (long.as_str(), "2048"),
            ("ftp://example.com", "http/https"),
            ("not a url", "unparseable"),
            ("http://user:pass@example.com/", "credentials"),
        ] {
            let err = fetch_with_transport(
                url,
                &limits(),
                &CancellationToken::new(),
                &unguarded_transport(),
                None,
            )
            .await
            .unwrap_err();
            assert!(matches!(err, WebFetchError::InvalidUrl(_)), "{url}: {err}");
            assert!(err.to_string().contains(needle), "{url}: {err}");
        }
    }

    #[tokio::test]
    async fn private_ip_literal_is_blocked_preflight() {
        let err = fetch_with_transport(
            "http://127.0.0.1:9/x",
            &limits(),
            &CancellationToken::new(),
            &Transport::guarded(Arc::new(SystemResolver)),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, WebFetchError::BlockedUrl(_)), "{err}");
        assert!(err.to_string().contains("127.0.0.1"), "{err}");
    }

    /// Hostnames that resolve into private space are refused by the
    /// validating resolver — no connection is ever attempted.
    struct StaticResolver {
        map: HashMap<String, Vec<SocketAddr>>,
    }

    impl Resolve for StaticResolver {
        fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
            let host = name.as_str().to_string();
            let addrs = self.map.get(host.as_str()).cloned().unwrap_or_default();
            Box::pin(async move {
                if addrs.is_empty() {
                    return Err(format!("no static mapping for {host}").into());
                }
                Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
            })
        }
    }

    #[tokio::test]
    async fn hostname_resolving_to_private_space_is_blocked() {
        let mut map = HashMap::new();
        map.insert(
            "private.test".to_string(),
            vec!["127.0.0.1:80".parse().unwrap()],
        );
        let transport = Transport::guarded(Arc::new(StaticResolver { map }));
        let err = fetch_with_transport(
            "http://private.test/",
            &limits(),
            &CancellationToken::new(),
            &transport,
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, WebFetchError::BlockedUrl(_)), "{err}");
        assert!(err.to_string().contains("non-public"), "{err}");
    }

    #[tokio::test]
    async fn cross_origin_redirect_is_blocked_and_target_never_contacted() {
        let a = TestServer::start().await;
        let b = TestServer::start().await;
        a.respond("/cross", test_server::redirect(302, &b.host_url("target")));
        b.respond("/target", test_server::ok("text/plain", b"secret"));
        let err = fetch_with_transport(
            &a.host_url("cross"),
            &limits(),
            &CancellationToken::new(),
            &server_transport(&a),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, WebFetchError::RedirectBlocked(_)), "{err}");
        assert!(err.to_string().contains("cross-origin"), "{err}");
        // The redirect discipline: prove the other origin was never contacted.
        assert_eq!(
            b.count("target"),
            0,
            "cross-origin target must not be contacted"
        );
        assert_eq!(a.count("cross"), 1);
    }

    #[tokio::test]
    async fn same_origin_redirect_chain_is_followed() {
        let server = TestServer::start().await;
        let base = format!("http://localhost:{}", server.addr.port());
        server.respond("/hop1", test_server::redirect(302, &format!("{base}/hop2")));
        server.respond("/hop2", test_server::redirect(302, &format!("{base}/hop3")));
        server.respond("/hop3", test_server::ok("text/plain", b"landed"));

        // Hostname URLs keep the resolver in the loop; count resolutions to
        // prove each redirect hop re-resolves (and is re-validated).
        #[derive(Clone)]
        struct FixedAddrResolver {
            addr: SocketAddr,
            calls: Arc<AtomicUsize>,
        }
        impl Resolve for FixedAddrResolver {
            fn resolve(&self, _name: reqwest::dns::Name) -> reqwest::dns::Resolving {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let addr = self.addr;
                Box::pin(async move {
                    let out = vec![addr];
                    Ok(Box::new(out.into_iter()) as reqwest::dns::Addrs)
                })
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let transport = Transport::unguarded(Arc::new(FixedAddrResolver {
            addr: server.addr,
            calls: Arc::clone(&calls),
        }));

        let res = fetch_with_transport(
            &format!("{base}/hop1"),
            &limits(),
            &CancellationToken::new(),
            &transport,
            None,
        )
        .await
        .expect("same-origin chain follows");
        assert_eq!(res.status, 200);
        assert!(res.final_url.ends_with("/hop3"), "{}", res.final_url);
        assert!(matches!(res.body, BodyKind::Text(ref t) if t == "landed"));
        assert!(
            calls.load(Ordering::SeqCst) >= 3,
            "each hop re-resolves: {}",
            calls.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn redirect_hop_cap_blocks_the_chain() {
        let server = TestServer::start().await;
        let base = format!("http://{SERVER_HOST}:{}", server.addr.port());
        for hop in 1..=6 {
            server.respond(
                &format!("/r{hop}"),
                test_server::redirect(302, &format!("{base}/r{}", hop + 1)),
            );
        }
        server.respond("/r7", test_server::ok("text/plain", b"never"));

        let err = fetch_with_transport(
            &server.host_url("r1"),
            &limits(),
            &CancellationToken::new(),
            &server_transport(&server),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, WebFetchError::RedirectBlocked(_)), "{err}");
        assert!(err.to_string().contains("hops"), "{err}");
        assert_eq!(server.count("r7"), 0, "beyond-cap hop never contacted");
    }

    #[tokio::test]
    async fn declared_body_over_byte_cap_is_too_large() {
        let server = TestServer::start().await;
        server.respond("/big", test_server::ok("text/plain", vec![b'x'; 4096]));
        let mut lim = limits();
        lim.max_response_bytes = 1024;
        let err = fetch_with_transport(
            &server.host_url("big"),
            &lim,
            &CancellationToken::new(),
            &server_transport(&server),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, WebFetchError::TooLarge { received, cap } if received >= 4096 && cap == 1024),
            "{err}"
        );
    }

    /// A body that outgrows the cap without declaring it is cut short and
    /// flagged: the caller keeps a bounded, usable prefix instead of nothing.
    #[tokio::test]
    async fn stream_over_byte_cap_is_truncated_and_flagged() {
        let server = TestServer::start().await;
        server.respond(
            "/undeclared",
            test_server::undeclared("text/plain", vec![b'x'; 4096]),
        );
        let mut lim = limits();
        lim.max_response_bytes = 1024;
        let res = fetch_with_transport(
            &server.host_url("undeclared"),
            &lim,
            &CancellationToken::new(),
            &server_transport(&server),
            None,
        )
        .await
        .expect("an undeclared over-cap body still fetches");
        assert!(res.truncated, "{res:?}");
        assert_eq!(res.total_bytes, 1024);
        assert!(matches!(res.body, BodyKind::Text(ref t) if t.len() == 1024));
        assert!(matches!(res.body, BodyKind::Text(_)));
    }

    #[tokio::test]
    async fn binary_and_missing_content_types_are_refused() {
        let server = TestServer::start().await;
        server.respond(
            "/png",
            test_server::ok("image/png", vec![0x89, 0x50, 0x4e, 0x47]),
        );
        server.respond(
            "/noct",
            ScriptedResponse {
                status: 200,
                headers: vec![],
                body: b"mystery bytes".to_vec(),
                hang: false,
                omit_content_length: false,
            },
        );
        for path in ["png", "noct"] {
            let err = fetch_with_transport(
                &server.host_url(path),
                &limits(),
                &CancellationToken::new(),
                &server_transport(&server),
                None,
            )
            .await
            .unwrap_err();
            assert!(
                matches!(err, WebFetchError::UnsupportedContentType(_)),
                "{path}: {err}"
            );
        }
    }

    #[tokio::test]
    async fn non_2xx_is_a_result_not_an_error() {
        let server = TestServer::start().await;
        server.respond(
            "/gone",
            test_server::ok("text/html", b"<html><body><h1>Nope</h1></body></html>")
                .with_status(404),
        );
        let res = fetch_with_transport(
            &server.host_url("gone"),
            &limits(),
            &CancellationToken::new(),
            &server_transport(&server),
            None,
        )
        .await
        .expect("404 is data");
        assert_eq!(res.status, 404);
        assert!(matches!(res.body, BodyKind::Html(_)));
    }

    #[tokio::test]
    async fn charset_is_decoded_from_content_type() {
        let (gbk_bytes, _, _) = encoding_rs::GBK.encode("你好世界");
        let server = TestServer::start().await;
        server.respond(
            "/gbk",
            test_server::ok("text/plain; charset=gbk", gbk_bytes.as_ref().to_vec()),
        );
        server.respond(
            "/unknown",
            test_server::ok("text/plain; charset=x-unknown", b"irrelevant".to_vec()),
        );
        server.respond(
            "/default",
            test_server::ok("text/plain", "héllo".as_bytes().to_vec()),
        );

        let res = fetch_with_transport(
            &server.host_url("gbk"),
            &limits(),
            &CancellationToken::new(),
            &server_transport(&server),
            None,
        )
        .await
        .expect("gbk decodes");
        assert!(matches!(res.body, BodyKind::Text(ref t) if t == "你好世界"));

        let err = fetch_with_transport(
            &server.host_url("unknown"),
            &limits(),
            &CancellationToken::new(),
            &server_transport(&server),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, WebFetchError::UnsupportedContentType(_))
                && err.to_string().contains("x-unknown"),
            "{err}"
        );

        let res = fetch_with_transport(
            &server.host_url("default"),
            &limits(),
            &CancellationToken::new(),
            &server_transport(&server),
            None,
        )
        .await
        .expect("utf-8 default");
        assert!(matches!(res.body, BodyKind::Text(ref t) if t == "héllo"));
    }

    #[tokio::test]
    async fn char_cap_truncates_with_flag() {
        let server = TestServer::start().await;
        server.respond("/long", test_server::ok("text/plain", vec![b'a'; 150_000]));
        let res = fetch_with_transport(
            &server.host_url("long"),
            &limits(),
            &CancellationToken::new(),
            &server_transport(&server),
            None,
        )
        .await
        .expect("long body fetches");
        assert!(res.truncated);
        assert!(matches!(res.body, BodyKind::Text(ref t) if t.chars().count() == 100_000));
    }

    #[tokio::test]
    async fn pre_cancelled_token_aborts() {
        let server = TestServer::start().await;
        server.respond("/x", test_server::ok("text/plain", b"hi"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = fetch_with_transport(
            &server.host_url("x"),
            &limits(),
            &cancel,
            &server_transport(&server),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, WebFetchError::Aborted), "{err}");
    }

    #[tokio::test]
    async fn unresponsive_server_times_out() {
        let server = TestServer::start().await;
        server.respond(
            "/hang",
            ScriptedResponse {
                status: 200,
                headers: vec![],
                body: vec![],
                hang: true,
                omit_content_length: false,
            },
        );
        let mut lim = limits();
        lim.timeout = Duration::from_millis(500);
        let err = fetch_with_transport(
            &server.host_url("hang"),
            &lim,
            &CancellationToken::new(),
            &server_transport(&server),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, WebFetchError::Timeout { .. }), "{err}");
    }

    #[test]
    fn classify_content_type_dispatch() {
        assert_eq!(classify_content_type("text/html"), ContentKind::Html);
        assert_eq!(
            classify_content_type("TEXT/HTML; charset=utf-8"),
            ContentKind::Html
        );
        assert_eq!(classify_content_type("Application/JSON"), ContentKind::Text);
        assert_eq!(
            classify_content_type("text/html; charset=utf-8"),
            ContentKind::Html
        );
        assert_eq!(
            classify_content_type("application/xhtml+xml"),
            ContentKind::Html
        );
        assert_eq!(classify_content_type("text/plain"), ContentKind::Text);
        assert_eq!(
            classify_content_type("application/json; charset=utf-8"),
            ContentKind::Text
        );
        assert_eq!(
            classify_content_type("application/vnd.api+json"),
            ContentKind::Text
        );
        assert_eq!(classify_content_type("image/png"), ContentKind::Binary);
        assert_eq!(
            classify_content_type("application/octet-stream"),
            ContentKind::Binary
        );
        assert_eq!(classify_content_type(""), ContentKind::Missing);
    }

    /// The IP-literal check does not depend on the DNS guard: a proxied fetch
    /// cannot validate resolved addresses, but a destination that is stated
    /// rather than resolved is still refused.
    #[tokio::test]
    async fn private_ip_literal_is_blocked_without_the_dns_guard() {
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]:8080/",
            "http://2130706433/",
        ] {
            let err = fetch_with_transport(
                url,
                &limits(),
                &CancellationToken::new(),
                &unguarded_transport(),
                None,
            )
            .await
            .unwrap_err();
            assert!(matches!(err, WebFetchError::BlockedUrl(_)), "{url}: {err}");
        }
    }

    /// Every redirect target re-earns the hygiene the request URL passed: a
    /// Location cannot smuggle in credentials or an over-long URL.
    #[tokio::test]
    async fn redirect_targets_re_earn_url_hygiene() {
        let server = TestServer::start().await;
        // Both targets are same-origin with the request URL, so URL hygiene is
        // the only thing that can refuse them.
        let credentialed = format!("http://user:pass@{SERVER_HOST}:{}/x", server.addr.port());
        let over_long = format!(
            "http://{SERVER_HOST}:{}/{}",
            server.addr.port(),
            "a".repeat(2_100)
        );
        server.respond("/credentialed", test_server::redirect(302, &credentialed));
        server.respond("/over-long", test_server::redirect(302, &over_long));
        server.respond("/x", test_server::ok("text/plain", b"never"));
        for (path, needle) in [("credentialed", "credentials"), ("over-long", "2048")] {
            let err = fetch_with_transport(
                &server.host_url(path),
                &limits(),
                &CancellationToken::new(),
                &server_transport(&server),
                None,
            )
            .await
            .unwrap_err();
            assert!(matches!(err, WebFetchError::InvalidUrl(_)), "{path}: {err}");
            assert!(err.to_string().contains(needle), "{path}: {err}");
            assert_eq!(
                server.count(path),
                1,
                "{path}: the redirect was the only request"
            );
            assert_eq!(
                server.count("x"),
                0,
                "the rejected target was never contacted"
            );
        }
    }

    /// The proxied leg: the proxy receives the absolute-form target URL (so it
    /// resolves the origin), while a destination that is a private literal is
    /// still refused before any request is sent.
    #[tokio::test]
    async fn proxied_hops_forward_absolute_urls_and_still_refuse_literals() {
        let server = TestServer::start().await;
        let target = server.host_url("through-proxy");
        server.respond(&target, test_server::ok("text/plain", b"proxied"));
        let proxy = Url::parse(&format!("http://{}", server.addr)).expect("proxy URL parses");

        let res = fetch_with_transport(
            &target,
            &limits(),
            &CancellationToken::new(),
            &unguarded_transport(),
            Some(proxy.clone()),
        )
        .await
        .expect("proxied fetch succeeds");
        assert!(
            matches!(res.body, BodyKind::Text(ref t) if t == "proxied"),
            "{res:?}"
        );
        assert_eq!(
            server.count(&target),
            1,
            "the proxy received the absolute-form request"
        );

        let err = fetch_with_transport(
            "http://169.254.169.254/latest/meta-data/",
            &limits(),
            &CancellationToken::new(),
            &unguarded_transport(),
            Some(proxy),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, WebFetchError::BlockedUrl(_)), "{err}");
    }

    /// Parameter names are case-insensitive and may be spaced, and the media
    /// type is folded before it is classified.
    #[tokio::test]
    async fn charset_parameter_tolerates_case_and_spacing() {
        let (gbk_bytes, _, _) = encoding_rs::GBK.encode("你好世界");
        let server = TestServer::start().await;
        server.respond(
            "/upper",
            test_server::ok("text/plain; Charset=gbk", gbk_bytes.as_ref().to_vec()),
        );
        server.respond(
            "/spaced",
            test_server::ok("TEXT/PLAIN; charset = \"gbk\"", gbk_bytes.as_ref().to_vec()),
        );
        for path in ["upper", "spaced"] {
            let res = fetch_with_transport(
                &server.host_url(path),
                &limits(),
                &CancellationToken::new(),
                &server_transport(&server),
                None,
            )
            .await
            .expect("decodes");
            assert!(
                matches!(res.body, BodyKind::Text(ref t) if t == "你好世界"),
                "{path}: {res:?}"
            );
        }
    }

    #[tokio::test]
    async fn requests_advertise_the_accepted_formats() {
        let server = TestServer::start().await;
        server.respond("/doc", test_server::ok("text/plain", b"ok"));
        fetch_with_transport(
            &server.host_url("doc"),
            &limits(),
            &CancellationToken::new(),
            &server_transport(&server),
            None,
        )
        .await
        .expect("fetches");
        let head = server
            .last_request_head("doc")
            .expect("request head recorded");
        let head = head.to_ascii_lowercase();
        assert!(head.contains("accept: text/html"), "{head}");
        assert!(head.contains("manox/"), "{head}");
    }
}
