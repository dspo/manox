//! AutoPilot safety-review side calls (model-agnostic core).
//!
//! The reviewer vets tool calls that would otherwise prompt the user. This
//! module owns the prompt construction, the timeout/cancellation race, and
//! the verdict parsing; the actual model call is injected as a [`ReviewerFn`]
//! closure so the harness backend adapts its own call machinery:
//!
//! - pi harness (`pi_approval`): the closure streams through the session
//!   model's `StreamFn` resolved from the `ModelRuntime`.
//!
//! Failures (reviewer unavailable, timeout, malformed response) **all**
//! downgrade to [`ReviewVerdict::Ask`] — the reviewer is fail-closed so a
//! broken autopilot path never silently widens access. Shared verdict types
//! live in [`crate::approval`].
//!
//! All entry points race against a deadline with `tokio::time::timeout` and
//! therefore must run inside a tokio runtime context.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::approval::{ReviewBatchOutcome, ReviewItem, ReviewOutcome, ReviewVerdict};
use crate::language::Language;
use crate::language_model::TokenUsage;

/// Hard deadline for one reviewer side call.
pub const REVIEW_TIMEOUT: Duration = Duration::from_secs(8);

/// The fail-closed reason attached to every call when the reviewer never
/// answered (timeout, cancellation, provider error, unparseable verdict).
pub const REVIEWER_UNAVAILABLE_REASON: &str = "autopilot reviewer unavailable; tool call denied";

/// Cap for any individual string field inside the reviewer prompt's tool
/// payload. The reviewer only needs enough context to judge safety; 2 KiB is
/// well past any plausible `command` / `path` / `pattern` while keeping a
/// 50 KiB `write_file` content from blowing the prompt budget.
pub const REVIEWER_FIELD_CAP: usize = 2048;

/// The rendered reviewer prompt handed to the [`ReviewerFn`]: a system prompt
/// (stable across calls, provider-side cacheable) plus the per-call user
/// payload.
#[derive(Debug, Clone)]
pub struct ReviewerRequest {
    pub system: String,
    pub user: String,
}

/// The reviewer's raw output plus the accounting metadata the caller records
/// for side-call metrics.
#[derive(Debug, Clone)]
pub struct ReviewerOutput {
    pub text: String,
    pub usage: Option<TokenUsage>,
    pub model_name: String,
}

/// Prompt in, raw reviewer output out. Errors fail closed (every call
/// escalates to the user).
pub type ReviewerFn = Arc<
    dyn Fn(ReviewerRequest) -> BoxFuture<'static, Result<ReviewerOutput, anyhow::Error>>
        + Send
        + Sync,
>;

#[derive(Debug, Deserialize)]
struct VerdictPayload {
    verdict: String,
    #[serde(default)]
    reason: Option<String>,
}
#[derive(Debug, Deserialize)]
struct BatchVerdictPayload {
    id: String,
    verdict: String,
    #[serde(default)]
    reason: Option<String>,
}

/// Vet every AutoPilot tool call from one assistant response in one side
/// request. Missing or malformed per-id verdicts fail closed independently.
pub async fn review_batch(
    reviewer: &ReviewerFn,
    items: &[ReviewItem],
    cwd: &Path,
    lang: Language,
    cancel: CancellationToken,
) -> ReviewBatchOutcome {
    review_batch_with_timeout(reviewer, items, cwd, lang, cancel, REVIEW_TIMEOUT).await
}

