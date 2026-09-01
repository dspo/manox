//! `web_fetch` tool: a lightweight HTTP GET client for fetching web documents
//! (HTML / text / JSON / XML). Ported from the retired manox harness to the
//! pi path as a host-layer tool (manox-original, no TS counterpart). It
//! carries no cookies, no login state, and no JS execution — for anything
//! behind auth or rendered by client-side JS, use the browser tools
//! (`ChromeUse*`, when enabled).
//!
//! HTML bodies are converted to readable text (html2text); non-2xx responses
//! surface as errors with a body summary instead of masquerading as content;
//! binary content types are rejected rather than fed to the context as
//! mojibake. Proxies come from the standard `HTTP_PROXY`/`HTTPS_PROXY`/
//! `ALL_PROXY` environment variables — the in-repo `http_proxy` crate is a
//! sandbox-side component (an allowlist proxy for seatbelt-confined child
//! processes) and does not apply to this unsandboxed agent-process tool.

use std::time::Duration;

use pi::tool::{AgentTool, AgentToolResult, ToolContext, ToolError};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

/// Default byte cap on the fetched body. Large enough for a typical doc page,
/// small enough to keep the model context bounded.
const DEFAULT_MAX_BYTES: usize = 512 * 1024;
/// Default per-request timeout (covers connect + full body read).
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Hard ceiling a caller cannot exceed even by opting in — protects the model
/// context from a runaway `max_bytes`.
const MAX_ALLOWED_BYTES: usize = 4 * 1024 * 1024;
/// Default timeout for the optional model extraction pass.
const DEFAULT_EXTRACT_TIMEOUT_SECS: u64 = 60;
/// Layout width for HTML→text conversion; wide enough to avoid mangling code
/// blocks in doc pages.
const HTML_TEXT_WIDTH: usize = 120;

pub struct WebFetchTool;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WebFetchInput {
    /// Absolute `http://` or `https://` URL to fetch.
    url: String,
    /// Cap on the returned body in bytes (UTF-8 boundary). Default 512 KiB;
    /// the effective value is clamped to 4 MiB regardless of what the caller
    /// asks.
    #[serde(default)]
    max_bytes: Option<usize>,
    /// Overall request timeout in seconds (connect + body read). Default 30.
    #[serde(default)]
    timeout_secs: Option<u64>,
    /// Optional extraction request: when given, the (converted) body is sent
    /// to the session's model for a one-shot answer instead of being returned
    /// verbatim — saves context on large pages. Adds a bounded model call
    /// (default timeout 60s).
    #[serde(default)]
    prompt: Option<String>,
}

impl WebFetchTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl AgentTool for WebFetchTool {
    fn name(&self) -> &str {
        crate::tools::WEB_FETCH
    }

    fn description(&self) -> &str {
        "Fetch a web document over HTTP/HTTPS GET and return readable text. \
         HTML pages are converted to text automatically; pass `prompt` to have \
         the fetched content distilled into a one-shot answer instead of \
         reading the full body. No JS execution, no cookies or login state — \
         for JS-rendered or auth-gated pages use the browser tools \
         (ChromeUse*, when enabled). Output: a header block (final URL, HTTP \
         status, content-type, received bytes, truncation advisory) followed \
         by the body text (or the extraction answer)."
    }

    fn is_read_only(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "Absolute http(s) URL" },
                "max_bytes": { "type": "integer", "description": "Body cap in bytes (default 512 KiB, clamped to 4 MiB)" },
                "timeout_secs": { "type": "integer", "description": "Request timeout in seconds (default 30)" },
                "prompt": { "type": "string", "description": "Optional extraction request: the model answers it from the fetched content instead of the full body being returned" }
            },
            "required": ["url"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        _signal: CancellationToken,
        _ctx: &dyn ToolContext,
    ) -> Result<AgentToolResult, ToolError> {
        let input: WebFetchInput = serde_json::from_value(params)
            .map_err(|e| ToolError::InvalidArguments(format!("invalid WebFetch arguments: {e}")))?;
        let max_bytes = input
            .max_bytes
            .unwrap_or(DEFAULT_MAX_BYTES)
            .min(MAX_ALLOWED_BYTES);
        let timeout_secs = input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
        let cancel = CancellationToken::new();
        let header_and_body = fetch(input.url.clone(), max_bytes, timeout_secs, cancel)
            .await
            .map_err(ToolError::ExecutionFailed)?;
        match input.prompt.as_deref() {
            None => Ok(AgentToolResult::text(header_and_body)),
            Some(prompt) => {
                let answer = extract(&header_and_body, prompt, DEFAULT_EXTRACT_TIMEOUT_SECS).await;
                match answer {
                    Ok(text) => Ok(AgentToolResult::text(text)),
                    // The FETCH succeeded; only the distillation failed. Fall
                    // back to the raw body so the call still lands the
                    // content, with the failure noted for retry.
                    Err(err) => Ok(AgentToolResult::text(format!(
                        "[extraction failed: {err} — returning raw content]\n\n{header_and_body}"
                    ))),
                }
            }
        }
    }
}

