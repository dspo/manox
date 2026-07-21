//! LLM-based thread title generation.
//!
//! `Thread::maybe_generate_title` runs in two modes:
//! - **First title** (`title` still `None`): after the first natural terminal
//!   turn, build a request from the first user message + most recent assistant
//!   reply and stream a concise title.
//! - **Topic-shift re-eval** (`title` already set): on a cadence (first 3 user
//!   turns, then every 5th), build a request naming the current title and ask
//!   the model to either emit a new title or the literal word `UNCHANGED`.
//!
//! The title is sanitized (quotes and `Title:` prefixes stripped, collapsed to
//! one line, length-capped) before being stored on the `Thread` and persisted
//! via `save_thread`. The mechanical first-message summary stays as the interim
//! display until the LLM title lands.

use anyhow::Result;
use futures::StreamExt;
use gpui::AsyncApp;

use crate::language_model::{
    AnyLanguageModel, LanguageModelCompletionEvent, LanguageModelRequest,
    LanguageModelRequestMessage, MessageContent, Role,
};
use crate::message::Message;
use crate::thread::truncate_summary;

/// Upper bound on raw streamed chars accumulated before stopping. Titles are
/// short; this caps consumption so a chatty model cannot run on.
const MAX_RAW_CHARS: usize = 120;

/// Per-message char cap sent to the model. Keeps the title request tiny and
/// cheap regardless of total conversation length.
const MESSAGE_SAMPLE_CHARS: usize = 800;

/// Build a minimal first-title request: the first user message plus the most
/// recent assistant reply (each truncated), then the title instruction. No
/// tools, no system prompt. Callers gate on the presence of an assistant
/// reply before invoking.
pub fn build_title_request(
    messages: &[Message],
    lang: crate::language::Language,
) -> LanguageModelRequest {
    let mut req_messages: Vec<LanguageModelRequestMessage> = Vec::new();

    if let Some(text) = first_user_text(messages) {
        req_messages.push(LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text(truncate_str(
                &text,
                MESSAGE_SAMPLE_CHARS,
            ))],
            cache: false,
        });
    }
    if let Some(text) = last_assistant_text(messages) {
        req_messages.push(LanguageModelRequestMessage {
            role: Role::Assistant,
            content: vec![MessageContent::Text(truncate_str(
                &text,
                MESSAGE_SAMPLE_CHARS,
            ))],
            cache: false,
        });
    }
    req_messages.push(LanguageModelRequestMessage {
        role: Role::User,
        content: vec![MessageContent::Text(
            crate::prompt::render_static(
                crate::prompt::PromptTemplate::TitleFirstInstruction,
                lang,
            )
            .expect("title first instruction render"),
        )],
        cache: false,
    });

    LanguageModelRequest {
        messages: req_messages,
        tools: Vec::new(),
        tool_choice: None,
        temperature: Some(0.3),
        thinking_allowed: false,
        reasoning_effort: None,
    }
}

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

/// Sentinel the model emits when the latest message does NOT signal a new
/// topic. Compared case-insensitively after stripping trailing punctuation.
pub const UNCHANGED_SENTINEL: &str = "UNCHANGED";

/// Whether an already-sanitized title string is the "no change" sentinel.
/// Accepts trailing punctuation (`UNCHANGED.` / `UNCHANGED。`) for robustness.
pub fn is_unchanged(sanitized: &str) -> bool {
    let trimmed = sanitized.trim_end_matches([
        '.', '。', '!', '！', '?', '？', ',', '，', ';', '；', ':', '：',
    ]);
    trimmed.eq_ignore_ascii_case(UNCHANGED_SENTINEL)
}

/// Build a topic-shift check request: the latest user message plus the latest
/// assistant reply (each truncated), then an instruction naming the current
/// title and asking for either a new title (if the latest user message signals
/// a new topic) or the literal word `UNCHANGED`.
pub fn build_topic_shift_request(
    current_title: &str,
    messages: &[Message],
    lang: crate::language::Language,
) -> LanguageModelRequest {
    let mut req_messages: Vec<LanguageModelRequestMessage> = Vec::new();

    if let Some(text) = last_user_text(messages) {
        req_messages.push(LanguageModelRequestMessage {
            role: Role::User,
            content: vec![MessageContent::Text(truncate_str(
                &text,
                MESSAGE_SAMPLE_CHARS,
            ))],
            cache: false,
        });
    }
    if let Some(text) = last_assistant_text(messages) {
        req_messages.push(LanguageModelRequestMessage {
            role: Role::Assistant,
            content: vec![MessageContent::Text(truncate_str(
                &text,
                MESSAGE_SAMPLE_CHARS,
            ))],
            cache: false,
        });
    }
    let instruction = crate::prompt::render(
        crate::prompt::PromptTemplate::TitleTopicShiftInstruction,
        lang,
        &crate::prompt::TopicShiftData {
            current_title: current_title.to_string(),
            unchanged_sentinel: UNCHANGED_SENTINEL,
        },
    )
    .expect("topic shift instruction render");
    req_messages.push(LanguageModelRequestMessage {
        role: Role::User,
        content: vec![MessageContent::Text(instruction)],
        cache: false,
    });

    LanguageModelRequest {
        messages: req_messages,
        tools: Vec::new(),
        tool_choice: None,
        temperature: Some(0.3),
        thinking_allowed: false,
        reasoning_effort: None,
    }
}

