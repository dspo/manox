//! Goal-mode evaluator.
//!
//! When a thread has an active goal, [`evaluate`] runs after each natural
//! turn end. It makes a single-shot LLM call (no tools, no streaming beyond
//! draining the response) and returns whether the user-defined completion
//! condition is met plus a one-line reason shown in the goal status popover.
//!
//! Failures (LLM unavailable, timeout, malformed response) **all** return
//! `satisfied: false` with a machine reason — the evaluator is fail-open to
//! continuation rather than fail-closed, because falsely declaring a goal
//! complete is worse than an extra round. A separate evaluation cap in
//! `Thread` bounds the loop either way.
//!
//! The evaluator prompt lives in the `side_call/goal_system.tera.md` template
//! and is rendered at the request-build boundary. It is model-facing text; it
//! is bilingual via the thread's `agent_language` (en / zh-CN mirrors) and is
//! never routed through the `i18n` bundle (which only carries UI chrome).

use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use gpui::AsyncApp;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::language_model::{
    AnyLanguageModel, LanguageModelCompletionEvent, LanguageModelRequest,
    LanguageModelRequestMessage, MessageContent, Role, TokenUsage,
};
use crate::message::Message;
use crate::thread::truncate_summary;

/// Per-call hard timeout. The evaluator judges a whole turn's outcome, so it
/// is allowed a little longer than a streaming chunk — but it runs on the
/// user's critical path between turns, so it must stay bounded.
const EVAL_TIMEOUT: Duration = Duration::from_secs(15);

/// Cap on each sampled message snippet forwarded to the evaluator. The
/// evaluator needs the gist of the latest user/assistant exchange, not the
/// full transcript — a sliver keeps its own provider-side prompt cache hot
/// and the call cheap.
const MESSAGE_SAMPLE_CHARS: usize = 800;

