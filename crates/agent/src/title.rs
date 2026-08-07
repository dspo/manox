//! LLM-based thread title generation for the pi harness.
//!
//! Port of the retired manox harness's title lifecycle (two modes: first
//! title + topic-shift re-eval), adapted to the pi kernel's wire messages
//! and the runtime-resolver side-call seam (same pattern as the approval
//! reviewer in `pi_approval`). Pure request-construction/sanitization logic
//! lives here; the cadence state machine rides the pi actor (`pi_engine`).

use pi::coding_agent::ModelRuntime;
use pi::types::{AgentMessage, ContentBlock, Model as PiModel, StreamOptions};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::language::Language;
use crate::prompt::{self, PromptTemplate};

/// Upper bound on raw streamed chars accumulated before stopping. Titles are
/// short; this caps consumption so a chatty model cannot run on.
pub const MAX_RAW_CHARS: usize = 120;

/// Per-message char cap sent to the model. Keeps the title request tiny and
/// cheap regardless of total conversation length.
pub const MESSAGE_SAMPLE_CHARS: usize = 800;

/// Sentinel the model emits when the latest message does NOT signal a new
/// topic. Compared case-insensitively after stripping trailing punctuation.
pub const UNCHANGED_SENTINEL: &str = "UNCHANGED";

/// Whether a turn at `user_count` total user messages should re-evaluate the
/// title. The first 3 user turns check every turn; thereafter every 5th
/// (turns 8, 13, 18, …). The first-title path (`title` still `None`) bypasses
/// this cadence and evaluates as soon as a reply exists.
pub fn should_retitle(user_count: usize) -> bool {
    if user_count <= 3 {
        return true;
    }
    (user_count - 3).is_multiple_of(5)
}

/// Whether an already-sanitized title string is the "no change" sentinel.
/// Accepts trailing punctuation (`UNCHANGED.` / `UNCHANGED。`) for robustness.
pub fn is_unchanged(sanitized: &str) -> bool {
    let trimmed = sanitized.trim_end_matches([
        '.', '。', '!', '！', '?', '？', ',', '，', ';', '；', ':', '：',
    ]);
    trimmed.eq_ignore_ascii_case(UNCHANGED_SENTINEL)
}

/// Trim, strip wrapping quotes and a leading `Title:`/`标题：` prefix, collapse
/// internal whitespace to one line, and cap at the summary length.
pub fn sanitize_title(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    strip_wrapping_quotes(&mut s);
    strip_title_prefix(&mut s);
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&collapsed, 60)
}

/// Build the title side-call conversation from the pi transcript. Two modes,
/// mirroring the manox requests:
/// - first title (`current_title` is `None`): first user message + latest
///   assistant reply + the first-title instruction;
/// - topic-shift re-eval: latest user message + latest assistant reply + the
///   topic-shift instruction naming the current title.
///
/// `None` when the transcript lacks the assistant reply the request is
/// built around (the caller gates on it anyway).
pub fn build_title_messages(
    messages: &[AgentMessage],
    current_title: Option<&str>,
    lang: Language,
) -> Option<Vec<AgentMessage>> {
    let assistant = last_assistant_text(messages)?;
    let mut out = Vec::with_capacity(3);
    match current_title {
        None => {
            if let Some(text) = first_user_text(messages) {
                out.push(user_message(truncate_chars(&text, MESSAGE_SAMPLE_CHARS)));
            }
            out.push(assistant_message(truncate_chars(
                &assistant,
                MESSAGE_SAMPLE_CHARS,
            )));
            out.push(user_message(
                prompt::render_static(PromptTemplate::TitleFirstInstruction, lang)
                    .expect("title first instruction render"),
            ));
        }
        Some(title) => {
            if let Some(text) = last_user_text(messages) {
                out.push(user_message(truncate_chars(&text, MESSAGE_SAMPLE_CHARS)));
            }
            out.push(assistant_message(truncate_chars(
                &assistant,
                MESSAGE_SAMPLE_CHARS,
            )));
            out.push(user_message(
                prompt::render(
                    PromptTemplate::TitleTopicShiftInstruction,
                    lang,
                    &prompt::TopicShiftData {
                        current_title: title.to_string(),
                        unchanged_sentinel: UNCHANGED_SENTINEL,
                    },
                )
                .expect("topic shift instruction render"),
            ));
        }
    }
    Some(out)
}

