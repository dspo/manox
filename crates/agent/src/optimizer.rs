//! Context-projection optimizer: per-turn model-facing tool-result compaction.
//!
//! Every tool result in the canonical [`Thread::messages`] history is rewritten
//! before it reaches the model. The original messages are never modified — this
//! layer only affects the projection built by [`build_completion_request`].
//!
//! Each tool type has a per-tool output budget. Results exceeding the budget are
//! truncated with head+tail preservation and a summary of what was elided. The
//! goal is to keep relevant information while removing verbose logs, repeated
//! metadata, and UI-only envelope content.
//!
//! Hashline numbering (`[path#TAG]` header and `N:` line-number prefix from
//! `read_file`) is **not** stripped — it is the protocol the model uses to
//! produce Edit tool patches, and removing it would break Edit.

use crate::language_model::{LanguageModelRequestMessage, MessageContent};
use crate::message::Message;

/// Per-tool output budgets in bytes. Roughly ordered by expected verbosity:
/// read_file returns source code (verbose but useful), bash returns logs (often
/// noisy), grep/glob/list return structured listings (compact).
const BUDGET_READ: usize = 24 * 1024; // 24 KiB
const BUDGET_BASH: usize = 16 * 1024; // 16 KiB
const BUDGET_BASH_OUTPUT: usize = 16 * 1024;
const BUDGET_MONITOR: usize = 16 * 1024;
const BUDGET_WEB: usize = 16 * 1024;
const BUDGET_GREP: usize = 8 * 1024; // 8 KiB
const BUDGET_GLOB: usize = 8 * 1024;
const BUDGET_LIST: usize = 8 * 1024;
const BUDGET_DEFAULT: usize = 16 * 1024;
/// Head fraction of the budget kept from the beginning.
const HEAD_FRAC: f64 = 0.6;
/// Tail fraction of the budget kept from the end.
const TAIL_FRAC: f64 = 0.25;

/// Rewrite every tool result in `messages` through the per-tool compact
/// renderer. Returns a new `Vec<Message>` — the canonical history is untouched.
pub fn optimize(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .map(|msg| {
            let content: Vec<MessageContent> = msg
                .content
                .iter()
                .map(|c| match c {
                    MessageContent::ToolResult(tr) => {
                        let budget = tool_budget(&tr.tool_name);
                        let compact = compact_tool_output(&tr.tool_name, &tr.content, budget);
                        MessageContent::ToolResult(crate::language_model::LanguageModelToolResult {
                            tool_use_id: tr.tool_use_id.clone(),
                            tool_name: tr.tool_name.clone(),
                            is_error: tr.is_error,
                            content: compact,
                        })
                    }
                    other => other.clone(),
                })
                .collect();
            Message {
                id: msg.id.clone(),
                timestamp: msg.timestamp,
                parent_id: msg.parent_id.clone(),
                role: msg.role,
                content,
                ui: msg.ui.clone(),
            }
        })
        .collect()
}

/// Project inline images for the selected model. Vision-capable models retain
/// images. Text-only models receive a compact placeholder. During compaction,
/// `max_bytes` may remove the oldest image-only messages first while retaining
/// the newest visual turn.
pub fn apply_image_policy(
    mut messages: Vec<LanguageModelRequestMessage>,
    supports_images: bool,
    max_bytes: Option<usize>,
) -> Vec<LanguageModelRequestMessage> {
    if !supports_images {
        for message in &mut messages {
            message.content = message
                .content
                .iter()
                .map(|content| match content {
                    MessageContent::Image { data, mime_type } => MessageContent::Text(format!(
                        "[image omitted: active model has no vision support; mime={mime_type}, encoded_bytes={}]",
                        data.len()
                    )),
                    other => other.clone(),
                })
                .collect();
        }
        return messages;
    }

    let Some(max_bytes) = max_bytes else {
        return messages;
    };
    let mut total = request_content_bytes(&messages);
    if total <= max_bytes {
        return messages;
    }
    let image_only: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            !message.content.is_empty()
                && message
                    .content
                    .iter()
                    .all(|content| matches!(content, MessageContent::Image { .. }))
        })
        .map(|(idx, _)| idx)
        .collect();
    let newest = image_only.last().copied();
    let mut remove = std::collections::HashSet::new();
    for idx in image_only {
        if Some(idx) == newest || total <= max_bytes {
            continue;
        }
        total = total.saturating_sub(message_content_bytes(&messages[idx]));
        remove.insert(idx);
    }
    messages
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| !remove.contains(idx))
        .map(|(_, message)| message)
        .collect()
}