/// How to treat a response body based on its content type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentKind {
    /// HTML → convert to readable text.
    Html,
    /// Plain textual → pass through verbatim.
    Text,
    /// Anything else (images, audio, octet-stream, …) → refuse.
    Binary,
}

fn classify_content_type(content_type: &str) -> ContentKind {
    // Strip parameters (`text/html; charset=utf-8` → `text/html`).
    let mime = content_type.split(';').next().unwrap_or("").trim();
    match mime {
        "text/html" | "application/xhtml+xml" => ContentKind::Html,
        "" => ContentKind::Text, // header absent: assume textual, lossy at worst
        _ if mime.starts_with("text/") => ContentKind::Text,
        _ if mime.starts_with("application/json") => ContentKind::Text,
        _ if mime.starts_with("application/xml") => ContentKind::Text,
        _ if mime.starts_with("application/yaml") => ContentKind::Text,
        _ if mime.starts_with("application/javascript") => ContentKind::Text,
        _ if mime.ends_with("+json") || mime.ends_with("+xml") => ContentKind::Text,
        _ => ContentKind::Binary,
    }
}

/// Build the request client: bounded redirects, explicit timeout, a product
/// User-Agent (many doc sites 403 the default reqwest UA), and standard
/// environment-proxy support (`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY`).
fn build_client(timeout_secs: u64) -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent(format!("manox/{}", env!("CARGO_PKG_VERSION")));
    for var in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if let Ok(proxy) = std::env::var(var)
            && let Ok(parsed) = reqwest::Proxy::all(&proxy)
        {
            builder = builder.proxy(parsed);
            break;
        }
    }
    builder
        .build()
        .map_err(|e| format!("WebFetch client build failed: {e}"))
}

/// Validate the scheme and run a single GET, streaming the body into a byte
/// buffer up to `max_bytes + 1` (the +1 lets us detect truncation). Returns
/// the model-facing output string.
async fn fetch(
    url: String,
    max_bytes: usize,
    timeout_secs: u64,
    cancel: CancellationToken,
) -> Result<String, String> {
    if !is_http_url(&url) {
        return Err(format!(
            "WebFetch only supports http/https URLs; got: {url}"
        ));
    }
    let client = build_client(timeout_secs)?;

    let response = tokio::select! {
        r = client.get(&url).send() => r.map_err(|e| format!("WebFetch request failed: {e}"))?,
        _ = cancel.cancelled() => return Err("WebFetch cancelled".to_string()),
    };

    let final_url = response.url().to_string();
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Stream the body up to max_bytes + 1 so a non-truncating fetch stays
    // exact while a truncating one is detectable without buffering the whole
    // body.
    let cap = max_bytes + 1;
    let mut buf: Vec<u8> = Vec::with_capacity(cap.min(64 * 1024));
    let mut stream = response.bytes_stream();
    use futures::StreamExt as _;
    let mut total_received: usize = 0;
    let mut truncated = false;
    while let Some(chunk_res) = stream.next().await {
        if cancel.is_cancelled() {
            return Err("WebFetch cancelled".to_string());
        }
        let chunk = chunk_res.map_err(|e| format!("WebFetch body read failed: {e}"))?;
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
        // Keep draining so `total_received` reflects the true body length for
        // the advisory, but stop copying once the cap is filled.
    }

    let shown = if buf.len() > max_bytes {
        &buf[..max_bytes]
    } else {
        &buf[..]
    };

    // Non-2xx is an error, not content: a 404 HTML page must not read as a
    // successful body. A short summary keeps the failure actionable.
    if !status.is_success() {
        let summary = String::from_utf8_lossy(&shown[..shown.len().min(512)]);
        return Err(format!(
            "WebFetch got HTTP {} for {final_url} (content-type {content_type}): {}",
            status.as_u16(),
            summary.chars().take(300).collect::<String>()
        ));
    }

    let advisory = truncation_advisory(truncated, total_received, max_bytes);
    match classify_content_type(&content_type) {
        ContentKind::Binary => Err(format!(
            "WebFetch refuses binary content (content-type {content_type}); \
             fetch it via Bash to a file or use the browser tools"
        )),
        ContentKind::Html => {
            let text = html2text::from_read(shown, HTML_TEXT_WIDTH)
                .map_err(|e| format!("HTML conversion failed: {e}"))?;
            Ok(format!(
                "URL: {final_url}\nStatus: {}\nContent-Type: {content_type}\nBytes: {total_received}\n{advisory}\n{text}",
                status.as_u16()
            ))
        }
        ContentKind::Text => {
            let text = String::from_utf8_lossy(shown).into_owned();
            Ok(format!(
                "URL: {final_url}\nStatus: {}\nContent-Type: {content_type}\nBytes: {total_received}\n{advisory}\n{text}",
                status.as_u16()
            ))
        }
    }
}

fn truncation_advisory(truncated: bool, total_received: usize, max_bytes: usize) -> String {
    if truncated || total_received > max_bytes {
        format!("Truncated: body is {total_received} bytes; showing first {max_bytes}.\n")
    } else {
        String::new()
    }
}