/// Stream a title through the session model's `StreamFn`: resolve via the
/// runtime, run the side call (temperature 0.3, no tools, no thinking, the
/// side-call output cap), then reduce the reply to its first line and
/// sanitize. Returns an empty string when the model produced no usable text;
/// the caller checks [`is_unchanged`] before adopting.
pub async fn stream_title(
    runtime: &ModelRuntime,
    model: &PiModel,
    convo: Vec<AgentMessage>,
) -> anyhow::Result<String> {
    let stream_fn = (runtime.resolver())(model)?;
    let context = pi::types::AgentContext {
        system_prompt: String::new(),
        messages: convo,
        tools: std::sync::Arc::new([]),
        model: model.clone(),
        thinking_level: None,
        cache_retention: pi::types::CacheRetention::default(),
        session_id: None,
        stream_options: StreamOptions {
            temperature: Some(0.3),
            max_tokens: crate::settings::side_call_output_cap(
                crate::settings::side_calls().title_policy(),
            )
            .map(|cap| cap as usize),
            ..Default::default()
        },
        metadata: std::collections::HashMap::new(),
    };
    let (tx, mut rx) = mpsc::channel(32);
    let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let message = stream_fn
        .stream(&context, CancellationToken::new(), tx)
        .await?;
    let _ = drain.await;
    let AgentMessage::Assistant { content, .. } = message else {
        return Ok(String::new());
    };
    let text: String = content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    // Titles are one line: cut at the first newline, cap the raw chars, then
    // sanitize (manox parity — the stream loop enforced the same bounds).
    let first_line = text
        .split(['\n', '\r'])
        .next()
        .unwrap_or_default()
        .to_string();
    let capped: String = first_line.chars().take(MAX_RAW_CHARS).collect();
    Ok(sanitize_title(&capped))
}

// ── transcript sampling (pi wire messages) ──────────────────────────────────

/// Count user messages carrying text — the title cadence's turn counter.
pub fn count_user_messages(messages: &[AgentMessage]) -> usize {
    messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::User { .. }) && message_text(m).is_some())
        .count()
}

fn first_user_text(messages: &[AgentMessage]) -> Option<String> {
    messages
        .iter()
        .filter(|m| matches!(m, AgentMessage::User { .. }))
        .find_map(message_text)
}

fn last_user_text(messages: &[AgentMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter(|m| matches!(m, AgentMessage::User { .. }))
        .find_map(message_text)
}

fn last_assistant_text(messages: &[AgentMessage]) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter(|m| matches!(m, AgentMessage::Assistant { .. }))
        .find_map(message_text)
}