fn request_content_bytes(messages: &[LanguageModelRequestMessage]) -> usize {
    messages.iter().map(message_content_bytes).sum()
}

fn message_content_bytes(message: &LanguageModelRequestMessage) -> usize {
    message
        .content
        .iter()
        .map(|content| match content {
            MessageContent::Text(text)
            | MessageContent::Compaction(text)
            | MessageContent::Thinking { text, .. } => text.len(),
            MessageContent::Image { data, mime_type } => data.len() + mime_type.len(),
            MessageContent::ToolUse(tool_use) => {
                tool_use.raw_input.len()
                    + serde_json::to_string(&tool_use.input)
                        .map(|text| text.len())
                        .unwrap_or_default()
            }
            MessageContent::ToolResult(result) => result.content.len(),
        })
        .sum()
}

/// The per-tool output budget in bytes.
pub(crate) fn tool_budget(tool_name: &str) -> usize {
    match tool_name {
        "Read" => BUDGET_READ,
        "Bash" => BUDGET_BASH,
        "BashOutput" => BUDGET_BASH_OUTPUT,
        "Monitor" => BUDGET_MONITOR,
        "WebFetch" | "WebSearch" => BUDGET_WEB,
        "Grep" => BUDGET_GREP,
        "Glob" => BUDGET_GLOB,
        "List" => BUDGET_LIST,
        _ => BUDGET_DEFAULT,
    }
}

/// Compact a single tool output: apply the per-tool budget with head+tail
/// preservation. Hashline numbering is preserved — the model needs the
/// `[path#TAG]` header and `N:` line-number prefix to generate Edit patches.
pub(crate) fn compact_tool_output(_tool_name: &str, raw: &str, budget: usize) -> String {
    truncate_with_budget(raw, budget)
}

/// Truncate `text` to `budget` bytes, preserving the head (HEAD_FRAC of
/// budget) and tail (TAIL_FRAC of budget). The middle is replaced by a
/// one-line elision marker. Truncation operates at byte positions on valid
/// UTF-8 character boundaries so multi-byte sequences are never split.
/// The truncation marker itself counts toward the budget.
fn truncate_with_budget(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    if budget < 128 {
        return truncate_str_to_bytes(text, budget).to_string();
    }
    let mut head_bytes = (budget as f64 * HEAD_FRAC) as usize;
    let mut tail_bytes = (budget as f64 * TAIL_FRAC) as usize;
    loop {
        let head = truncate_str_to_bytes(text, head_bytes);
        let tail_start = snap_to_char_boundary(text, text.len().saturating_sub(tail_bytes));
        let tail = &text[tail_start..];
        let skipped = text.len().saturating_sub(head.len() + tail.len());
        let rendered = format!(
            "{head}\n⚠ Output truncated ({total} bytes, keeping head {head_b}B + tail {tail_b}B; {skipped} bytes elided)\n{tail}",
            total = text.len(),
            head_b = head.len(),
            tail_b = tail.len(),
        );
        if rendered.len() <= budget {
            return rendered;
        }
        let overflow = rendered.len() - budget;
        if tail_bytes > overflow + 4 {
            tail_bytes -= overflow + 4;
        } else if head_bytes > overflow + 4 {
            head_bytes -= overflow + 4;
        } else {
            return truncate_str_to_bytes(text, budget).to_string();
        }
    }
}

/// Return the longest prefix of `s` whose byte length is ≤ `max_bytes` and
/// ends at a valid UTF-8 character boundary.
fn truncate_str_to_bytes(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let end = snap_to_char_boundary(s, max_bytes);
    &s[..end]
}