/// One-shot model extraction over the fetched content: the same bare-model
/// mechanism the VS Code LanguageModelChat provider rides (`model_chat`),
/// driven directly here — the agent crate cannot depend on
/// manox-session-core without inverting the crate layering. Uses the
/// settings default model; bounded by `timeout_secs`; empty answers and
/// provider errors surface verbatim.
async fn extract(content: &str, prompt: &str, timeout_secs: u64) -> Result<String, String> {
    use pi::agent_loop::StreamFn;
    use pi::types::{AgentContext, AgentEvent, AssistantMessageEvent};

    let registry = crate::provider_glue::global();
    let model = crate::provider_glue::default_model()
        .ok_or_else(|| "no default model configured".to_string())?;
    let stream: std::sync::Arc<dyn StreamFn> = registry
        .resolve_stream(&model)
        .map_err(|e| format!("stream resolution failed: {e}"))?;

    let ctx = AgentContext {
        system_prompt: "You extract information from fetched web content. \
                        Answer the user's request using ONLY the provided content. \
                        If the content does not contain the answer, say so."
            .to_string(),
        messages: vec![pi::types::AgentMessage::User {
            content: vec![pi::types::ContentBlock::Text {
                text: format!("Fetched content:\n\n{content}\n\nRequest: {prompt}"),
                signature: None,
            }],
            timestamp: chrono::Utc::now(),
        }],
        tools: vec![].into(),
        model: model.clone(),
        thinking_level: None,
        cache_retention: Default::default(),
        session_id: None,
        stream_options: pi::types::StreamOptions::default(),
        metadata: Default::default(),
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<AgentEvent>(256);
    let token = CancellationToken::new();
    let mut stream_fut = stream.stream(&ctx, token.clone(), tx);
    let mut text = String::new();
    let outcome = loop {
        tokio::select! {
            biased;
            ev = rx.recv() => {
                match ev {
                    Some(AgentEvent::MessageUpdate {
                        assistant_message_event: AssistantMessageEvent::TextDelta { delta, .. },
                        ..
                    }) => text.push_str(&delta),
                    // Channel closed: only the stream future remains.
                    None => break Ok(()),
                    _ => {}
                }
            }
            res = &mut stream_fut => {
                match res {
                    Ok(_) => break Ok(()),
                    Err(e) => break Err(format!("extraction failed: {e}")),
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
                token.cancel();
                return Err(format!("extraction timed out after {timeout_secs}s"));
            }
        }
    };
    // Deltas that raced the stream future's completion still count.
    while let Ok(ev) = rx.try_recv() {
        if let AgentEvent::MessageUpdate {
            assistant_message_event,
            ..
        } = ev
            && let AssistantMessageEvent::TextDelta { delta, .. } = assistant_message_event
        {
            text.push_str(&delta);
        }
    }
    outcome?;
    if text.trim().is_empty() {
        return Err("extraction produced an empty answer".to_string());
    }
    Ok(text)
}

fn is_http_url(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_content_type_dispatch() {
        assert_eq!(classify_content_type("text/html"), ContentKind::Html);
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
        assert_eq!(classify_content_type(""), ContentKind::Text);
    }

    #[test]
    fn html_conversion_strips_markup() {
        let html =
            b"<html><head><title>T</title></head><body><h1>Hello</h1><p>World</p></body></html>";
        let text = html2text::from_read(&html[..], HTML_TEXT_WIDTH).expect("converts");
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("World"), "{text}");
        assert!(!text.contains("<h1>"), "{text}");
    }

    #[tokio::test]
    async fn fetch_rejects_non_http() {
        let err = fetch(
            "ftp://example.com".into(),
            1024,
            5,
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("only supports http/https"), "{err}");
    }

    #[tokio::test]
    async fn fetch_rejects_invalid_url() {
        let err = fetch("not a url".into(), 1024, 5, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(err.contains("only supports http/https"), "{err}");
    }

    #[test]
    fn truncation_advisory_shapes() {
        assert!(truncation_advisory(true, 100, 50).contains("Truncated"));
        assert!(truncation_advisory(false, 100, 50).contains("Truncated"));
        assert!(truncation_advisory(false, 50, 50).is_empty());
    }

    /// Live round-trip, opt-in only (`MANOX_RUN_LIVE=1`): converts a real
    /// HTML page and asserts markup is gone.
    #[tokio::test]
    async fn live_fetch_converts_html() {
        if std::env::var("MANOX_RUN_LIVE").ok().as_deref() != Some("1") {
            eprintln!("skipping live web_fetch test (MANOX_RUN_LIVE != 1)");
            return;
        }
        let out = fetch(
            "https://example.com".into(),
            DEFAULT_MAX_BYTES,
            DEFAULT_TIMEOUT_SECS,
            CancellationToken::new(),
        )
        .await
        .expect("live fetch");
        assert!(out.contains("Status: 200"), "{out}");
        assert!(out.contains("Example Domain"), "{out}");
        assert!(!out.contains("<html"), "{out}");
    }
}