/// Stream a title from `model`, taking the first line and sanitizing it.
/// Returns an empty string when the model produced no usable text. The caller
/// checks `is_unchanged` on the result before adopting it.
pub async fn stream_thread_title(
    model: &AnyLanguageModel,
    request: LanguageModelRequest,
    cx: &AsyncApp,
) -> Result<String> {
    let mut events = model.stream_completion(request, cx).await?;
    let mut raw = String::new();
    while let Some(ev) = events.next().await {
        let Ok(LanguageModelCompletionEvent::Text(text)) = ev else {
            continue;
        };
        if let Some(nl) = text.find(['\n', '\r']) {
            raw.push_str(&text[..nl]);
            break;
        }
        raw.push_str(&text);
        if raw.chars().count() > MAX_RAW_CHARS {
            break;
        }
    }
    Ok(sanitize_title(&raw))
}

/// Trim, strip wrapping quotes and a leading `Title:`/`标题：` prefix, collapse
/// internal whitespace to one line, and cap at the summary length.
pub fn sanitize_title(raw: &str) -> String {
    let mut s = raw.trim().to_string();
    strip_wrapping_quotes(&mut s);
    strip_title_prefix(&mut s);
    let collapsed: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_summary(&collapsed, 60)
}

fn first_user_text(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .filter(|m| m.role == Role::User)
        .find_map(message_text)
}

fn last_user_text(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::User)
        .find_map(message_text)
}

fn last_assistant_text(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .filter(|m| m.role == Role::Assistant)
        .find_map(message_text)
}

/// Concatenate all `Text` blocks of a message into one trimmed string.
fn message_text(m: &Message) -> Option<String> {
    let mut buf = String::new();
    for c in &m.content {
        if let MessageContent::Text(t) = c {
            buf.push_str(t);
        }
    }
    let trimmed = buf.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let t: String = s.chars().take(max_chars).collect();
    format!("{t}…")
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
    fn build_request_shape() {
        let messages = vec![
            Message::user("帮我看下登录".into()),
            Message::assistant(vec![MessageContent::Text("好的，我先读代码".into())]),
        ];
        let req = build_title_request(&messages, crate::language::Language::En);
        assert!(req.tools.is_empty());
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[0].role, Role::User);
        assert_eq!(req.messages[1].role, Role::Assistant);
        assert_eq!(req.messages[2].role, Role::User);
        assert!(req.messages[2].string_contents().contains("title"));
        assert_eq!(req.temperature, Some(0.3));
        assert!(!req.thinking_allowed);
    }

    #[test]
    fn build_request_without_assistant_reply() {
        let messages = vec![Message::user("only user".into())];
        let req = build_title_request(&messages, crate::language::Language::En);
        // user + instruction, no assistant turn.
        assert_eq!(req.messages.len(), 2);
    }

    #[test]
    fn last_user_text_skips_tool_result_user_messages() {
        // A tool result is role User with no Text block. `last_user_text` must
        // skip it and surface the last real user-typed message instead, so a
        // topic-shift check fired right after a tool round still has the user
        // turn to evaluate.
        use crate::language_model::LanguageModelToolResult;
        use std::sync::Arc;
        let messages = vec![
            Message::user("旧话题".into()),
            Message::assistant(vec![MessageContent::Text("旧回复".into())]),
            Message::user_with_content(vec![MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: "tu_1".into(),
                tool_name: Arc::<str>::from("Read"),
                is_error: false,
                content: "file contents".into(),
            })]),
            Message::user("现在帮我改下登录".into()),
            Message::assistant(vec![MessageContent::Text("好的".into())]),
            // Trailing tool result with no later user text — must not starve
            // the topic-shift request of a user turn.
            Message::user_with_content(vec![MessageContent::ToolResult(LanguageModelToolResult {
                tool_use_id: "tu_2".into(),
                tool_name: Arc::<str>::from("Bash"),
                is_error: false,
                content: "ok".into(),
            })]),
        ];
        let req = build_topic_shift_request("旧话题", &messages, crate::language::Language::En);
        // user (latest real text) + assistant + instruction.
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[0].role, Role::User);
        assert_eq!(req.messages[0].string_contents(), "现在帮我改下登录");
        assert_eq!(req.messages[1].role, Role::Assistant);
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
        for n in [9, 10, 11, 12, 14, 15] {
            assert!(!should_retitle(n), "turn {n} should skip");
        }
    }

    #[test]
    fn is_unchanged_accepts_variants() {
        assert!(is_unchanged("UNCHANGED"));
        assert!(is_unchanged("unchanged"));
        assert!(is_unchanged("UNCHANGED."));
        assert!(is_unchanged("UNCHANGED。"));
        assert!(!is_unchanged("Fix login bug"));
        assert!(!is_unchanged(""));
    }

    #[test]
    fn build_topic_shift_request_shape() {
        let messages = vec![
            Message::user("旧话题".into()),
            Message::assistant(vec![MessageContent::Text("旧回复".into())]),
            Message::user("现在帮我改下登录".into()),
            Message::assistant(vec![MessageContent::Text("好的".into())]),
        ];
        let req = build_topic_shift_request("旧话题标题", &messages, crate::language::Language::En);
        assert!(req.tools.is_empty());
        assert_eq!(req.messages.len(), 3);
        // Latest user + latest assistant, then the instruction.
        assert_eq!(req.messages[0].role, Role::User);
        assert_eq!(req.messages[0].string_contents(), "现在帮我改下登录");
        assert_eq!(req.messages[1].role, Role::Assistant);
        assert_eq!(req.messages[1].string_contents(), "好的");
        let instr = req.messages[2].string_contents();
        assert!(instr.contains("旧话题标题"));
        assert!(instr.contains(UNCHANGED_SENTINEL));
        assert_eq!(req.temperature, Some(0.3));
        assert!(!req.thinking_allowed);
    }
}
