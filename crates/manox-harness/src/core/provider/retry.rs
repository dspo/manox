//! Transient-failure retry for the LLM HTTP handshake.
//!
//! Every wire sends a streaming POST; a non-2xx status — most painfully 429 —
//! or a connect-phase transport failure is usually transient. This module
//! wraps the send in an exponential-backoff retry loop so those recover
//! silently, emitting [`AgentEvent::Retry`] between attempts; only after
//! `MAX_ATTEMPTS` does the classified error reach the caller.
//!
//! Safety boundary: retry happens only at the handshake stage, before any SSE
//! event has been forwarded. A stream that fails mid-flight is never retried
//! — re-sending would duplicate output already emitted. Each attempt sends a
//! byte-identical body, so provider-side prefix caching is unaffected.

use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::core::provider::{ProviderError, RequestObserver, overflow};
use crate::types::AgentEvent;

/// Total request budget per stream call, including the original attempt.
const MAX_ATTEMPTS: u32 = 6;
const BASE_DELAY: Duration = Duration::from_secs(1);
const BACKOFF_FACTOR: f64 = 2.0;
const MAX_DELAY: Duration = Duration::from_secs(30);
/// Upper bound on a server-advertised `Retry-After`, so a misbehaving upstream
/// cannot stall a turn indefinitely.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// HTTP statuses whose failure is likely to resolve on retry. The unofficial
/// 529 ("service overloaded") is included — Anthropic emits it in practice.
/// 520–524 are Cloudflare gateway errors common to provider front-ends.
/// Whether an HTTP status is transient (rate limit, timeout, gateway
/// errors). Auth (401/403), invalid requests (400/404), and quota/billing
/// (429 with an error body the API marks terminal) are not — those are
/// deterministic and must not be retried. The explicit allowlist, rather
/// than a blanket >=500, keeps deterministic codes (501, 511) from burning
/// the retry budget.
pub fn is_retryable_status(status: u16) -> bool {
    matches!(
        status,
        408 | 429 | 500 | 502 | 503 | 504 | 520 | 521 | 522 | 523 | 524 | 529
    )
}

/// reqwest send errors worth retrying. A failure that never produced an HTTP
/// status is a transport-class error and the request can be re-sent; this
/// covers connect/timeout failures and the generic request-phase errors
/// reqwest renders as `error sending request for url (...)` — e.g. a
/// connection dropped before the response arrived ("connection closed before
/// message completed") — whose inner cause carries no io kind. Redirect
/// loops and client-construction bugs reproduce identically and never retry.
fn is_retryable_send_error(err: &reqwest::Error) -> bool {
    !err.is_status() && !err.is_redirect() && !err.is_builder()
}

/// Short, user-facing label for a retryable reqwest send error. Mirrors the
/// retry-decision logic of `is_retryable_send_error` but only classifies — it
/// never gates a retry. Kept terse so the retry event reads as one line.
fn classify_send_error(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        return "timeout";
    }
    if err.is_redirect() {
        return "redirect error";
    }
    if err.is_connect() {
        return "connection error";
    }
    let mut src: Option<&dyn std::error::Error> = Some(err);
    while let Some(s) = src {
        if let Some(io) = s.downcast_ref::<std::io::Error>() {
            match io.kind() {
                std::io::ErrorKind::ConnectionReset => return "connection reset",
                std::io::ErrorKind::ConnectionAborted => return "connection aborted",
                std::io::ErrorKind::BrokenPipe => return "broken pipe",
                std::io::ErrorKind::TimedOut => return "timeout",
                std::io::ErrorKind::UnexpectedEof => return "unexpected EOF",
                _ => {}
            }
        }
        src = s.source();
    }
    "network error"
}

/// User-facing label for a retryable HTTP status: "429 Too Many Requests" for
/// standard codes, bare numeric for unofficial ones (529) where no canonical
/// reason exists — avoids the "<unknown status code>" placeholder.
fn retry_status_reason(status: reqwest::StatusCode) -> String {
    match status.canonical_reason() {
        Some(r) => format!("{} {}", status.as_u16(), r),
        None => status.as_u16().to_string(),
    }
}

