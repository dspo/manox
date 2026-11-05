// Provider layer — real `StreamFn` implementations backed by LLM APIs.
//
// Each provider lives in its own submodule with wire types that mirror that
// provider's protocol exactly. Wire types are never shared across providers;
// the cross-provider representation is the domain types in `crate::types`.
// The SSE parser (`sse`) is transport-level and is shared, as are the
// handshake retry loop (`retry`), the context-overflow classifier
// (`overflow`), and the transcript repair (`transform`) — all
// shape-agnostic.

pub mod anthropic;
pub mod openai;
pub mod overflow;
pub mod retry;
pub mod sse;
pub mod transform;

/// Observes each HTTP request attempt of a provider stream. A consumer
/// attaches one via the
/// provider builder (`with_request_observer`) to surface payload and status
/// outside the provider; the harness maps it onto its
/// `BeforeProviderPayload` / `AfterProviderResponse` hook points.
pub trait RequestObserver: Send + Sync {
    /// The payload about to be sent for `model`, `attempt` 1-indexed.
    /// Returning `Some(replacement)` substitutes the payload for this
    /// attempt; `None` sends the original.
    fn before_payload(
        &self,
        attempt: u32,
        model: &crate::types::Model,
        payload: &serde_json::Value,
    ) -> Option<serde_json::Value>;

    /// The HTTP status and headers of an attempt's response — success and
    /// retryable statuses both fire.
    fn after_response(&self, attempt: u32, status: u16, headers: &reqwest::header::HeaderMap);
}

/// Identity substituted when validating a template outside a live request:
/// a template only has to render with *some* session id to be well formed.
const VALIDATION_SESSION_ID: &str = "00000000-0000-0000-0000-000000000000";

/// Render a config-supplied header value template against one request's
/// session identity. Tera syntax — the same engine the prompt templates
/// use — with `session_id` as the only variable the context carries, so a
/// template referencing anything else fails to render. Any render failure
/// (unknown field, missing session id, unclosed braces) drops the header
/// (`None`) instead of shipping a broken value upstream. Autoescape stays
/// off: header values are wire text, never HTML.
pub fn render_header_value(template: &str, session_id: Option<&str>) -> Option<String> {
    let mut ctx = tera::Context::new();
    if let Some(id) = session_id {
        ctx.insert("session_id", id);
    }
    match tera::Tera::one_off(template, &ctx, false) {
        Ok(rendered) => Some(rendered),
        Err(err) => {
            tracing::warn!(
                template = %template,
                error = %err,
                "header template failed to render — header dropped"
            );
            None
        }
    }
}

/// Check a config-supplied template without a live request, so a broken one
/// surfaces once at provider-registration time instead of as a silently
/// missing header (and an upstream complaint) on every request.
pub fn validate_header_value_template(template: &str) -> Result<(), String> {
    let mut ctx = tera::Context::new();
    ctx.insert("session_id", VALIDATION_SESSION_ID);
    tera::Tera::one_off(template, &ctx, false)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

/// Build one request's extra headers from config-supplied `(name, value)`
/// pairs. Values render against `session_id`; an unrenderable template or a
/// name/value that is not a legal HTTP header drops that entry.
///
/// Later entries win on a same-name collision — header names compare
/// case-insensitively — which is what makes the registry's injected auth
/// header (appended after the configured headers) override a same-name
/// config entry instead of putting two values on the wire.
pub fn build_extra_headers(
    headers: &[(String, String)],
    session_id: Option<&str>,
) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, template) in headers {
        let Some(value) = render_header_value(template, session_id) else {
            continue;
        };
        let Ok(name) = reqwest::header::HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        let Ok(value) = value.parse::<reqwest::header::HeaderValue>() else {
            continue;
        };
        map.insert(name, value);
    }
    map
}

use crate::types::{Cost, Model, Usage};

/// The model's rate card from registration metadata (`cost`, USD per 1M
/// tokens per class), when present and non-zero. The kernel never guesses
/// rates: a model registered without pricing (or with an all-zero card)
/// carries no cost.
pub fn model_cost_rates(model: &Model) -> Option<Cost> {
    let card = model.metadata.get("cost")?;
    let rate = |key: &str| card.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let rates = Cost {
        input: rate("input"),
        output: rate("output"),
        cache_read: rate("cacheRead"),
        cache_write: rate("cacheWrite"),
        total: 0.0,
    };
    (rates.input > 0.0 || rates.output > 0.0 || rates.cache_read > 0.0 || rates.cache_write > 0.0)
        .then_some(rates)
}

/// Price a wire usage against a rate card (USD per 1M tokens per class).
/// `Cost.total` sums all classes.
pub fn price_usage(rates: &Cost, usage: &Usage) -> Cost {
    let per_million = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
    let input = per_million(usage.input_tokens, rates.input);
    let output = per_million(usage.output_tokens, rates.output);
    let cache_read = per_million(usage.cache_read_input_tokens, rates.cache_read);
    let cache_write = per_million(usage.cache_creation_input_tokens, rates.cache_write);
    Cost {
        input,
        output,
        cache_read,
        cache_write,
        total: input + output + cache_read + cache_write,
    }
}

use thiserror::Error;