/// Concatenate all `Text` blocks of a message into one trimmed string.
fn message_text(m: &AgentMessage) -> Option<String> {
    let content = match m {
        AgentMessage::User { content, .. } | AgentMessage::Assistant { content, .. } => content,
        _ => return None,
    };
    let mut buf = String::new();
    for block in content {
        if let ContentBlock::Text { text, .. } = block {
            buf.push_str(text);
        }
    }
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn user_message(text: String) -> AgentMessage {
    AgentMessage::User {
        content: vec![ContentBlock::Text {
            text,
            signature: None,
        }],
        timestamp: chrono::Utc::now(),
    }
}

fn assistant_message(text: String) -> AgentMessage {
    AgentMessage::Assistant {
        content: vec![ContentBlock::Text {
            text,
            signature: None,
        }],
        model: String::new(),
        provider: String::new(),
        api: String::new(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        stop_reason: None,
        raw_stop_reason: None,
        usage: Box::new(pi::types::Usage::default()),
        error_message: None,
        timestamp: chrono::Utc::now(),
    }
}

// ── sanitization helpers ────────────────────────────────────────────────────

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let one_line = s.replace('\n', " ");
    if one_line.chars().count() > max_chars {
        let t: String = one_line.chars().take(max_chars).collect();
        format!("{t}…")
    } else {
        one_line
    }
}

/// Repeatedly strip one matched wrapping pair until the ends no longer match.
fn strip_wrapping_quotes(s: &mut String) {
    const PAIRS: [(char, char); 5] = [
        ('"', '"'),
        ('\'', '\''),
        ('「', '」'),
        ('『', '』'),
        ('《', '》'),
    ];
    loop {
        let count = s.chars().count();
        if count < 2 {
            return;
        }
        let first = s.chars().next().unwrap();
        let last = s.chars().last().unwrap();
        if !PAIRS.iter().any(|(o, c)| first == *o && last == *c) {
            return;
        }
        // Drop the first and last char (char-boundary safe).
        let inner: String = s.chars().skip(1).take(count - 2).collect();
        *s = inner.trim().to_string();
    }
}

fn strip_title_prefix(s: &mut String) {
    for prefix in ["Title:", "Title：", "标题：", "标题:"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            *s = rest.trim_start().to_string();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_wrapping_quotes() {
        assert_eq!(sanitize_title("\"Fix login bug\""), "Fix login bug");
        assert_eq!(sanitize_title("'修复登录'"), "修复登录");
        assert_eq!(sanitize_title("「修复登录」"), "修复登录");
    }

    #[test]
    fn sanitize_strips_title_prefix() {
        assert_eq!(sanitize_title("Title: 修复登录 bug"), "修复登录 bug");
        assert_eq!(sanitize_title("标题：修复登录"), "修复登录");
        assert_eq!(sanitize_title("Title：hello"), "hello");
    }

    #[test]
    fn sanitize_collapses_newlines_and_caps_length() {
        assert_eq!(sanitize_title("修复\n登录\n第二行"), "修复 登录 第二行");
        let long = "x".repeat(80);
        let out = sanitize_title(&long);
        assert!(out.chars().count() <= 61);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn should_retitle_cadence() {
        // First 3 user turns: every turn.
        for n in [1, 2, 3] {
            assert!(should_retitle(n), "turn {n} should re-eval");
        }
        // Turns 4-7: skip.
        for n in [4, 5, 6, 7] {
            assert!(!should_retitle(n), "turn {n} should skip");
        }
        // Every 5th thereafter: 8, 13, 18.
        for n in [8, 13, 18] {
            assert!(should_retitle(n), "turn {n} should re-eval");
        }
    }

    #[test]
    fn unchanged_sentinel_detection() {
        assert!(is_unchanged("UNCHANGED"));
        assert!(is_unchanged("unchanged"));
        assert!(is_unchanged("UNCHANGED."));
        assert!(is_unchanged("UNCHANGED。"));
        assert!(!is_unchanged("Fix login bug"));
        assert!(!is_unchanged("UNCHANGED topic"));
    }

    #[test]
    fn build_first_title_request_shape() {
        let messages = vec![
            user_message("first question".into()),
            assistant_message("first answer".into()),
            user_message("second question".into()),
            assistant_message("second answer".into()),
        ];
        let convo = build_title_messages(&messages, None, Language::En).unwrap();
        assert_eq!(convo.len(), 3);
        // First user message (not the latest) seeds the first-title request.
        assert!(matches!(&convo[0], AgentMessage::User { content, .. }
            if matches!(&content[0], ContentBlock::Text { text, .. } if text == "first question")));
        assert!(matches!(&convo[1], AgentMessage::Assistant { .. }));
        assert!(matches!(&convo[2], AgentMessage::User { .. }));
    }

    #[test]
    fn build_topic_shift_request_shape() {
        let messages = vec![
            user_message("latest question".into()),
            assistant_message("latest answer".into()),
        ];
        let convo = build_title_messages(&messages, Some("Old Title"), Language::En).unwrap();
        assert_eq!(convo.len(), 3);
        assert!(matches!(&convo[0], AgentMessage::User { content, .. }
            if matches!(&content[0], ContentBlock::Text { text, .. } if text == "latest question")));
        // The instruction names the current title.
        let AgentMessage::User { content, .. } = &convo[2] else {
            panic!("instruction must be a user message")
        };
        let ContentBlock::Text { text, .. } = &content[0] else {
            panic!("instruction must be text")
        };
        assert!(text.contains("Old Title"));
        assert!(text.contains(UNCHANGED_SENTINEL));
    }

    #[test]
    fn build_request_requires_assistant_reply() {
        let messages = vec![user_message("only a question".into())];
        assert!(build_title_messages(&messages, None, Language::En).is_none());
    }

    #[test]
    fn count_user_messages_counts_text_only() {
        let messages = vec![
            user_message("one".into()),
            assistant_message("reply".into()),
            user_message("   ".into()),
            user_message("two".into()),
        ];
        assert_eq!(count_user_messages(&messages), 2);
    }
}