/// Cap a provider error body so the retry event's detail stays readable. A
/// 429 or 5xx body can be a multi-KB HTML/JSON error page; truncate with an
/// ellipsis. Snaps back to the nearest UTF-8 char boundary so the cut never
/// splits a multi-byte character (provider bodies often carry non-ASCII).
fn truncate_body(body: &str) -> String {
    const MAX: usize = 2000;
    if body.len() <= MAX {
        body.to_string()
    } else {
        let mut end = MAX;
        while end > 0 && !body.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &body[..end])
    }
}

/// Parse `Retry-After`-style headers. Supports the non-standard
/// `retry-after-ms` (milliseconds) and the standard `Retry-After` (seconds).
/// The HTTP-date form is not parsed — providers emit integer seconds in
/// practice; an unparseable value falls back to computed backoff.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    if let Some(ms) = headers
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        return Some(Duration::from_millis(ms));
    }
    let s = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    s.parse::<u64>().ok().map(Duration::from_secs)
}

/// Exponential backoff for `attempt` (1-indexed): `BASE_DELAY * 2^(attempt-1)`,
/// ±20% jitter, capped at `MAX_DELAY`. The cap applies after jitter so a
/// jittered value can never exceed `MAX_DELAY`.
fn backoff(attempt: u32) -> Duration {
    let exp = BACKOFF_FACTOR.powi((attempt.saturating_sub(1)) as i32);
    let base = BASE_DELAY.as_secs_f64() * exp;
    // Cheap entropy: subsec nanos span [0, 1e9), map to ±20%.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as f64;
    let jitter = 0.8 + 0.4 * (nanos / 1e9);
    let secs = (base * jitter).max(0.05).min(MAX_DELAY.as_secs_f64());
    Duration::from_secs_f64(secs)
}

/// Delay actually slept before attempt N+1: the larger of computed backoff and
/// a server-advertised `Retry-After`, capped to `MAX_RETRY_AFTER`.
fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    let bo = backoff(attempt);
    let raw = retry_after.map_or(bo, |ra| bo.max(ra));
    raw.min(MAX_RETRY_AFTER)
}