/// Errors a provider can surface while streaming.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The API returned a non-2xx status. `body` holds the error envelope.
    #[error("http {status}: {body}")]
    Http { status: u16, body: String },

    /// The request input exceeds the model's context window. Deterministic —
    /// the loop layer answers with compact-and-retry, not a plain retry.
    #[error("context overflow: {0}")]
    Overflow(String),

    /// An SSE frame could not be parsed.
    #[error("malformed sse frame: {line}")]
    Sse { line: String },

    /// A protocol payload failed to deserialize.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// The request was aborted via its cancellation token.
    #[error("aborted")]
    Aborted,

    /// A transport-level failure (connect, timeout, reset).
    #[error("transport error: {0}")]
    Transport(String),

    /// The API signalled an error mid-stream: a 2xx response whose event
    /// stream carries an `{"error": ...}` payload instead of protocol events.
    #[error("provider error mid-stream: {0}")]
    MidStream(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ThinkingKind;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn header_template_renders_session_id() {
        assert_eq!(
            render_header_value("{{ session_id }}", Some("77c6f3e6")),
            Some("77c6f3e6".to_string())
        );
        // Whitespace inside the braces is insignificant; surrounding text
        // is preserved verbatim.
        assert_eq!(
            render_header_value("prefix-{{session_id}}-suffix", Some("abc")),
            Some("prefix-abc-suffix".to_string())
        );
        assert_eq!(
            render_header_value("static", Some("abc")),
            Some("static".to_string())
        );
        // Tera's built-in filters ride along — the developer doc promises
        // them (docs/cx/request-headers.md).
        assert_eq!(
            render_header_value("{{ session_id | upper }}", Some("abc")),
            Some("ABC".to_string())
        );
    }

    #[test]
    fn header_template_drops_when_unrenderable() {
        // No session id on the request: the placeholder must not reach the
        // wire.
        assert_eq!(render_header_value("{{ session_id }}", None), None);
        // Unsupported fields and malformed braces drop the header too.
        assert_eq!(render_header_value("{{ model }}", Some("abc")), None);
        assert_eq!(render_header_value("{{ session_id", Some("abc")), None);
    }

    /// Registration-time validation accepts every template a live request
    /// can render and rejects the ones that would be dropped on the wire.
    #[test]
    fn header_template_validation_matches_the_render_path() {
        assert!(validate_header_value_template("static").is_ok());
        assert!(validate_header_value_template("go-{{ session_id }}").is_ok());
        assert!(validate_header_value_template("{{ session_id | upper }}").is_ok());
        assert!(validate_header_value_template("{{ model }}").is_err());
        assert!(validate_header_value_template("{{ session_id").is_err());
    }

    /// A later same-name entry overrides an earlier one — case-insensitively,
    /// as HTTP header names compare — so a runtime-injected auth header wins
    /// over a config entry instead of doubling it on the wire.
    #[test]
    fn build_extra_headers_renders_drops_and_overrides() {
        let headers = vec![
            ("x-static".to_string(), "value".to_string()),
            ("x-session".to_string(), "go-{{ session_id }}".to_string()),
            ("x-broken".to_string(), "{{ model }}".to_string()),
            ("X-Overridden".to_string(), "from-config".to_string()),
            ("x-overridden".to_string(), "from-runtime".to_string()),
        ];
        let map = build_extra_headers(&headers, Some("sess_1"));
        assert_eq!(map.get("x-static").unwrap(), "value");
        assert_eq!(map.get("x-session").unwrap(), "go-sess_1");
        assert!(
            map.get("x-broken").is_none(),
            "unrenderable value must drop"
        );
        assert_eq!(
            map.get("x-overridden").unwrap(),
            "from-runtime",
            "the later entry must replace, not append"
        );
        assert_eq!(map.get_all("x-overridden").iter().count(), 1);
    }

    fn model_with_cost(card: Option<serde_json::Value>) -> Model {
        let mut metadata = HashMap::new();
        if let Some(v) = card {
            metadata.insert("cost".to_string(), v);
        }
        Model {
            provider: "p".into(),
            api: "anthropic".into(),
            id: "m".into(),
            context_window: 1000,
            max_tokens: 100,
            thinking: ThinkingKind::None,
            metadata,
        }
    }

    #[test]
    fn rate_card_parses_and_prices_usage() {
        let model = model_with_cost(Some(json!({
            "input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75
        })));
        let rates = model_cost_rates(&model).expect("rate card present");
        let usage = Usage {
            input_tokens: 1_000_000,
            output_tokens: 100_000,
            cache_read_input_tokens: 2_000_000,
            cache_creation_input_tokens: 400_000,
            cache_write_1h: None,
            reasoning_tokens: None,
            total_tokens: 0,
            cost: None,
        };
        let cost = price_usage(&rates, &usage);
        assert!((cost.input - 3.0).abs() < 1e-9);
        assert!((cost.output - 1.5).abs() < 1e-9);
        assert!((cost.cache_read - 0.6).abs() < 1e-9);
        assert!((cost.cache_write - 1.5).abs() < 1e-9);
        assert!((cost.total - 6.6).abs() < 1e-9);
    }

    #[test]
    fn missing_or_zero_rate_card_yields_no_cost() {
        assert!(model_cost_rates(&model_with_cost(None)).is_none());
        assert!(
            model_cost_rates(&model_with_cost(Some(json!({
                "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0
            }))))
            .is_none()
        );
    }
}
