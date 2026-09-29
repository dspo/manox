//! Hand-rolled LSP JSON-RPC framing over a stdio byte stream.
//!
//! LSP messages are framed as:
//! ```text
//! Content-Length: <n>\r\n
//! [\r\n]
//! <n bytes of JSON>
//! ```
//! We read/write exactly this. No `async-lsp`/`tower-lsp`: the framer is ~80
//! lines and keeps the dependency surface minimal (manox forbids vendoring
//! third-party crate code; the smallest correct hand-roll beats a dependency).

use std::io;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// Read one framed message from the server's stdout. Returns `Ok(None)` on
/// clean EOF (server closed the stream); `Err` on a malformed frame.
pub async fn read_message<R>(reader: &mut BufReader<R>) -> io::Result<Option<Value>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut content_length: Option<usize> = None;
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader.read_until(b'\n', &mut line).await?;
        if n == 0 {
            // EOF before any header on this iteration — stream closed cleanly.
            return Ok(None);
        }
        // Strip the trailing CRLF / LF.
        let trimmed = std::str::from_utf8(&line)
            .unwrap_or("")
            .trim_matches(['\r', '\n']);
        if trimmed.is_empty() {
            // Blank line — end of headers, body follows.
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = match rest.trim().parse::<usize>() {
                Ok(n) => Some(n),
                Err(e) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("malformed Content-Length `{}`: {e}", rest.trim()),
                    ));
                }
            };
        }
        // Other headers (Content-Type, …) are ignored — JSON is the only body.
    }

    let len = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "frame missing Content-Length")
    })?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    let value = serde_json::from_slice(&body).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid JSON body: {e}"),
        )
    })?;
    Ok(Some(value))
}

/// Write one framed message to the server's stdin.
pub async fn write_message<W>(writer: &mut W, msg: &Value) -> io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let body = serde_json::to_vec(msg).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("JSON encode failed: {e}"),
        )
    })?;
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{BufReader, duplex};

    // Round-trip a JSON value through the framer in both directions.
    #[tokio::test]
    async fn round_trip_single_message() {
        let (mut client_tx, server_rx) = duplex(8192);
        let (mut server_tx, client_rx) = duplex(8192);

        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "initialize",
            "params": { "capabilities": {} }
        });
        write_message(&mut client_tx, &msg).await.unwrap();

        let mut reader = BufReader::new(server_rx);
        let got = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(got, msg);
        assert_eq!(got["method"], "initialize");

        // And back the other way.
        let resp = serde_json::json!({ "jsonrpc": "2.0", "id": 7, "result": { "ok": true } });
        write_message(&mut server_tx, &resp).await.unwrap();
        let mut reader2 = BufReader::new(client_rx);
        let got2 = read_message(&mut reader2).await.unwrap().unwrap();
        assert_eq!(got2["result"]["ok"], true);
    }

    // Two messages back-to-back must each frame independently — no leftover
    // bytes bleed into the next read.
    #[tokio::test]
    async fn round_trip_two_messages() {
        let (mut tx, rx) = duplex(8192);
        let mut reader = BufReader::new(rx);

        let a = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "a" });
        let b = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "longer-method-name",
            "params": { "x": 42 }
        });
        write_message(&mut tx, &a).await.unwrap();
        write_message(&mut tx, &b).await.unwrap();

        assert_eq!(read_message(&mut reader).await.unwrap().unwrap(), a);
        assert_eq!(read_message(&mut reader).await.unwrap().unwrap(), b);
    }

    // A frame carrying a Content-Type header (some servers send one) must still
    // parse — extra headers are skipped, Content-Length still drives the body.
    #[tokio::test]
    async fn ignores_content_type_header() {
        let (mut tx, rx) = duplex(8192);
        let body = br#"{"jsonrpc":"2.0","id":1}"#;
        let frame = format!(
            "Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        tx.write_all(frame.as_bytes()).await.unwrap();
        tx.write_all(body).await.unwrap();
        tx.flush().await.unwrap();

        let mut reader = BufReader::new(rx);
        let got = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(got["id"], 1);
    }

    // Clean EOF (empty stream) yields None, not an error. Note the `_`
    // placeholder (not `_tx`): a named binding keeps the peer's write half
    // alive, so the reader would never observe EOF and hang forever.
    #[tokio::test]
    async fn eof_yields_none() {
        let (_, rx) = duplex(8192);
        let mut reader = BufReader::new(rx);
        assert!(read_message(&mut reader).await.unwrap().is_none());
    }

    // A malformed Content-Length value is a hard frame error — not silently
    // swallowed and misreported as "missing".
    #[tokio::test]
    async fn malformed_content_length_errors() {
        let (mut tx, rx) = duplex(8192);
        let frame = b"Content-Length: not-a-number\r\n\r\n";
        tx.write_all(frame).await.unwrap();
        tx.flush().await.unwrap();
        let mut reader = BufReader::new(rx);
        let err = read_message(&mut reader).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().to_lowercase().contains("malformed"));
    }
}
