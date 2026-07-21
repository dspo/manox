//! WebSocket client for the Monitor tool. Validates URLs, resolves DNS,
//! rejects private/loopback addresses, connects, and streams text frames
//! as events.

use std::net::ToSocketAddrs;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::http::{Request, Uri};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// Maximum frame size accepted (1 MiB). Frames larger than this are rejected
/// with a size-placeholder event.
const MAX_FRAME_SIZE: usize = 1_048_576;

/// Validated WebSocket connection parameters.
#[derive(Debug, Clone)]
pub struct WsTarget {
    pub url: String,
    pub protocols: Vec<String>,
}

/// A single frame received from a WebSocket.
#[derive(Debug)]
pub enum WsFrame {
    /// Text frame content.
    Text(String),
    /// Binary frame with byte count (content is not forwarded).
    Binary { len: usize },
    /// Close frame with optional code and reason.
    Close { code: Option<u16>, reason: Option<String> },
}

/// Validate a WebSocket URL: must be `ws://` or `wss://`, no userinfo,
/// no whitespace, pure ASCII.
pub fn validate_ws_url(url: &str) -> Result<(), String> {
    if !url.is_ascii() {
        return Err("WebSocket URL must be ASCII only".into());
    }
    if url.contains(char::is_whitespace) {
        return Err("WebSocket URL must not contain whitespace".into());
    }
    let uri: Uri = url
        .parse()
        .map_err(|e| format!("invalid WebSocket URL: {e}"))?;
    let scheme = uri
        .scheme_str()
        .ok_or("WebSocket URL must have a scheme (ws:// or wss://)")?;
    if scheme != "ws" && scheme != "wss" {
        return Err(format!("unsupported scheme: {scheme} (expected ws:// or wss://)"));
    }
    if uri.authority().map(|a| a.as_str()).unwrap_or("").contains('@') {
        return Err("WebSocket URL must not contain userinfo".into());
    }
    Ok(())
}

/// Validate subprotocol names: must be valid HTTP tokens (RFC 6455 §4.1),
/// no duplicates.
pub fn validate_protocols(protocols: &[String]) -> Result<(), String> {
    if protocols.is_empty() {
        return Ok(());
    }
    let mut seen = std::collections::HashSet::new();
    for p in protocols {
        if p.is_empty() {
            return Err("subprotocol name must not be empty".into());
        }
        if !is_valid_http_token(p) {
            return Err(format!("invalid subprotocol name: {p}"));
        }
        if !seen.insert(p) {
            return Err(format!("duplicate subprotocol: {p}"));
        }
    }
    Ok(())
}

/// An HTTP token (RFC 2616 §2.2) is one or more characters from the set
/// `!#$%&'*+-.^_`|~``, digits, or letters.
fn is_valid_http_token(s: &str) -> bool {
    s.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(b, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*'
                | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
    })
}

/// Resolve a host:port pair and reject private/loopback/link-local/unspecified
/// addresses. Returns the resolved addresses so the caller can pin them and
/// avoid DNS rebinding.
pub fn resolve_and_validate_addrs(host: &str, port: u16) -> Result<Vec<std::net::SocketAddr>, String> {
    let addr_str = format!("{host}:{port}");
    let addrs: Vec<std::net::SocketAddr> = addr_str
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolution failed for {host}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("no addresses resolved for {host}"));
    }
    for addr in &addrs {
        let ip = addr.ip();
        if ip.is_loopback() {
            return Err(format!("rejected: {ip} is a loopback address"));
        }
        if is_private_ip(&ip) {
            return Err(format!("rejected: {ip} is a private address"));
        }
        if is_link_local_ip(&ip) {
            return Err(format!("rejected: {ip} is a link-local address"));
        }
        if ip.is_unspecified() {
            return Err(format!("rejected: {ip} is an unspecified address"));
        }
        // Reject metadata addresses (IPv4-mapped IPv6 etc.)
        if let std::net::IpAddr::V6(v6) = ip
            && v6.to_ipv4_mapped().is_some()
        {
            return Err(format!("rejected: {ip} is an IPv4-mapped IPv6 address"));
        }
    }
    Ok(addrs)
}