/// The evaluator's verdict for one turn.
#[derive(Debug, Clone)]
pub struct GoalVerdict {
    pub satisfied: bool,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct GoalEvaluation {
    pub verdict: GoalVerdict,
    pub usage: Option<TokenUsage>,
}

#[derive(Debug, Deserialize)]
struct VerdictPayload {
    satisfied: bool,
    #[serde(default)]
    reason: Option<String>,
}

/// Vet whether `condition` is met given the conversation so far. Blocks until
/// the evaluator responds, the per-call timeout elapses, or `cancel` fires —
/// every non-success path returns `satisfied: false`.
///
/// `model` is the same `AnyLanguageModel` the owning thread uses for its main
/// loop. Only the latest user and assistant messages are sampled (truncated),
/// mirroring the title-turn's context window: the evaluator judges "what did
/// this turn accomplish", not the full history.
pub async fn evaluate(
    model: &AnyLanguageModel,
    condition: &str,
    messages: &[Message],
    lang: crate::language::Language,
    cancel: CancellationToken,
    cx: &AsyncApp,
) -> GoalEvaluation {
    let last_user = last_text(messages, Role::User)
        .map(|t| truncate_summary(&t, MESSAGE_SAMPLE_CHARS))
        .unwrap_or_default();
    let last_assistant = last_text(messages, Role::Assistant)
        .map(|t| truncate_summary(&t, MESSAGE_SAMPLE_CHARS))
        .unwrap_or_default();
    let user_prompt = crate::prompt::render(
        crate::prompt::PromptTemplate::SideCallGoalUser,
        lang,
        &crate::prompt::GoalEvalPromptData {
            condition: condition.to_string(),
            last_user,
            last_assistant,
        },
    )
    .expect("goal user prompt render");

    let request = LanguageModelRequest {
        messages: vec![
            LanguageModelRequestMessage {
                role: Role::System,
                content: vec![MessageContent::Text(
                    crate::prompt::render_static(
                        crate::prompt::PromptTemplate::SideCallGoalSystem,
                        lang,
                    )
                    .expect("goal system prompt render"),
                )],
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
        reasoning_effort: crate::settings::side_call_effort(
            &crate::settings::side_calls().goal_policy(),
            crate::language_model::ReasoningEffort::Low,
        ),
        max_output_tokens: crate::settings::side_call_output_cap(
            crate::settings::side_calls().goal_policy(),
        ),
    };

    let model = Arc::clone(model);
    let call = async move {
        let stream = match model.stream_completion(request, cx).await {
            Ok(s) => s,
            Err(_) => return None,
        };
        futures::pin_mut!(stream);
        let mut text = String::new();
        let mut usage = None;
        while let Some(event) = stream.next().await {
            let Ok(event) = event else { return None };
            match event {
                LanguageModelCompletionEvent::Text(delta) => text.push_str(&delta),
                LanguageModelCompletionEvent::UsageUpdate(value) => usage = Some(value),
                LanguageModelCompletionEvent::Stop(_) => break,
                _ => {}
            }
        }
        Some((text, usage))
    };

    // Race the evaluator call against a hard deadline and cancellation.
    // `evaluate` runs inline on the gpui foreground executor — there is no
    // tokio runtime context here, so `tokio::time::timeout` would panic at
    // `Handle::current` and abort the process. The timer comes from the gpui
    // executor instead; `call` is safe on the foreground executor because
    // `stream_completion` bridges its HTTP work onto the global tokio runtime.
    let outcome = tokio::select! {
        result = call => result,
        _ = cx.background_executor().timer(EVAL_TIMEOUT) => None,
        _ = cancel.cancelled() => None,
    };
    let Some((text, usage)) = outcome else {
        return GoalEvaluation {
            verdict: GoalVerdict {
                satisfied: false,
                reason: "evaluator unavailable; continuing".to_string(),
            },
            usage: None,
        };
    };

    GoalEvaluation {
        verdict: parse_verdict(&text).unwrap_or(GoalVerdict {
            satisfied: false,
            reason: "evaluator response unparseable; continuing".to_string(),
        }),
        usage,
    }
}

/// The trimmed concatenated `Text` blocks of the most recent message with the
/// given role. `User` skips tool-result user messages (which carry no `Text`).
fn last_text(messages: &[Message], role: Role) -> Option<String> {
    let mut buf = String::new();
    for m in messages.iter().rev() {
        if m.role != role {
            continue;
        }
        for c in &m.content {
            if let MessageContent::Text(t) = c {
                buf.push_str(t);
            }
        }
        let trimmed = buf.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
        buf.clear();
    }
    None
}

fn parse_verdict(text: &str) -> Option<GoalVerdict> {
    let trimmed = text.trim();
    if let Some(v) = try_parse_payload(trimmed) {
        return Some(verdict_from(v));
    }
    // Prose-wrapped JSON. The prompt forbids extra text, but models
    // occasionally add a preamble or a fenced example. Take the most-recently
    // emitted balanced `{...}` block so a trailing example doesn't swallow
    // the actual answer.
    let bytes = trimmed.as_bytes();
    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        if bytes[i] != b'{' {
            continue;
        }
        if let Some(end) = find_matching_close(bytes, i)
            && let Some(payload) = try_parse_payload(&trimmed[i..=end])
        {
            return Some(verdict_from(payload));
        }
    }
    None
}

fn find_matching_close(bytes: &[u8], start: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    let mut j = start;
    while j < bytes.len() {
        let c = bytes[j];
        if escape {
            escape = false;
        } else if c == b'\\' {
            escape = true;
        } else if c == b'"' {
            in_string = !in_string;
        } else if !in_string {
            match c {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
        }
        j += 1;
    }
    None
}

fn try_parse_payload(s: &str) -> Option<VerdictPayload> {
    serde_json::from_str(s).ok()
}

fn verdict_from(payload: VerdictPayload) -> GoalVerdict {
    let reason = payload
        .reason
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| {
            if payload.satisfied {
                "condition met"
            } else {
                "not yet met"
            }
            .to_string()
        });
    GoalVerdict {
        satisfied: payload.satisfied,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_satisfied_with_reason() {
        let v = parse_verdict(r#"{"satisfied":true,"reason":"test passes"}"#).unwrap();
        assert!(v.satisfied);
        assert_eq!(v.reason, "test passes");
    }

    #[test]
    fn parses_unsatisfied_without_reason() {
        let v = parse_verdict(r#"{"satisfied":false}"#).unwrap();
        assert!(!v.satisfied);
        assert_eq!(v.reason, "not yet met");
    }

    #[test]
    fn parses_satisfied_without_reason() {
        let v = parse_verdict(r#"{"satisfied":true}"#).unwrap();
        assert!(v.satisfied);
        assert_eq!(v.reason, "condition met");
    }

    #[test]
    fn tolerates_surrounding_prose_and_fences() {
        let v = parse_verdict(
            "Here is my judgment:\n```json\n{\"satisfied\":true,\"reason\":\"done\"}\n```\n",
        )
        .unwrap();
        assert!(v.satisfied);
        assert_eq!(v.reason, "done");
    }

    #[test]
    fn falls_through_on_garbage() {
        assert!(parse_verdict("not json at all").is_none());
    }

    #[test]
    fn picks_latest_object_when_format_example_precedes() {
        let v = parse_verdict(
            r#"Format: {"satisfied":true} and my answer: {"satisfied":false,"reason":"still working"}"#,
        )
        .unwrap();
        assert!(!v.satisfied);
        assert_eq!(v.reason, "still working");
    }

    #[test]
    fn last_text_skips_empty_user_tool_result() {
        use crate::language_model::MessageContent;
        // A tool result is role User with no Text block; the sampler must
        // skip it and return the prior real user text.
        let msgs = vec![
            Message::user_with_content(vec![MessageContent::Text("real question".into())]),
            Message::user_with_content(vec![]),
            Message::assistant(vec![MessageContent::Text("answer".into())]),
        ];
        let u = last_text(&msgs, Role::User).unwrap();
        assert_eq!(u, "real question");
        let a = last_text(&msgs, Role::Assistant).unwrap();
        assert_eq!(a, "answer");
    }
}
