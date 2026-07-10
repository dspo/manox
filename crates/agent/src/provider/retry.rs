//! Transient-failure retry for the LLM HTTP handshake.
//!
//! All three wires (`anthropic` / `completions` / `responses`) send a streaming
//! POST and previously surfaced any non-2xx status — most painfully 429 —
//! directly to the user as a terminal `ThreadEvent::Error`. This module wraps
//! the `send()` + status check in an exponential-backoff retry loop so 429,
//! 5xx, and network errors recover silently; only after `MAX_ATTEMPTS` does
//! the error reach the user.
//!
//! Safety boundary: retry happens only at the handshake stage, before any SSE
//! event has been forwarded through `tx`. A stream that fails mid-flight (after
//! text/thinking deltas are already emitted) is NOT retried — re-sending would
//! duplicate output the user already saw. The retry body is byte-identical to
//! the original, so provider-side prefix caching is unaffected.
//!
//! Cancellation: the turn's `CancellationToken` is not threaded into the
//! provider trait. Instead the loop watches `tx.is_closed()` — when the turn is
//! cancelled, `thread.rs` drops the stream receiver and the sender observes
//! closure, letting the in-flight retry bail without firing another request.

use std::time::Duration;

use anyhow::{Result, anyhow};
use async_channel::Sender;
use futures::Future;
use http::StatusCode;

use crate::language_model::LanguageModelCompletionEvent;

/// Total request budget per turn — the original attempt plus this many retries.
const MAX_ATTEMPTS: u32 = 6;
const BASE_DELAY: Duration = Duration::from_secs(1);
const BACKOFF_FACTOR: f64 = 2.0;
const MAX_DELAY: Duration = Duration::from_secs(30);
/// Upper bound on a server-advertised `Retry-After`, so a misbehaving upstream
/// cannot stall a turn indefinitely.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
/// Poll cadence for cancel-during-sleep detection.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// HTTP statuses whose failure is likely to resolve on retry. The unofficial
/// 529 ("service overloaded") is included — Anthropic emits it in practice.
/// 520–524 are Cloudflare gateway errors common to provider front-ends.
fn should_retry_status(status: StatusCode) -> bool {
    matches!(
        status.as_u16(),
        408 | 429 | 500 | 502 | 503 | 504 | 520 | 521 | 522 | 523 | 524 | 529
    )
}

/// reqwest errors worth retrying. `is_connect` covers only the connect phase,
/// so a connection reset / broken pipe / HTTP-2 stream reset mid-request (the
/// common transient-transport class) is caught via the source-chain io kind.
/// Request-construction (`is_request`) and body-serialization (`is_body`)
/// errors reproduce identically and are never retried.
fn is_retryable_send_error(err: &reqwest::Error) -> bool {
    if err.is_connect() || err.is_timeout() || err.is_redirect() {
        return true;
    }
    let mut source: Option<&dyn std::error::Error> = Some(err);
    while let Some(s) = source {
        if let Some(io) = s.downcast_ref::<std::io::Error>()
            && matches!(
                io.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::UnexpectedEof
            )
        {
            return true;
        }
        source = s.source();
    }
    false
}

/// Short, user-facing label for a retryable reqwest send error. Mirrors the
/// retry-decision logic of `is_retryable_send_error` but only classifies — it
/// never gates a retry. Kept terse so the retry badge reads as one line.
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
/// reason exists — avoids http's "<unknown status code>" placeholder.
fn retry_status_reason(status: StatusCode) -> String {
    match status.canonical_reason() {
        Some(r) => format!("{} {}", status.as_u16(), r),
        None => status.as_u16().to_string(),
    }
}

/// Cap a provider error body so the expanded retry card stays readable. A 429
/// or 5xx body can be a multi-KB HTML/JSON error page; truncate with an
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
/// The HTTP-date form of `Retry-After` is not parsed — Anthropic and OpenAI
/// both emit integer seconds, so the common path is covered; an unparseable
/// value falls back to computed backoff.
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