/// Send a streaming request, retrying transient handshake failures.
///
/// `build` constructs a fresh `RequestBuilder` per attempt — the body must be
/// re-sent on each retry, so the builder cannot be reused.
///
/// On success returns the `reqwest::Response` ready for `bytes_stream()`.
/// Terminal failures (non-retryable status, non-retryable send error, retries
/// exhausted) come back classified: overflow rejections as
/// [`ProviderError::Overflow`], other statuses as [`ProviderError::Http`],
/// transport failures as [`ProviderError::Transport`]. Cancellation at any
/// point returns [`ProviderError::Aborted`].
/// Send the streaming POST with exponential-backoff retry, firing the
/// request observer around every attempt: `before_payload` once the payload
/// is known (before the HTTP send) and `after_response` with the status of
/// each response, success and retryable alike — the TS before-payload /
/// after-response hooks.
pub async fn send_with_retry<F>(
    build: F,
    observer: Option<&dyn RequestObserver>,
    model: &crate::types::Model,
    payload: &serde_json::Value,
    signal: &CancellationToken,
    event_tx: &mpsc::Sender<AgentEvent>,
) -> Result<reqwest::Response, anyhow::Error>
where
    F: Fn(&serde_json::Value) -> reqwest::RequestBuilder,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        // The observer may substitute a mutated payload for this attempt;
        // the default keeps the request byte-identical across retries so
        // provider-side prefix caching is unaffected.
        let current = match observer {
            Some(observer) => observer
                .before_payload(attempt, model, payload)
                .unwrap_or_else(|| payload.clone()),
            None => payload.clone(),
        };
        let result = tokio::select! {
            _ = signal.cancelled() => return Err(ProviderError::Aborted.into()),
            res = build(&current).send() => res,
        };
        if let (Some(observer), Ok(resp)) = (observer, &result) {
            observer.after_response(attempt, resp.status().as_u16(), resp.headers());
        }
        match result {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) => {
                let status = resp.status();
                let retry_after = parse_retry_after(resp.headers());
                let body = resp.text().await.unwrap_or_default();
                if !is_retryable_status(status.as_u16()) || attempt >= MAX_ATTEMPTS {
                    return Err(overflow::terminal(status.as_u16(), body).into());
                }
                let delay = retry_delay(attempt, retry_after);
                tracing::warn!(
                    attempt,
                    max_attempts = MAX_ATTEMPTS,
                    status = %status,
                    delay_secs = delay.as_secs(),
                    "transient status, retrying"
                );
                let _ = event_tx
                    .send(AgentEvent::Retry {
                        attempt,
                        max_attempts: MAX_ATTEMPTS,
                        delay,
                        reason: retry_status_reason(status),
                        detail: Some(truncate_body(&body)),
                    })
                    .await;
                tokio::select! {
                    _ = signal.cancelled() => return Err(ProviderError::Aborted.into()),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            Err(err) => {
                if !is_retryable_send_error(&err) || attempt >= MAX_ATTEMPTS {
                    return Err(ProviderError::Transport(err.to_string()).into());
                }
                let delay = backoff(attempt);
                tracing::warn!(
                    attempt,
                    max_attempts = MAX_ATTEMPTS,
                    error = %err,
                    delay_secs = delay.as_secs(),
                    "send error, retrying"
                );
                let _ = event_tx
                    .send(AgentEvent::Retry {
                        attempt,
                        max_attempts: MAX_ATTEMPTS,
                        delay,
                        reason: classify_send_error(&err).to_string(),
                        detail: None,
                    })
                    .await;
                tokio::select! {
                    _ = signal.cancelled() => return Err(ProviderError::Aborted.into()),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
}

/// Error substrings that never classify as retryable — subscription/account
/// limits and quota/billing exhaustion are deterministic and would burn the
/// retry budget. Mirrors the TS `NON_RETRYABLE_PROVIDER_LIMIT_ERROR_PATTERN`.
const NON_RETRYABLE_PATTERNS: &[&str] = &[
    "GoUsageLimitError",
    "FreeUsageLimitError",
    "Monthly usage limit reached",
    "available balance",
    "insufficient_quota",
    "out of budget",
    "quota exceeded",
    "billing",
];

/// Error patterns that classify as retryable — provider load, transient HTTP
/// statuses, transport failures, and premature stream endings. Verbatim TS
/// `RETRYABLE_PROVIDER_ERROR_PATTERN`: the `.?` separators match one optional
/// character, so e.g. "rate.?limit" hits "rate limit", "ratelimit", and
/// "rate-limit" alike.
const RETRYABLE_PATTERNS: &[&str] = &[
    "overloaded",
    "rate.?limit",
    "too many requests",
    "429",
    "500",
    "502",
    "503",
    "504",
    "524",
    "service.?unavailable",
    "server.?error",
    "internal.?error",
    "provider.?returned.?error",
    "network.?error",
    "connection.?error",
    "connection.?refused",
    "connection.?lost",
    "other side closed",
    "fetch failed",
    "getaddrinfo",
    "ENOTFOUND",
    "EAI_AGAIN",
    "upstream.?connect",
    "reset before headers",
    "socket hang up",
    "socket connection was closed",
    "timed? out",
    "timeout",
    "terminated",
    "websocket.?closed",
    "websocket.?error",
    "ended without",
    "stream ended before message_stop",
    "stream ended before a terminal response event",
    "http2 request did not get a response",
    "retry delay",
    "you can retry your request",
    "try your request again",
    "please retry your request",
    // reqwest renders request-phase failures as "error sending request for
    // url (...)" and body-phase failures as "error decoding response body";
    // the inner hyper/io cause often renders empty, so the outer text alone
    // must classify. The "for url" qualifier keeps provider error bodies
    // that merely mention "error sending request" from classifying.
    "error.?sending.?request.?for.?url",
    "decoding.?response.?body",
    "ResourceExhausted",
];

fn combined_regex(patterns: &[&str]) -> regex::Regex {
    regex::RegexBuilder::new(&patterns.join("|"))
        .case_insensitive(true)
        .build()
        .expect("static retry patterns are valid regex")
}

/// Classify whether a failed assistant message looks like a transient
/// provider or transport error, mirroring the TS `isRetryableAssistantError`
/// (a case-insensitive regex over the raw provider text). Callers handle
/// context overflow separately (compaction, not retry); this classifier
/// itself does not know the context window. A non-retryable limit pattern
/// wins over a retryable one, so deterministic errors fail fast.
pub fn is_retryable_assistant_error(message: &crate::types::AgentMessage) -> bool {
    let crate::types::AgentMessage::Assistant {
        stop_reason,
        error_message,
        ..
    } = message
    else {
        return false;
    };
    if *stop_reason != Some(crate::types::StopReason::Error) {
        return false;
    }
    let Some(error_message) = error_message else {
        return false;
    };
    static NON_RETRYABLE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static RETRYABLE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    if NON_RETRYABLE
        .get_or_init(|| combined_regex(NON_RETRYABLE_PATTERNS))
        .is_match(error_message)
    {
        return false;
    }
    RETRYABLE
        .get_or_init(|| combined_regex(RETRYABLE_PATTERNS))
        .is_match(error_message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_model() -> crate::types::Model {
        crate::types::Model {
            provider: "test".into(),
            api: "test".into(),
            id: "m".into(),
            context_window: 1000,
            max_tokens: 10,
            thinking: crate::types::ThinkingKind::None,
            metadata: Default::default(),
        }
    }

    #[test]
    fn retryable_statuses() {
        for s in [408, 429, 500, 502, 503, 504, 520, 521, 522, 523, 524, 529] {
            assert!(is_retryable_status(s), "{s}");
        }
        for s in [400, 401, 403, 404, 409, 422, 451] {
            assert!(!is_retryable_status(s), "{s}");
        }
    }

    #[test]
    fn backoff_is_bounded() {
        // Without jitter the base grows as 1,2,4,8,16,30 (capped). With ±20%
        // jitter each sample stays in [0.8×base, 1.2×base] ∩ [0.05, MAX_DELAY].
        // The cap applies after jitter, so no sample exceeds MAX_DELAY.
        for attempt in 1..=MAX_ATTEMPTS {
            let d = backoff(attempt);
            assert!(d >= Duration::from_millis(40), "attempt {attempt}: {d:?}");
            assert!(d <= MAX_DELAY, "attempt {attempt}: {d:?} exceeds cap");
        }
        // Cap enforced even at extreme attempt counts.
        assert!(backoff(100) <= MAX_DELAY);
    }

    #[test]
    fn retry_delay_takes_max_and_caps() {
        // No Retry-After → backoff.
        let d = retry_delay(1, None);
        assert!(d <= MAX_DELAY && d >= Duration::from_millis(40));
        // Retry-After larger than backoff wins, but capped to MAX_RETRY_AFTER.
        let d = retry_delay(1, Some(Duration::from_secs(120)));
        assert_eq!(d, MAX_RETRY_AFTER);
        // Backoff larger than Retry-After wins.
        let d = retry_delay(5, Some(Duration::from_millis(10)));
        assert!(d > Duration::from_millis(10));
    }

    #[test]
    fn parse_retry_after_seconds_and_ms() {
        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after", "5".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(5)));

        let mut h = reqwest::header::HeaderMap::new();
        h.insert("retry-after-ms", "2500".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(Duration::from_millis(2500)));

        let h = reqwest::header::HeaderMap::new();
        assert_eq!(parse_retry_after(&h), None);

        // Unparseable (HTTP-date form) falls back to None → caller uses backoff.
        let mut h = reqwest::header::HeaderMap::new();
        h.insert(
            "retry-after",
            "Wed, 01 Jan 2099 00:00:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn retry_status_reason_labels() {
        let s = reqwest::StatusCode::from_u16(429).unwrap();
        assert_eq!(retry_status_reason(s), "429 Too Many Requests");
        let s = reqwest::StatusCode::from_u16(529).unwrap();
        assert_eq!(retry_status_reason(s), "529");
    }

    #[test]
    fn truncate_body_keeps_short_and_caps_long() {
        assert_eq!(truncate_body("short"), "short");
        let long = "x".repeat(3000);
        let t = truncate_body(&long);
        assert!(t.ends_with('…'));
        assert!(t.len() < 3000);
    }

    #[test]
    fn truncate_body_respects_utf8_boundary() {
        // A 3-byte CJK char straddling the 2000-byte cut must be dropped
        // wholesale, not split mid-codepoint (would panic / yield invalid UTF-8).
        let prefix = "a".repeat(1999);
        let body = format!("{prefix}中");
        let t = truncate_body(&body);
        assert!(t.ends_with('…'));
        assert!(std::str::from_utf8(t.as_bytes()).is_ok());
        assert!(!t.contains('中'));
    }

    #[tokio::test]
    async fn cancel_during_backoff_aborts() {
        // A closed port fails the connect phase fast and retryably; the retry
        // event confirms the backoff sleep has started, and cancelling then
        // must abort instead of sleeping through.
        let (tx, mut rx) = mpsc::channel(8);
        let signal = CancellationToken::new();
        let sig = signal.clone();
        // Loopback must bypass any system proxy from the environment; the
        // default client honors proxy env vars and would never reach port 1.
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let handle = tokio::spawn(async move {
            send_with_retry(
                |_| client.post("http://127.0.0.1:1/").body("x".to_string()),
                None,
                &test_model(),
                &serde_json::json!({"x": 1}),
                &signal,
                &tx,
            )
            .await
        });
        let event = rx.recv().await.unwrap();
        assert!(matches!(event, AgentEvent::Retry { attempt: 1, .. }));
        sig.cancel();
        let err = handle.await.unwrap().unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ProviderError>(),
            Some(ProviderError::Aborted)
        ));
    }

    #[tokio::test]
    async fn non_retryable_status_is_terminal_and_classified() {
        // A one-shot server answering a 400 overflow body: the helper must not
        // retry (exactly one request) and must surface ProviderError::Overflow.
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = requests.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Ok((mut socket, _)) = listener.accept().await {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body =
                    r#"{"error":{"message":"prompt is too long: 213462 tokens > 200000 maximum"}}"#;
                let response = format!(
                    "HTTP/1.1 400 Bad Request\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        let (tx, mut rx) = mpsc::channel(8);
        // Loopback must bypass any system proxy from the environment; the
        // default client honors proxy env vars and would route the fixture
        // server through it.
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://{addr}/");
        let err = send_with_retry(
            |_| client.post(&url).body("x".to_string()),
            None,
            &test_model(),
            &serde_json::json!({"x": 1}),
            &CancellationToken::new(),
            &tx,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ProviderError>(),
            Some(ProviderError::Overflow(_))
        ));
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            rx.try_recv().is_err(),
            "no retry event for a terminal status"
        );
    }
    #[tokio::test]
    async fn send_error_without_io_kind_is_retried() {
        // A connection accepted then closed before any response surfaces as a
        // request-phase reqwest error ("connection closed before message
        // completed") whose inner cause carries no io kind; the send layer
        // must classify this class as retryable. The retry event proves the
        // classification; cancelling then aborts the backoff.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            while let Ok((mut socket, _)) = listener.accept().await {
                let _ = socket.shutdown().await;
                drop(socket);
            }
        });
        let (tx, mut rx) = mpsc::channel(8);
        let signal = CancellationToken::new();
        let sig = signal.clone();
        // Loopback must bypass any system proxy from the environment; the
        // default client honors proxy env vars and would route the fixture
        // server through it.
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://{addr}/");
        let handle = tokio::spawn(async move {
            send_with_retry(
                |_| client.post(&url).body("x".to_string()),
                None,
                &test_model(),
                &serde_json::json!({"x": 1}),
                &signal,
                &tx,
            )
            .await
        });
        let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("send-phase drop must emit a retry event")
            .expect("retry channel closed");
        assert!(matches!(event, AgentEvent::Retry { attempt: 1, .. }));
        sig.cancel();
        let err = handle.await.unwrap().unwrap_err();
        assert!(matches!(
            err.downcast_ref::<ProviderError>(),
            Some(ProviderError::Aborted)
        ));
    }

    fn assistant(error_message: Option<&str>) -> crate::types::AgentMessage {
        crate::types::AgentMessage::Assistant {
            content: Vec::new(),
            model: "m".into(),
            provider: "p".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(crate::types::StopReason::Error),
            raw_stop_reason: None,
            usage: Box::default(),
            error_message: error_message.map(str::to_string),
            timestamp: chrono::Utc::now(),
        }
    }

    #[test]
    fn retryable_assistant_error_classifies_transients() {
        for message in [
            "http 529: overloaded",
            "429 Too Many Requests",
            "The server is overloaded, please retry later",
            "Provider returned error: connection refused",
            "Error: socket hang up",
            "Anthropic stream ended before message_stop",
            "stream ended without finish_reason",
            "upstream connect error or disconnect/reset before headers",
            "request timed out after 60s",
            "fetch failed: getaddrinfo ENOTFOUND api.example.com",
            "http 500: internal error",
        ] {
            assert!(
                is_retryable_assistant_error(&assistant(Some(message))),
                "{message}"
            );
        }
    }

    #[test]
    fn retryable_assistant_error_classifies_reqwest_transport_wraps() {
        // The reqwest-produced strings that actually reach the turn-level
        // classifier in production, wrapped by `ProviderError::Transport`.
        // The inner hyper/io cause renders empty, so only the outer text is
        // available to classify.
        for message in [
            "transport error: error sending request for url (https://dashscope.aliyuncs.com/apps/anthropic/v1/messages)",
            "transport error: error sending request for url (https://api.deepseek.com/anthropic/v1/messages)",
            "transport error: error sending request for url (http://127.0.0.1:1/)",
            "transport error: error decoding response body",
            "transport error: error decoding response body for url (https://api.example.com/v1/messages)",
        ] {
            assert!(
                is_retryable_assistant_error(&assistant(Some(message))),
                "{message}"
            );
        }
    }

    #[test]
    fn retryable_assistant_error_excludes_limits_and_non_errors() {
        for message in [
            "insufficient_quota: you have exceeded your quota",
            "Monthly usage limit reached, please enable available balance",
            "billing error: out of budget",
            "GoUsageLimitError: free-tier limit",
            "invalid temperature: only 0.6 is allowed",
            "prompt is too long: 213462 tokens > 200000 maximum",
        ] {
            assert!(
                !is_retryable_assistant_error(&assistant(Some(message))),
                "{message}"
            );
        }
        // A provider error body that coincidentally contains reqwest wording
        // must not classify as retryable.
        assert!(!is_retryable_assistant_error(&assistant(Some(
            "http 400: {\"error\":{\"message\":\"error sending request ID is required\"}}"
        ))));
        // A successful stop or an error without a message is never retried.
        let stop = crate::types::AgentMessage::Assistant {
            content: Vec::new(),
            model: "m".into(),
            provider: "p".into(),
            api: "test".into(),
            response_model: None,
            response_id: None,
            diagnostics: None,
            stop_reason: Some(crate::types::StopReason::Stop),
            raw_stop_reason: None,
            usage: Box::default(),
            error_message: Some("http 529: overloaded".into()),
            timestamp: chrono::Utc::now(),
        };
        assert!(!is_retryable_assistant_error(&stop));
        assert!(!is_retryable_assistant_error(&assistant(None)));
    }

    #[test]
    fn retryable_assistant_error_matches_ts_regex_separators() {
        // The TS `.?` separator matches one optional character: a space, a
        // dash, an underscore, or nothing at all.
        for message in [
            "Rate Limit exceeded",
            "rate-limit exceeded",
            "rate_limit exceeded",
            "ratelimit exceeded",
            "connection_error",
            "service-unavailable",
            "Service Unavailable",
            "server_error",
            "stream ended before message_stop",
        ] {
            assert!(
                is_retryable_assistant_error(&assistant(Some(message))),
                "{message}"
            );
        }
    }
}