/// Check if an IP address is in a private range (RFC 1918, RFC 4193).
fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let octets = v4.octets();
            // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
            octets[0] == 10
                || (octets[0] == 172 && octets[1] >= 16 && octets[1] <= 31)
                || (octets[0] == 192 && octets[1] == 168)
        }
        std::net::IpAddr::V6(v6) => {
            // fc00::/7 (unique local)
            (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Check if an IP address is link-local.
fn is_link_local_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            // 169.254.0.0/16
            let octets = v4.octets();
            octets[0] == 169 && octets[1] == 254
        }
        std::net::IpAddr::V6(v6) => {
            // fe80::/10
            (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Connect to the WebSocket target using the pinned addresses (DNS rebinding
/// protection). Returns the stream handle.
pub async fn connect_pinned(
    target: &WsTarget,
    pinned_addrs: &[std::net::SocketAddr],
    timeout: Option<Duration>,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String> {
    let uri: Uri = target
        .url
        .parse()
        .map_err(|e| format!("invalid URL: {e}"))?;
    let host = uri
        .host()
        .ok_or("URL has no host")?
        .to_string();
    let port = uri.port_u16().unwrap_or(match uri.scheme_str() {
        Some("wss") => 443,
        _ => 80,
    });

    // Build the request with subprotocols.
    let mut req = Request::builder()
        .uri(&target.url)
        .header("Host", format!("{host}:{port}"));
    if !target.protocols.is_empty() {
        req = req.header(
            "Sec-WebSocket-Protocol",
            target.protocols.join(", "),
        );
    }
    let req = req
        .body(())
        .map_err(|e| format!("failed to build request: {e}"))?;

    // Connect to the first pinned address that works.
    let mut last_err = None;
    for addr in pinned_addrs {
        let connect_fut = async {
            let stream = match timeout {
                Some(t) => tokio::time::timeout(t, TcpStream::connect(*addr)).await
                    .map_err(|_| "connection timeout".to_string())?
                    .map_err(|e| format!("TCP connect failed: {e}"))?,
                None => TcpStream::connect(*addr).await
                    .map_err(|e| format!("TCP connect failed: {e}"))?,
            };
            let scheme = target.url.starts_with("wss://");
            let ws_stream = if scheme {
                let (stream, _) = tokio_tungstenite::client_async_tls(req.clone(), stream)
                    .await
                    .map_err(|e| format!("TLS handshake failed: {e}"))?;
                stream
            } else {
                let plain = tokio_tungstenite::MaybeTlsStream::Plain(stream);
                let (stream, _) = tokio_tungstenite::client_async(req.clone(), plain)
                    .await
                    .map_err(|e| format!("WebSocket handshake failed: {e}"))?;
                stream
            };
            Ok::<_, String>(ws_stream)
        };
        match connect_fut.await {
            Ok(stream) => return Ok(stream),
            Err(e) => {
                last_err = Some(e);
                continue;
            }
        }
    }
    Err(last_err.unwrap_or_else(|| "no addresses to connect to".into()))
}

/// Read the next frame from the WebSocket. Text frames up to `MAX_FRAME_SIZE`
/// are returned as `WsFrame::Text`; binary frames are returned as
/// `WsFrame::Binary` with a byte count; frames exceeding the size limit are
/// rejected with an error.
pub async fn read_frame(
    stream: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> Result<WsFrame, String> {
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let msg = stream
            .next()
            .await
            .ok_or("WebSocket stream ended")?
            .map_err(|e| format!("WebSocket error: {e}"))?;
        match msg {
            Message::Text(text) => {
                if text.len() > MAX_FRAME_SIZE {
                    return Err(format!(
                        "text frame exceeds 1 MiB limit ({len} bytes)",
                        len = text.len()
                    ));
                }
                return Ok(WsFrame::Text(text));
            }
            Message::Binary(data) => {
                return Ok(WsFrame::Binary { len: data.len() });
            }
            Message::Close(frame) => {
                let code = frame.as_ref().map(|f| u16::from(f.code));
                let reason = frame.as_ref().map(|f| f.reason.to_string());
                return Ok(WsFrame::Close { code, reason });
            }
            Message::Ping(data) => {
                // Auto-respond to pings.
                let _ = stream
                    .send(tokio_tungstenite::tungstenite::Message::Pong(data))
                    .await;
            }
            Message::Pong(_) => {}
            Message::Frame(_) => {
                // Raw frames are not handled.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_ws_url() {
        assert!(validate_ws_url("ws://example.com/ws").is_ok());
        assert!(validate_ws_url("wss://example.com/path").is_ok());
        assert!(validate_ws_url("http://example.com").is_err());
        assert!(validate_ws_url("ws://user:pass@example.com").is_err());
        assert!(validate_ws_url("ws://example.com/path with spaces").is_err());
    }

    #[test]
    fn validates_protocols() {
        assert!(validate_protocols(&[]).is_ok());
        assert!(validate_protocols(&["v12.stomp".into()]).is_ok());
        assert!(validate_protocols(&["soap".into(), "wamp".into()]).is_ok());
        assert!(validate_protocols(&["soap".into(), "soap".into()]).is_err());
        assert!(validate_protocols(&[String::new()]).is_err());
        assert!(validate_protocols(&["bad protocol".into()]).is_err());
    }

    #[test]
    fn validates_http_token() {
        assert!(is_valid_http_token("v12.stomp"));
        assert!(is_valid_http_token("soap"));
        assert!(!is_valid_http_token("bad token"));
        assert!(!is_valid_http_token(""));
    }
}