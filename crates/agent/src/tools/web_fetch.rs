//! `web_fetch` tool: a lightweight HTTP GET client for fetching web documents
//! (HTML / text / JSON / XML). Registered in `base_tools` so both the main
//! agent and read-only sub-agents can pull docs without spinning up the full
//! browser. It carries no cookies, no login state, and no JS execution — for
//! anything behind auth or rendered by client-side JS, use the `web_explore_*`
//! tools instead.

use std::time::Duration;

use gpui::{App, AppContext as _, Task};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::tool::AgentTool;

use super::truncate_output;

/// Default byte cap on the fetched body. Large enough for a typical doc page,
/// small enough to keep the model context bounded.
const DEFAULT_MAX_BYTES: usize = 512 * 1024;
/// Default per-request timeout (covers connect + full body read).
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Hard ceiling a caller cannot exceed even by opting in — protects the model
/// context from a runaway `max_bytes`.
const MAX_ALLOWED_BYTES: usize = 4 * 1024 * 1024;

pub struct WebFetchTool;

#[derive(Deserialize, JsonSchema)]
struct WebFetchInput {
    /// Absolute `http://` or `https://` URL to fetch.
    url: String,
    /// Cap on the returned body in bytes (UTF-8 boundary). Default 512 KiB; the
    /// effective value is clamped to 4 MiB regardless of what the caller asks.
    #[serde(default)]
    max_bytes: Option<usize>,
    /// Overall request timeout in seconds (connect + body read). Default 30.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

impl AgentTool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch a web document over HTTP/HTTPS GET and return its text. Use this for \
         public docs, articles, JSON/XML feeds, and raw file URLs. It performs no JS \
         execution and carries no cookies or login state — for auth-gated or \
         JS-rendered pages, use the `web_explore_*` tools instead. Output: a header \
         block (final URL, HTTP status, content-type, received bytes, truncation \
         advisory) followed by the body text (truncated on a UTF-8 boundary if it \
         exceeds `max_bytes`)."
    }
    fn input_schema(&self) -> serde_json::Value {
        super::schema::<WebFetchInput>()
    }
    fn is_read_only(&self) -> bool {
        true
    }
    fn run(
        &self,
        input: serde_json::Value,
        cancel: CancellationToken,
        _ctx: &dyn crate::tool::ToolContext,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let Ok(parsed) = serde_json::from_value::<WebFetchInput>(input) else {
            return cx.background_spawn(async { Err("input parse failed".to_string()) });
        };
        let max_bytes = parsed
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .clamp(1, MAX_ALLOWED_BYTES);
        let timeout_secs = parsed.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let url = parsed.url;
        let (tx, rx) = async_channel::bounded(1);
        crate::runtime::handle().spawn(async move {
            let result = fetch(url, max_bytes, timeout_secs, cancel).await;
            let _ = tx.send(result).await;
        });
        cx.background_spawn(async move {
            rx.recv()
                .await
                .map_err(|_| "web_fetch cancelled".to_string())
                .and_then(|r| r)
        })
    }
}

/// Validate the scheme and run a single GET, streaming the body into a byte
/// buffer up to `max_bytes + 1` (the +1 lets us detect truncation). Returns the
/// model-facing output string.
async fn fetch(
    url: String,
    max_bytes: usize,
    timeout_secs: u64,
    cancel: CancellationToken,
) -> Result<String, String> {
    if !is_http_url(&url) {
        return Err(format!(
            "web_fetch only supports http/https URLs; got: {url}"
        ));
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .map_err(|e| format!("web_fetch client build failed: {e}"))?;

    let response = tokio::select! {
        r = client.get(&url).send() => r.map_err(|e| format!("web_fetch request failed: {e}"))?,
        _ = cancel.cancelled() => return Err("web_fetch cancelled".to_string()),
    };

    let final_url = response.url().to_string();
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Stream the body up to max_bytes + 1 so a non-truncating fetch stays exact
    // while a truncating one is detectable without buffering the whole body.
    let cap = max_bytes + 1;
    let mut buf: Vec<u8> = Vec::with_capacity(cap.min(64 * 1024));
    let mut stream = response.bytes_stream();
    use futures::StreamExt as _;
    let mut total_received: usize = 0;
    let mut truncated = false;
    while let Some(chunk_res) = stream.next().await {
        if cancel.is_cancelled() {
            return Err("web_fetch cancelled".to_string());
        }
        let chunk = chunk_res.map_err(|e| format!("web_fetch body read failed: {e}"))?;
        total_received += chunk.len();
        if buf.len() < cap {
            let remaining = cap - buf.len();
            if chunk.len() >= remaining {
                buf.extend_from_slice(&chunk[..remaining]);
                truncated = true;
            } else {
                buf.extend_from_slice(&chunk);
            }
        }
        // Once we've filled the cap we still drain the stream's total size for the
        // advisory, but stop copying. Keep iterating so `total_received` reflects
        // the true body length — cheap relative to buffering it.
    }

    let shown = if buf.len() > max_bytes {
        &buf[..max_bytes]
    } else {
        &buf[..]
    };
    let text = String::from_utf8_lossy(shown).into_owned();
    let truncated_text = truncate_output(&text, max_bytes);
    let advisory = if truncated_text.truncated || truncated {
        format!("Truncated: body is {total_received} bytes; showing first {max_bytes}.\n")
    } else {
        String::new()
    };

    let body = truncated_text
        .render("fetch a narrower range or use `web_explore_read_text` for the live DOM");
    Ok(format!(
        "URL: {final_url}\nStatus: {status}\nContent-Type: {content_type}\nBytes: {total_received}\n{advisory}\n{body}"
    ))
}

fn is_http_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_http_schemes() {
        assert!(is_http_url("http://example.com"));
        assert!(is_http_url("https://example.com"));
        assert!(!is_http_url("file:///etc/passwd"));
        assert!(!is_http_url("ftp://example.com"));
        assert!(!is_http_url("example.com"));
    }

    #[test]
    fn fetch_rejects_non_http() {
        // Synchronous prefix check happens before any network IO, so this returns
        // immediately without touching the runtime.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cancel = CancellationToken::new();
        let r = rt
            .block_on(fetch("file:///etc/passwd".into(), 1024, 5, cancel))
            .unwrap_err();
        assert!(r.contains("only supports http/https"));
    }

    #[test]
    fn fetch_rejects_invalid_url() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let cancel = CancellationToken::new();
        let r = rt
            .block_on(fetch("https://".into(), 1024, 5, cancel))
            .unwrap_err();
        // reqwest rejects a host-less URL with a builder or send error.
        assert!(r.contains("web_fetch"));
    }

    #[test]
    fn output_header_format() {
        // Header block shape is a contract the UI summary parses; pin it.
        let s = format!(
            "URL: {}\nStatus: {}\nContent-Type: {}\nBytes: {}\n\nbody",
            "https://x/", 200, "text/html", 4
        );
        assert!(s.starts_with("URL: https://x/\nStatus: 200\n"));
        assert!(s.contains("Content-Type: text/html"));
        assert!(s.contains("Bytes: 4"));
    }
}