pub async fn review_batch_with_timeout(
    reviewer: &ReviewerFn,
    items: &[ReviewItem],
    cwd: &Path,
    lang: Language,
    cancel: CancellationToken,
    timeout: Duration,
) -> ReviewBatchOutcome {
    if items.is_empty() {
        return ReviewBatchOutcome {
            verdicts: HashMap::new(),
            usage: None,
            model_name: String::new(),
        };
    }
    let fallback = || ReviewVerdict::Ask {
        reason: REVIEWER_UNAVAILABLE_REASON.to_string(),
    };
    let fail_closed = || {
        items
            .iter()
            .map(|item| (item.id.clone(), fallback()))
            .collect::<HashMap<_, _>>()
    };

    let (system, user) = batch_prompt(items, cwd, lang);
    let call = reviewer(ReviewerRequest { system, user });
    let outcome = tokio::select! {
        result = call => result.ok(),
        _ = tokio::time::sleep(timeout) => None,
        _ = cancel.cancelled() => None,
    };
    let Some(output) = outcome else {
        return ReviewBatchOutcome {
            verdicts: fail_closed(),
            usage: None,
            model_name: String::new(),
        };
    };
    ReviewBatchOutcome {
        verdicts: parse_batch_verdicts(&output.text, items),
        usage: output.usage,
        model_name: output.model_name,
    }
}

/// Vet a single tool call under `AutoPilot`. Blocks until the reviewer
/// responds, the per-call timeout elapses, or `cancel` fires — every
/// non-success path returns [`ReviewVerdict::Ask`].
///
/// We deliberately do not include the thread's full message history: the
/// reviewer needs only the call itself plus a sliver of context (cwd) to
/// make a sound decision, and excluding history keeps the reviewer's own
/// provider-side prompt cache hot across calls.
pub async fn review(
    reviewer: &ReviewerFn,
    tool_name: &str,
    tool_input: &serde_json::Value,
    tool_title: &str,
    cwd: &Path,
    lang: Language,
    cancel: CancellationToken,
) -> ReviewOutcome {
    review_with_timeout(
        reviewer,
        tool_name,
        tool_input,
        tool_title,
        cwd,
        lang,
        cancel,
        REVIEW_TIMEOUT,
    )
    .await
}

// too_many_arguments: each parameter is a distinct review input the caller
// already holds as a separate owned value; bundling them into a struct would
// reshape the public side-call API for one call site. Mirrors the batch
// entry point's shape.
#[allow(clippy::too_many_arguments)]
pub async fn review_with_timeout(
    reviewer: &ReviewerFn,
    tool_name: &str,
    tool_input: &serde_json::Value,
    tool_title: &str,
    cwd: &Path,
    lang: Language,
    cancel: CancellationToken,
    timeout: Duration,
) -> ReviewOutcome {
    let (system, user_prompt) = single_prompt(tool_name, tool_input, tool_title, cwd, lang);

    let call = reviewer(ReviewerRequest {
        system,
        user: user_prompt,
    });
    let outcome = tokio::select! {
        result = call => result.ok(),
        _ = tokio::time::sleep(timeout) => None,
        _ = cancel.cancelled() => None,
    };
    let Some(output) = outcome else {
        return ReviewOutcome {
            verdict: ReviewVerdict::Ask {
                reason: REVIEWER_UNAVAILABLE_REASON.to_string(),
            },
            usage: None,
            model_name: String::new(),
        };
    };
    ReviewOutcome {
        verdict: parse_verdict(&output.text).unwrap_or(ReviewVerdict::Ask {
            reason: "autopilot reviewer response unparseable; tool call denied".to_string(),
        }),
        usage: output.usage,
        model_name: output.model_name,
    }
}

/// Render the batch-review system + user prompts (shared by every adapter).
pub fn batch_prompt(items: &[ReviewItem], cwd: &Path, lang: Language) -> (String, String) {
    let calls: Vec<serde_json::Value> = items
        .iter()
        .map(|item| {
            serde_json::json!({
                "id": item.id,
                "tool_name": item.tool_name,
                "tool_title": item.tool_title,
                "tool_input": truncate_tool_input(&item.tool_input),
            })
        })
        .collect();
    let batch_override = match lang {
        Language::En => {
            "\n\n## Batch override\nThe user message contains an array of calls. Review every call and return only a JSON array with one object per id: [{\"id\":\"...\",\"verdict\":\"ALLOW|ASK\",\"reason\":\"<=200 chars\"}]. This replaces the single-object output format above for this request."
        }
        Language::ZhCn => {
            "\n\n## 批量覆盖规则\n用户消息包含一组调用。逐项审核，只返回 JSON 数组，每个 id 一个对象：[{\"id\":\"...\",\"verdict\":\"ALLOW|ASK\",\"reason\":\"不超过200字\"}]。本规则替代上面的单对象输出格式。"
        }
    };
    let system = format!(
        "{}{}",
        crate::prompt::render_static(crate::prompt::PromptTemplate::SideCallApprovalSystem, lang,)
            .expect("approval system prompt render"),
        batch_override
    );
    let user = serde_json::json!({
        "cwd": cwd.display().to_string(),
        "calls": calls,
    })
    .to_string();
    (system, user)
}

