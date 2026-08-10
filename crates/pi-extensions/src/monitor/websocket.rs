//! WebSocket client for the Monitor tool.
//!
//! Ported from the retired `harness-manox/src/tools/websocket.rs`.
//! Validates URLs, resolves DNS, rejects private/loopback addresses,
//! connects, and streams text frames as events.

use futures::{SinkExt, StreamExt};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::{
    client::IntoClientRequest, http::Uri, protocol::WebSocketConfig,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;

/// Maximum frame size accepted (1 MiB).
const MAX_FRAME_SIZE: usize = 1_048_576;

/// Connection timeout per address attempt.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A single frame received from a WebSocket.
#[derive(Debug)]
pub enum WsFrame {
    /// Text frame content.
    Text(String),
    /// Binary frame with byte count (content is not forwarded).
    Binary { len: usize },
    /// Close frame with optional code and reason.
    Close {
        code: Option<u16>,
        reason: Option<String>,
    },
    /// The stream ended cleanly (server closed the TCP/TLS connection without
    /// a close frame). Distinct from an error so the monitor can report a
    /// normal completion.
    Ended,
}

/// Validate a WebSocket URL.
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
        return Err(format!(
            "unsupported scheme: {scheme} (expected ws:// or wss://)"
        ));
    }
    if uri
        .authority()
        .map(|a| a.as_str())
        .unwrap_or("")
        .contains('@')
    {
        return Err("WebSocket URL must not contain userinfo".into());
    }
    Ok(())
}

/// Validate subprotocol names: valid HTTP tokens, no duplicates.
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

fn is_valid_http_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Resolve a host:port pair and reject private/loopback/link-local/unspecified
/// addresses. Returns the resolved addresses.
pub async fn resolve_and_validate_addrs(
    host: &str,
    port: u16,
) -> Result<Vec<std::net::SocketAddr>, String> {
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
        .await
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
        if is_non_unicast_ip(&ip) {
            return Err(format!("rejected: {ip} is not a public unicast address"));
        }
        if is_shared_ipv4(&ip) {
            return Err(format!("rejected: {ip} is a shared CGNAT address"));
        }
        if let std::net::IpAddr::V6(v6) = ip
            && v6.to_ipv4_mapped().is_some()
        {
            return Err(format!("rejected: {ip} is an IPv4-mapped IPv6 address"));
        }
    }
    Ok(addrs)
}

fn is_private_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private(),
        std::net::IpAddr::V6(_) => false,
    }
}

fn is_link_local_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_link_local(),
        std::net::IpAddr::V6(v6) => v6.is_unicast_link_local(),
    }
}

fn is_non_unicast_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_multicast() || v4.is_broadcast(),
        std::net::IpAddr::V6(v6) => v6.is_multicast(),
    }
}

fn is_shared_ipv4(ip: &std::net::IpAddr) -> bool {
    if let std::net::IpAddr::V4(v4) = ip {
        let octets = v4.octets();
        octets[0] == 100 && (octets[1] & 0b11000000) == 0b01000000
    } else {
        false
    }
}

/// Connect to a WebSocket server, iterating resolved addresses in order.
/// Returns the connected stream or an error if all addresses fail.
pub async fn connect_pinned(
    url: &str,
    addrs: &[std::net::SocketAddr],
    cancel: CancellationToken,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, String> {
    let req = url
        .to_owned()
        .into_client_request()
        .map_err(|e| format!("invalid request: {e}"))?;

    let ws_config = WebSocketConfig {
        max_frame_size: Some(MAX_FRAME_SIZE),
        ..Default::default()
    };
    let mut last_err = None;

    for &addr in addrs {
        if cancel.is_cancelled() {
            return Err("WebSocket connection cancelled".into());
        }

        let connect_fut = async {
            let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
                .await
                .map_err(|e| format!("TCP connect timeout: {e}"))?
                .map_err(|e| format!("TCP connect failed: {e}"))?;

            let is_tls = url.starts_with("wss://");
            let ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>> = if is_tls {
                let tls = tokio_native_tls::TlsConnector::from(
                    native_tls::TlsConnector::builder()
                        .build()
                        .map_err(|e| format!("TLS builder failed: {e}"))?,
                );
                let domain = req.uri().host().unwrap_or("localhost");
                let tls_stream = tls
                    .connect(domain, stream)
                    .await
                    .map_err(|e| format!("TLS handshake failed: {e}"))?;
                let tls = MaybeTlsStream::NativeTls(tls_stream);
                let (stream, _) =
                    tokio_tungstenite::client_async_with_config(req.clone(), tls, Some(ws_config))
                        .await
                        .map_err(|e| format!("WebSocket handshake failed: {e}"))?;
                stream
            } else {
                let plain = MaybeTlsStream::Plain(stream);
                let (stream, _) = tokio_tungstenite::client_async_with_config(
                    req.clone(),
                    plain,
                    Some(ws_config),
                )
                .await
                .map_err(|e| format!("WebSocket handshake failed: {e}"))?;
                stream
            };
            Ok::<_, String>(ws_stream)
        };

        let remaining = if addrs.len() > 1 {
            Some(CONNECT_TIMEOUT)
        } else {
            None
        };

        match remaining {
            Some(t) => tokio::select! {
                _ = cancel.cancelled() => return Err("WebSocket connection cancelled".into()),
                r = tokio::time::timeout(t, connect_fut) => match r {
                    Ok(Ok(stream)) => return Ok(stream),
                    Ok(Err(e)) => { last_err = Some(e); continue; }
                    Err(_) => { last_err = Some("connection timeout".into()); continue; }
                }
            },
            None => tokio::select! {
                _ = cancel.cancelled() => return Err("WebSocket connection cancelled".into()),
                r = connect_fut => match r {
                    Ok(stream) => return Ok(stream),
                    Err(e) => { last_err = Some(e); continue; }
                },
            },
        }
    }

    Err(last_err.unwrap_or_else(|| "no addresses to connect to".into()))
}

/// Read the next frame from the WebSocket.
pub async fn read_frame(
    stream: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
) -> Result<WsFrame, String> {
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let Some(msg) = stream.next().await else {
            return Ok(WsFrame::Ended);
        };
        let msg = msg.map_err(|e| format!("WebSocket error: {e}"))?;
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
                if data.len() > MAX_FRAME_SIZE {
                    return Err(format!(
                        "binary frame exceeds 1 MiB limit ({len} bytes)",
                        len = data.len()
                    ));
                }
                return Ok(WsFrame::Binary { len: data.len() });
            }
            Message::Close(frame) => {
                let code = frame.as_ref().map(|f| u16::from(f.code));
                let reason = frame.as_ref().map(|f| f.reason.to_string());
                return Ok(WsFrame::Close { code, reason });
            }
            Message::Ping(data) => {
                let _ = stream.send(Message::Pong(data)).await;
            }
            Message::Pong(_) => {}
            Message::Frame(_) => {}
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

    #[tokio::test]
    async fn resolves_and_rejects_loopback() {
        let err = resolve_and_validate_addrs("127.0.0.1", 80)
            .await
            .expect_err("loopback must be rejected");
        assert!(err.contains("loopback"));
    }

    #[tokio::test]
    async fn resolves_and_rejects_shared_and_non_unicast() {
        let cgnat = resolve_and_validate_addrs("100.64.0.1", 80)
            .await
            .expect_err("CGNAT must be rejected");
        assert!(cgnat.contains("CGNAT"));

        let multicast = resolve_and_validate_addrs("224.0.0.1", 80)
            .await
            .expect_err("multicast must be rejected");
        assert!(multicast.contains("public unicast"));
    }
}
