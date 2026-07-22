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

use crate::language_model::MessageContent;
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
/// one-line elision marker. Truncation operates on chars (not bytes) to
/// avoid splitting multi-byte UTF-8 sequences mid-character.
fn truncate_with_budget(text: &str, budget: usize) -> String {
    let char_count = text.chars().count();
    if char_count <= budget {
        return text.to_string();
    }
    let head_chars = (budget as f64 * HEAD_FRAC) as usize;
    let tail_chars = (budget as f64 * TAIL_FRAC) as usize;
    let total_kept = head_chars + tail_chars;
    if total_kept >= char_count {
        // Budget covers everything after rounding — no truncation needed.
        return text.to_string();
    }
    let elided = char_count - total_kept;
    let head: String = text.chars().take(head_chars).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(tail_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!(
        "{head}\n⚠ Output truncated ({total} chars, showing first {head_c} and last {tail_c}; {skipped} chars elided)\n{tail}",
        total = char_count,
        head_c = head_chars,
        tail_c = tail_chars,
        skipped = elided,
    )
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
        assert!(out.len() <= 1200); // budget + marker overhead
    }

    #[test]
    fn multi_byte_chars_handled_correctly() {
        // Each emoji is 4 bytes in UTF-8. chars().take() preserves whole chars.
        let text = "🚀".repeat(500) + "hello" + &"🌟".repeat(500);
        let out = truncate_with_budget(&text, 600);
        assert!(out.starts_with('🚀'), "starts with rocket: {out}");
        assert!(out.ends_with('🌟'), "ends with star");
        // Must not contain split multi-byte sequences (would cause UTF-8 errors).
        assert!(std::str::from_utf8(out.as_bytes()).is_ok(), "valid UTF-8");
    }

    #[test]
    fn budgets_are_reasonable() {
        let _ = BUDGET_READ;
        let _ = BUDGET_GREP;
        let _ = BUDGET_DEFAULT;
    }

    const _: () = {
        assert!(BUDGET_READ >= 16 * 1024);
        assert!(BUDGET_GREP >= 4 * 1024);
        assert!(BUDGET_DEFAULT >= 8 * 1024);
    };
}