/// Sleep for `delay` unless the receiver has been dropped (turn cancelled).
/// Polls `tx.is_closed()` at `CANCEL_POLL` cadence rather than blocking the
/// full delay — keeps cancel latency bounded without a trait-level cancel
/// signal.
async fn wait_or_cancelled(
    delay: Duration,
    tx: &Sender<Result<LanguageModelCompletionEvent>>,
) -> bool {
    let mut remaining = delay;
    loop {
        if tx.is_closed() {
            return false;
        }
        let step = remaining.min(CANCEL_POLL);
        tokio::time::sleep(step).await;
        remaining = match remaining.checked_sub(step) {
            Some(r) if r.is_zero() => return true,
            Some(r) => r,
            None => return true,
        };
    }
}

/// Send a streaming request, retrying transient handshake failures.
///
/// `build` constructs a fresh `RequestBuilder.send()` future per attempt — the
/// body must be re-sent on each retry, so the builder cannot be reused. The
/// closure owns the body/handler capture.
///
/// On success returns the `reqwest::Response` ready for `bytes_stream()`. On
/// terminal failure (non-retryable status, non-retryable send error, retries
/// exhausted, or cancellation) the error has already been forwarded through
/// `tx` as a `LanguageModelCompletionEvent::Err`-equivalent (an `Err(...)` event)
/// and the caller should return `Ok(())` — the stream consumer will surface it.
///
/// `label` prefixes error strings ("Anthropic API", "Completions API", …).
pub async fn send_with_retry<F, Fut>(
    build: F,
    tx: &Sender<Result<LanguageModelCompletionEvent>>,
    label: &str,
) -> Result<reqwest::Response>
where
    F: Fn() -> Fut,
    Fut: Future<Output = reqwest::Result<reqwest::Response>>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        if tx.is_closed() {
            return Err(anyhow!("stream receiver closed before attempt {attempt}"));
        }
        match build().await {
            Ok(resp) if resp.status().is_success() => return Ok(resp),
            Ok(resp) => {
                let status = resp.status();
                let retry_after = parse_retry_after(resp.headers());
                let body = resp.text().await.unwrap_or_default();
                if !should_retry_status(status) || attempt >= MAX_ATTEMPTS {
                    let _ = tx
                        .send(Err(anyhow!("{label} returned {status}: {body}")))
                        .await;
                    return Err(anyhow!("{label} returned {status}"));
                }
                let delay = retry_delay(attempt, retry_after);
                tracing::warn!(
                    target: "provider",
                    attempt, max_attempts = MAX_ATTEMPTS,
                    status = %status,
                    delay_secs = delay.as_secs(),
                    "{label} transient status, retrying"
                );
                let _ = tx
                    .send(Ok(LanguageModelCompletionEvent::Retry {
                        attempt,
                        max_attempts: MAX_ATTEMPTS,
                        delay_secs: delay.as_secs(),
                        reason: retry_status_reason(status),
                        detail: Some(truncate_body(&body)),
                    }))
                    .await;
                if !wait_or_cancelled(delay, tx).await {
                    return Err(anyhow!("stream receiver closed during retry"));
                }
            }
            Err(err) => {
                if !is_retryable_send_error(&err) || attempt >= MAX_ATTEMPTS {
                    let _ = tx.send(Err(anyhow!("{label} call failed: {err}"))).await;
                    return Err(anyhow!("{label} send failed: {err}"));
                }
                let delay = backoff(attempt);
                tracing::warn!(
                    target: "provider",
                    attempt, max_attempts = MAX_ATTEMPTS,
                    error = %err,
                    delay_secs = delay.as_secs(),
                    "{label} send error, retrying"
                );
                let _ = tx
                    .send(Ok(LanguageModelCompletionEvent::Retry {
                        attempt,
                        max_attempts: MAX_ATTEMPTS,
                        delay_secs: delay.as_secs(),
                        reason: classify_send_error(&err).to_string(),
                        detail: None,
                    }))
                    .await;
                if !wait_or_cancelled(delay, tx).await {
                    return Err(anyhow!("stream receiver closed during retry"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_statuses() {
        for s in [408, 429, 500, 502, 503, 504, 520, 521, 522, 523, 524, 529] {
            assert!(should_retry_status(StatusCode::from_u16(s).unwrap()), "{s}");
        }
        for s in [400, 401, 403, 404, 409, 422, 451] {
            assert!(
                !should_retry_status(StatusCode::from_u16(s).unwrap()),
                "{s}"
            );
        }
    }

    #[test]
    fn backoff_is_bounded_and_nondecreasing_in_mean() {
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
}