/// Render the single-call review system + user prompts (shared by every
/// adapter).
pub fn single_prompt(
    tool_name: &str,
    tool_input: &serde_json::Value,
    tool_title: &str,
    cwd: &Path,
    lang: Language,
) -> (String, String) {
    let user_prompt = crate::prompt::render(
        crate::prompt::PromptTemplate::SideCallApprovalUser,
        lang,
        &crate::prompt::ApprovalReviewPromptData {
            cwd: cwd.display().to_string(),
            tool_name: tool_name.to_string(),
            tool_title: tool_title.to_string(),
            tool_input: serde_json::to_string_pretty(&truncate_tool_input(tool_input))
                .unwrap_or_else(|_| "<unprintable input>".to_string()),
        },
    )
    .expect("approval user prompt render");
    let system =
        crate::prompt::render_static(crate::prompt::PromptTemplate::SideCallApprovalSystem, lang)
            .expect("approval system prompt render");
    (system, user_prompt)
}

pub fn parse_batch_verdicts(text: &str, items: &[ReviewItem]) -> HashMap<String, ReviewVerdict> {
    let trimmed = text.trim();
    let json = serde_json::from_str::<Vec<BatchVerdictPayload>>(trimmed)
        .ok()
        .or_else(|| {
            let start = trimmed.find('[')?;
            let end = trimmed.rfind(']')?;
            serde_json::from_str(&trimmed[start..=end]).ok()
        });
    let mut parsed = HashMap::new();
    if let Some(payloads) = json {
        let expected: std::collections::HashSet<&str> =
            items.iter().map(|item| item.id.as_str()).collect();
        for payload in payloads {
            if !expected.contains(payload.id.as_str()) || parsed.contains_key(&payload.id) {
                continue;
            }
            if let Some(verdict) = verdict_from(VerdictPayload {
                verdict: payload.verdict,
                reason: payload.reason,
            }) {
                parsed.insert(payload.id, verdict);
            }
        }
    }
    for item in items {
        parsed
            .entry(item.id.clone())
            .or_insert_with(|| ReviewVerdict::Ask {
                reason: "autopilot reviewer verdict missing or malformed; tool call denied"
                    .to_string(),
            });
    }
    parsed
}

/// Deep-clone a `serde_json::Value`, replacing any string field longer than
/// [`REVIEWER_FIELD_CAP`] with a truncated form: the longest char-aligned
/// prefix not exceeding [`REVIEWER_FIELD_CAP`] bytes, plus a byte-length
/// marker. The original value is left intact — only the reviewer's serialized
/// view is affected.
pub fn truncate_tool_input(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::String(s) => {
            if s.len() <= REVIEWER_FIELD_CAP {
                v.clone()
            } else {
                // Snap the cut to the nearest preceding char boundary so a
                // multi-byte UTF-8 sequence is never split mid-character.
                let head_end = s.floor_char_boundary(REVIEWER_FIELD_CAP);
                let head = &s[..head_end];
                serde_json::Value::String(format!(
                    "{head}…[truncated {} bytes]",
                    s.len() - head_end
                ))
            }
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(truncate_tool_input).collect())
        }
        serde_json::Value::Object(o) => {
            let mut m = serde_json::Map::with_capacity(o.len());
            for (k, v) in o {
                m.insert(k.clone(), truncate_tool_input(v));
            }
            serde_json::Value::Object(m)
        }
        _ => v.clone(),
    }
}