/// Find the nearest valid UTF-8 character boundary at or before `pos`.
fn snap_to_char_boundary(s: &str, mut pos: usize) -> usize {
    if pos >= s.len() {
        return s.len();
    }
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_output_passes_through() {
        let out = compact_tool_output("Read", "hello world", 1024);
        assert_eq!(out, "hello world");
    }

    #[test]
    fn hashline_is_preserved_for_edit_tool() {
        // The model needs [path#TAG] and N: line numbers to generate Edit patches.
        let raw = "[src/main.rs#L1]\n1:fn main() {\n2:    println!(\"hi\");\n3:}\n";
        let out = compact_tool_output("Read", raw, 1024);
        assert!(out.contains("[src/main.rs#L1]"), "header preserved: {out}");
        assert!(out.contains("1:fn"), "line numbers preserved: {out}");
    }

    #[test]
    fn multi_digit_line_numbers_preserved() {
        let raw = "[src/lib.rs#L100]\n99:fn foo() {\n100:    bar();\n101:}\n";
        let out = compact_tool_output("Read", raw, 1024);
        assert!(out.contains("99:fn"), "multi-digit line preserved: {out}");
        assert!(
            out.contains("100:    bar()"),
            "3-digit line preserved: {out}"
        );
    }

    #[test]
    fn bash_output_passes_through() {
        let raw = "1:error: something failed\n2:  at line 42\n";
        let out = compact_tool_output("Bash", raw, 1024);
        assert_eq!(out, raw); // No hashline stripping on non-Read tools
    }

    #[test]
    fn truncation_preserves_head_and_tail() {
        let big = "A".repeat(2000);
        let out = truncate_with_budget(&big, 1000);
        assert!(out.starts_with('A'));
        assert!(out.contains("truncated"));
        assert!(out.len() <= 1000); // marker is included in the budget
    }

    #[test]
    fn multi_byte_chars_preserved_at_boundaries() {
        // Emoji are 4 bytes each. Truncation must not split them.
        let text = "🚀".repeat(500) + "hello" + &"🌟".repeat(500);
        let out = truncate_with_budget(&text, 600);
        assert!(out.starts_with('🚀'), "starts with rocket");
        assert!(out.ends_with('🌟'), "ends with star");
        assert!(std::str::from_utf8(out.as_bytes()).is_ok(), "valid UTF-8");
    }

    #[test]
    fn budget_is_bytes_not_chars() {
        // "中" is 3 bytes in UTF-8. 24 KiB budget should allow at most 8192
        // of them (= 24576 bytes), NOT 24576 chars (= 73728 bytes).
        let text = "中".repeat(25000); // 75000 bytes
        let out = truncate_with_budget(&text, 24 * 1024); // 24576 bytes
        assert!(
            out.len() <= 24 * 1024,
            "24 KiB budget: output {} bytes (budget 24576)",
            out.len()
        );
    }

    #[test]
    fn budgets_are_reasonable() {
        let _ = BUDGET_READ;
        let _ = BUDGET_GREP;
        let _ = BUDGET_DEFAULT;
    }

    fn image_message(id: &str, bytes: usize) -> LanguageModelRequestMessage {
        LanguageModelRequestMessage {
            role: crate::language_model::Role::User,
            content: vec![MessageContent::Image {
                data: id.repeat(bytes),
                mime_type: "image/png".into(),
            }],
            cache: false,
        }
    }

    #[test]
    fn nonvision_projection_replaces_images_without_mutating_input() {
        let input = vec![image_message("A", 1024)];
        let projected = apply_image_policy(input.clone(), false, None);
        assert!(matches!(input[0].content[0], MessageContent::Image { .. }));
        assert!(
            matches!(projected[0].content[0], MessageContent::Text(ref text) if text.contains("image omitted"))
        );
    }

    #[test]
    fn vision_projection_keeps_images_without_compaction_budget() {
        let input = vec![image_message("A", 1024)];
        assert!(matches!(
            apply_image_policy(input, true, None)[0].content[0],
            MessageContent::Image { .. }
        ));
    }

    #[test]
    fn compaction_drops_oldest_image_only_messages_but_keeps_newest() {
        let projected = apply_image_policy(
            vec![image_message("A", 1000), image_message("B", 1000)],
            true,
            Some(1200),
        );
        assert_eq!(projected.len(), 1);
        assert!(matches!(
            &projected[0].content[0],
            MessageContent::Image { data, .. } if data.starts_with('B')
        ));
    }

    const _: () = {
        assert!(BUDGET_READ >= 16 * 1024);
        assert!(BUDGET_GREP >= 4 * 1024);
        assert!(BUDGET_DEFAULT >= 8 * 1024);
    };
}
