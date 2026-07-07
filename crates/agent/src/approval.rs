//! Built-in approval reviewer agent.
//!
//! When the thread's `ApprovalMode` is `AutoReview`, each tool call that would
//! normally show the authorization overlay is instead vetted by [`review`]
//! before running. The reviewer makes a single-shot LLM call (no tools, no
//! streaming) and returns one of two verdicts:
//!
//! - [`ReviewVerdict::Allow`] — the tool runs immediately, same as YOLO.
//! - [`ReviewVerdict::Ask { reason }`] — the auth overlay is shown; the
//!   `reason` is rendered under the tool title so the user knows why the
//!   reviewer escalated the call.
//!
//! Failures (LLM unavailable, timeout, malformed response) **all** downgrade
//! to `Ask` with a generic reason — the reviewer is fail-closed so a broken
//! auto-review path never silently widens access.
//!
//! The reviewer prompt lives in `approval/prompt.md` and is `include_str!`-ed
//! at compile time. It is model-facing text and is therefore English-only —
//! it is never routed through the `i18n` bundle.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::AsyncApp;
use serde::Deserialize;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::language_model::{
    AnyLanguageModel, LanguageModelCompletionEvent, LanguageModelRequest,
    LanguageModelRequestMessage, MessageContent, Role,
};

const PROMPT: &str = include_str!("approval/prompt.md");

/// Per-call hard timeout for the reviewer. The reviewer is allowed to take
/// longer than a streaming chunk — the user is already waiting for the tool
/// to run, so a couple of seconds for an LLM judgment is acceptable. Past
/// this bound we fail-closed to `Ask`.
const REVIEW_TIMEOUT: Duration = Duration::from_secs(8);

/// Verdict the reviewer returns for a single tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewVerdict {
    /// The tool is safe to run without prompting the user.
    Allow,
    /// The reviewer could not auto-approve; show the auth overlay with
    /// `reason` rendered as a one-line justification.
    Ask { reason: String },
}

#[derive(Debug, Deserialize)]
struct VerdictPayload {
    verdict: String,
    #[serde(default)]
    reason: Option<String>,
}

/// Vet a single tool call under `AutoReview`. Blocks until the reviewer
/// responds, the per-call timeout elapses, or `cancel` fires — every
/// non-success path returns [`ReviewVerdict::Ask`].
///
/// `model` is the same `AnyLanguageModel` the owning thread uses for its main
/// loop. We deliberately do not include the thread's full message history:
/// the reviewer needs only the call itself plus a sliver of context (cwd) to
/// make a sound decision, and excluding history keeps the reviewer's own
/// provider-side prompt cache hot across calls.
pub async fn review(
    model: &AnyLanguageModel,
    tool_name: &str,
    tool_input: &serde_json::Value,
    tool_title: &str,
    cwd: &Path,
    cancel: CancellationToken,
    cx: &AsyncApp,
) -> ReviewVerdict {
    let user_prompt = format!(
        "cwd: {}\ntool_name: {}\ntool_title: {}\ntool_input: {}",
        cwd.display(),
        tool_name,
        tool_title,
        serde_json::to_string_pretty(tool_input)
            .unwrap_or_else(|_| "<unprintable input>".to_string()),
    );

    let request = LanguageModelRequest {
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![MessageContent::Text(PROMPT.to_string())],
                cache: true,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(user_prompt)],
                cache: false,
            },
        ],
        tools: Vec::new(),
        tool_choice: None,
        temperature: Some(0.0),
        thinking_allowed: false,
    };

    let model = Arc::clone(model);
    let call = async move {
        let stream = match model.stream_completion(request, cx).await {
            Ok(s) => s,
            Err(_) => return None,
        };
        futures::pin_mut!(stream);
        let mut text = String::new();
        while let Some(event) = stream.next().await {
            let Ok(event) = event else { return None };
            match event {
                LanguageModelCompletionEvent::Text(delta) => text.push_str(&delta),
                LanguageModelCompletionEvent::Stop(_) => break,
                _ => {}
            }
        }
        Some(text)
    };

    let outcome = tokio::select! {
        result = timeout(REVIEW_TIMEOUT, call) => result.ok().flatten(),
        _ = cancel.cancelled() => None,
    };
    let Some(text) = outcome else {
        return ReviewVerdict::Ask {
            reason: "auto-review unavailable; please confirm".to_string(),
        };
    };

    parse_verdict(&text).unwrap_or(ReviewVerdict::Ask {
        reason: "auto-review response unparseable; please confirm".to_string(),
    })
}

fn parse_verdict(text: &str) -> Option<ReviewVerdict> {
    // The reviewer is told to emit a single JSON line. Be tolerant of
    // surrounding whitespace, code fences, or a short prose preamble by
    // pulling the first '{' through the matching '}'.
    let trimmed = text.trim();
    let payload: VerdictPayload = if let Some(start) = trimmed.find('{') {
        let candidate = &trimmed[start..];
        let end = candidate.rfind('}')?;
        serde_json::from_str(&candidate[..=end]).ok()?
    } else {
        serde_json::from_str(trimmed).ok()?
    };
    let reason = payload
        .reason
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty());
    match payload.verdict.to_ascii_uppercase().as_str() {
        "ALLOW" => Some(ReviewVerdict::Allow),
        "ASK" => Some(ReviewVerdict::Ask {
            reason: reason.unwrap_or_else(|| "auto-review asked to confirm".to_string()),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_allow_without_reason() {
        let v = parse_verdict(r#"{"verdict":"ALLOW"}"#).unwrap();
        assert_eq!(v, ReviewVerdict::Allow);
    }

    #[test]
    fn parses_ask_with_reason() {
        let v = parse_verdict(r#"{"verdict":"ASK","reason":"network access"}"#).unwrap();
        assert_eq!(
            v,
            ReviewVerdict::Ask {
                reason: "network access".into()
            }
        );
    }

    #[test]
    fn tolerates_surrounding_prose_and_fences() {
        let v = parse_verdict("Here is my judgment:\n```json\n{\"verdict\":\"ALLOW\",\"reason\":\"read-only\"}\n```\n").unwrap();
        assert_eq!(
            v,
            ReviewVerdict::Allow,
            "should drop preamble and code fences"
        );
    }

    #[test]
    fn falls_through_on_unknown_verdict() {
        assert!(parse_verdict(r#"{"verdict":"MAYBE"}"#).is_none());
    }

    #[test]
    fn falls_through_on_garbage() {
        assert!(parse_verdict("not json at all").is_none());
    }
}