pub fn parse_verdict(text: &str) -> Option<ReviewVerdict> {
    let trimmed = text.trim();
    // Plain JSON: just parse it.
    if let Some(v) = try_parse_payload(trimmed) {
        return verdict_from(v);
    }
    // Prose-wrapped JSON. The reviewer prompt forbids extra text, but
    // models occasionally add a preamble or, worse, an example in the same
    // response. Take the most-recently-emitted balanced `{...}` block so a
    // trailing format example doesn't swallow the actual answer.
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
            return verdict_from(payload);
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

fn verdict_from(payload: VerdictPayload) -> Option<ReviewVerdict> {
    let reason = payload
        .reason
        .map(|r| r.trim().to_string())
        .filter(|r| !r.is_empty());
    match payload.verdict.to_ascii_uppercase().as_str() {
        "ALLOW" => Some(ReviewVerdict::Allow),
        "ASK" => Some(ReviewVerdict::Ask {
            reason: reason.unwrap_or_else(|| "autopilot reviewer denied the call".to_string()),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reviewer_returning(text: &'static str) -> ReviewerFn {
        Arc::new(move |_req: ReviewerRequest| {
            Box::pin(async move {
                Ok(ReviewerOutput {
                    text: text.to_string(),
                    usage: None,
                    model_name: "reviewer-test".to_string(),
                })
            })
        })
    }

    #[test]
    fn parse_verdict_accepts_plain_json() {
        let v = parse_verdict(r#"{"verdict":"ALLOW"}"#).unwrap();
        assert_eq!(v, ReviewVerdict::Allow);
    }

    #[test]
    fn parse_verdict_prefers_last_json_object_in_prose() {
        // The reviewer prompt forbids prose around the JSON verdict. Models
        // occasionally wrap the answer in text or, worse, include a format
        // example before its actual verdict. The brace-scan should prefer the
        // most recently emitted object, not the first one.
        let v = parse_verdict(
            r#"Format: {"verdict":"ALLOW"} and my answer: {"verdict":"ASK","reason":"risky"}"#,
        )
        .unwrap();
        assert_eq!(
            v,
            ReviewVerdict::Ask {
                reason: "risky".into()
            }
        );
    }

    #[test]
    fn truncate_tool_input_caps_long_string_fields() {
        let big = "x".repeat(10_000);
        let v = serde_json::json!({
            "command": "ls -la",
            "content": big,
            "nested": { "deep": big.clone() },
            "list": ["short", big.clone()],
        });
        let out = truncate_tool_input(&v);
        assert_eq!(out["command"], "ls -la");
        let content = out["content"].as_str().unwrap();
        assert!(content.starts_with(&"x".repeat(REVIEWER_FIELD_CAP)));
        assert!(content.contains("truncated"));
        let deep = out["nested"]["deep"].as_str().unwrap();
        assert!(deep.contains("truncated"));
        let arr = out["list"].as_array().unwrap();
        assert_eq!(arr[0], "short");
        assert!(arr[1].as_str().unwrap().contains("truncated"));
    }

    #[test]
    fn truncate_tool_input_truncates_multibyte_at_char_boundary() {
        // REVIEWER_FIELD_CAP (2048) is not divisible by 3, so byte-slicing a
        // long CJK string would land mid-character and panic. The cut must
        // snap to the preceding char boundary instead.
        let big = "文".repeat(4_000); // 12_000 bytes
        let out = truncate_tool_input(&serde_json::json!({ "content": big }));
        let content = out["content"].as_str().unwrap();
        assert!(content.starts_with('文'));
        assert!(content.contains("…[truncated "));
        let head_end = content.find('…').unwrap();
        assert!(head_end <= REVIEWER_FIELD_CAP);
        assert!(content.is_char_boundary(head_end));
    }

    #[test]
    fn batch_parser_fails_closed_per_missing_or_malformed_id() {
        let items = vec![
            ReviewItem {
                id: "a".into(),
                tool_name: "Read".into(),
                tool_title: "read".into(),
                tool_input: serde_json::json!({}),
            },
            ReviewItem {
                id: "b".into(),
                tool_name: "Bash".into(),
                tool_title: "bash".into(),
                tool_input: serde_json::json!({}),
            },
            ReviewItem {
                id: "c".into(),
                tool_name: "Write".into(),
                tool_title: "write".into(),
                tool_input: serde_json::json!({}),
            },
        ];
        let parsed = parse_batch_verdicts(
            r#"[{"id":"a","verdict":"ALLOW"},{"id":"b","verdict":"MAYBE"}]"#,
            &items,
        );
        assert_eq!(parsed["a"], ReviewVerdict::Allow);
        assert!(matches!(parsed["b"], ReviewVerdict::Ask { .. }));
        assert!(matches!(parsed["c"], ReviewVerdict::Ask { .. }));
    }

    #[tokio::test]
    async fn batch_timeout_fails_closed_for_every_id() {
        // A reviewer that never answers: the deadline must fire and every
        // call must fail closed to Ask.
        let reviewer: ReviewerFn = Arc::new(|_req: ReviewerRequest| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(ReviewerOutput {
                    text: String::new(),
                    usage: None,
                    model_name: String::new(),
                })
            })
        });
        let items = vec![
            ReviewItem {
                id: "a".into(),
                tool_name: "Bash".into(),
                tool_title: "bash".into(),
                tool_input: serde_json::json!({"command":"echo a"}),
            },
            ReviewItem {
                id: "b".into(),
                tool_name: "Write".into(),
                tool_title: "write".into(),
                tool_input: serde_json::json!({"path":"b"}),
            },
        ];
        let outcome = review_batch_with_timeout(
            &reviewer,
            &items,
            Path::new("/tmp"),
            Language::En,
            CancellationToken::new(),
            Duration::from_millis(5),
        )
        .await;
        assert_eq!(outcome.verdicts.len(), 2);
        assert!(
            outcome
                .verdicts
                .values()
                .all(|verdict| matches!(verdict, ReviewVerdict::Ask { .. }))
        );
        assert!(outcome.usage.is_none());
    }

    #[tokio::test]
    async fn batch_verdicts_round_trip_through_reviewer() {
        let reviewer = reviewer_returning(
            r#"[{"id":"a","verdict":"ALLOW"},{"id":"b","verdict":"ASK","reason":"risky"}]"#,
        );
        let items = vec![
            ReviewItem {
                id: "a".into(),
                tool_name: "Read".into(),
                tool_title: "read".into(),
                tool_input: serde_json::json!({}),
            },
            ReviewItem {
                id: "b".into(),
                tool_name: "Bash".into(),
                tool_title: "bash".into(),
                tool_input: serde_json::json!({"command":"rm -rf /"}),
            },
        ];
        let outcome = review_batch(
            &reviewer,
            &items,
            Path::new("/tmp"),
            Language::En,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome.verdicts["a"], ReviewVerdict::Allow);
        assert_eq!(
            outcome.verdicts["b"],
            ReviewVerdict::Ask {
                reason: "risky".into()
            }
        );
        assert_eq!(outcome.model_name, "reviewer-test");
    }

    #[tokio::test]
    async fn single_review_fails_closed_on_error() {
        let reviewer: ReviewerFn = Arc::new(|_req: ReviewerRequest| {
            Box::pin(async move { Err(anyhow::anyhow!("provider down")) })
        });
        let outcome = review(
            &reviewer,
            "Bash",
            &serde_json::json!({"command":"echo hi"}),
            "echo hi",
            Path::new("/tmp"),
            Language::En,
            CancellationToken::new(),
        )
        .await;
        assert!(matches!(outcome.verdict, ReviewVerdict::Ask { .. }));
    }

    #[tokio::test]
    async fn single_review_parses_allow() {
        let reviewer = reviewer_returning(r#"{"verdict":"ALLOW"}"#);
        let outcome = review(
            &reviewer,
            "Read",
            &serde_json::json!({"path":"a.txt"}),
            "read a.txt",
            Path::new("/tmp"),
            Language::En,
            CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome.verdict, ReviewVerdict::Allow);
    }
}
