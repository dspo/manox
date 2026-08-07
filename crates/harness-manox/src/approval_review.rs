//! The retired manox harness approval-review adapter.
//!
//! The model-agnostic core lives in `agent::approval_review` (prompt
//! construction, verdict parsing, fail-closed policy, field truncation).
//! This module adapts the manox `AnyLanguageModel::stream_completion`
//! machinery to it.
//!
//! Unlike the pi path (whose `ReviewerFn` races a `tokio::time` deadline on
//! the actor runtime), the manox turn loop runs on the gpui foreground
//! executor where no tokio runtime context exists — `tokio::time::timeout`
//! would panic at `Handle::current`. The race therefore stays gpui-native
//! here (`cx.background_executor().timer`), exactly as before the
//! generalization; `stream_completion` itself is executor-agnostic (it
//! spawns its HTTP work onto the global tokio runtime and forwards events
//! through an executor-agnostic channel).

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use agent::approval::{ReviewBatchOutcome, ReviewItem, ReviewOutcome, ReviewVerdict};
use agent::approval_review::{
    REVIEW_TIMEOUT, REVIEWER_UNAVAILABLE_REASON, batch_prompt, parse_batch_verdicts, parse_verdict,
    single_prompt,
};
use futures::StreamExt as _;
use gpui::AsyncApp;
use tokio_util::sync::CancellationToken;

use crate::language_model::{
    AnyLanguageModel, LanguageModelCompletionEvent, LanguageModelRequest,
    LanguageModelRequestMessage, MessageContent, Role,
};

fn fail_closed(items: &[ReviewItem]) -> HashMap<String, ReviewVerdict> {
    items
        .iter()
        .map(|item| {
            (
                item.id.clone(),
                ReviewVerdict::Ask {
                    reason: REVIEWER_UNAVAILABLE_REASON.to_string(),
                },
            )
        })
        .collect()
}

/// Build the manox reviewer request from the core-rendered prompts, applying
/// the manox side-call policy (model override, reasoning effort, output
/// cap).
fn reviewer_request(
    model: &AnyLanguageModel,
    system: String,
    user: String,
) -> (AnyLanguageModel, LanguageModelRequest) {
    let policy = crate::settings_ext::side_calls().approval_policy();
    let model = crate::settings_ext::side_call_model(&policy, model);
    let request = LanguageModelRequest {
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![MessageContent::Text(system)],
                cache: true,
            },
            LanguageModelRequestMessage {
                role: Role::User,
                content: vec![MessageContent::Text(user)],
                cache: false,
            },
        ],
        tools: Vec::new(),
        tool_choice: None,
        temperature: Some(0.0),
        thinking_allowed: false,
        reasoning_effort: agent::settings::side_call_effort(
            &policy,
            crate::language_model::RequestReasoningEffort::Low,
        ),
        max_output_tokens: agent::settings::side_call_output_cap(policy),
    };
    (model, request)
}

/// Vet every AutoPilot tool call from one assistant response in one side
/// request. Missing or malformed per-id verdicts fail closed independently.
pub async fn review_batch(
    model: &AnyLanguageModel,
    items: &[ReviewItem],
    cwd: &Path,
    lang: agent::language::Language,
    cancel: CancellationToken,
    cx: &AsyncApp,
) -> ReviewBatchOutcome {
    review_batch_with_timeout(model, items, cwd, lang, cancel, REVIEW_TIMEOUT, cx).await
}

async fn review_batch_with_timeout(
    model: &AnyLanguageModel,
    items: &[ReviewItem],
    cwd: &Path,
    lang: agent::language::Language,
    cancel: CancellationToken,
    timeout: Duration,
    cx: &AsyncApp,
) -> ReviewBatchOutcome {
    if items.is_empty() {
        return ReviewBatchOutcome {
            verdicts: HashMap::new(),
            usage: None,
            model_name: model.name(),
        };
    }
    let (system, user) = batch_prompt(items, cwd, lang);
    let (model, request) = reviewer_request(model, system, user);
    let model_name = model.name();

    let call = async move {
        let stream = model.stream_completion(request, cx).await.ok()?;
        futures::pin_mut!(stream);
        let mut text = String::new();
        let mut usage = None;
        while let Some(event) = stream.next().await {
            match event.ok()? {
                LanguageModelCompletionEvent::Text(delta) => text.push_str(&delta),
                LanguageModelCompletionEvent::UsageUpdate(value) => usage = Some(value),
                LanguageModelCompletionEvent::Stop(_) => break,
                _ => {}
            }
        }
        Some((text, usage))
    };
    let outcome = tokio::select! {
        result = call => result,
        _ = cx.background_executor().timer(timeout) => None,
        _ = cancel.cancelled() => None,
    };
    let Some((text, usage)) = outcome else {
        return ReviewBatchOutcome {
            verdicts: fail_closed(items),
            usage: None,
            model_name,
        };
    };
    ReviewBatchOutcome {
        verdicts: parse_batch_verdicts(&text, items),
        usage,
        model_name,
    }
}

/// Vet a single tool call under `AutoPilot`. Blocks until the reviewer
/// responds, the per-call timeout elapses, or `cancel` fires — every
/// non-success path returns [`ReviewVerdict::Ask`].
// too_many_arguments: signature preserved from the pre-generalization
// implementation; each parameter is a distinct review input the caller
// already holds as a separate owned value.
#[allow(clippy::too_many_arguments)]
pub async fn review(
    model: &AnyLanguageModel,
    tool_name: &str,
    tool_input: &serde_json::Value,
    tool_title: &str,
    cwd: &Path,
    lang: agent::language::Language,
    cancel: CancellationToken,
    cx: &AsyncApp,
) -> ReviewOutcome {
    let (system, user) = single_prompt(tool_name, tool_input, tool_title, cwd, lang);
    let (model, request) = reviewer_request(model, system, user);
    let model_name = model.name();

    let call = async move {
        let stream = model.stream_completion(request, cx).await.ok()?;
        futures::pin_mut!(stream);
        let mut text = String::new();
        let mut usage = None;
        while let Some(event) = stream.next().await {
            match event.ok()? {
                LanguageModelCompletionEvent::Text(delta) => text.push_str(&delta),
                LanguageModelCompletionEvent::UsageUpdate(value) => usage = Some(value),
                LanguageModelCompletionEvent::Stop(_) => break,
                _ => {}
            }
        }
        Some((text, usage))
    };
    let outcome = tokio::select! {
        result = call => result,
        _ = cx.background_executor().timer(REVIEW_TIMEOUT) => None,
        _ = cancel.cancelled() => None,
    };
    let Some((text, usage)) = outcome else {
        return ReviewOutcome {
            verdict: ReviewVerdict::Ask {
                reason: REVIEWER_UNAVAILABLE_REASON.to_string(),
            },
            usage: None,
            model_name,
        };
    };
    ReviewOutcome {
        verdict: parse_verdict(&text).unwrap_or(ReviewVerdict::Ask {
            reason: "autopilot reviewer response unparseable; tool call denied".to_string(),
        }),
        usage,
        model_name,
    }
}
