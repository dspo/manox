//! Context-projection optimizer: per-turn model-facing tool-result compaction.
//!
//! Every tool result in the canonical [`Thread::messages`] history is rewritten
//! before it reaches the model. The original messages are never modified — this
//! layer only affects the projection built by [`build_completion_request`].
//!
//! Each tool type has a compact renderer with a per-tool output budget. Results
//! exceeding the budget are truncated with head+tail preservation and a summary
//! of what was elided. The goal is to keep relevant information while removing
//! verbose logs, repeated metadata, and UI-only envelope content.

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

/// Compact a single tool output: strip hashline numbering (the `N:`
/// prefix on each line from `read_file`), then apply the per-tool budget
/// with head+tail preservation.
pub(crate) fn compact_tool_output(tool_name: &str, raw: &str, budget: usize) -> String {
    let cleaned = strip_hashline_numbering(tool_name, raw);
    truncate_with_budget(&cleaned, budget)
}

/// Strip the `[path#TAG]\n` header and the `N:` line-number prefix added by
/// `read_file`. Preserve the tag marker for reference.
fn strip_hashline_numbering(tool_name: &str, raw: &str) -> String {
    if tool_name != "Read" {
        return raw.to_string();
    }
    // Remove the hashline header line (first line starting with `[` and ending
    // with `#<tag>]`), but keep everything else.
    let body = match raw.find('\n') {
        Some(nl) => &raw[nl + 1..],
        None => raw,
    };
    // Strip `N:` prefix from each line.
    body.lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix(|c: char| c.is_ascii_digit())
                && rest.starts_with(':')
            {
                &rest[1..]
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Truncate `text` to `budget` bytes, preserving the head (HEAD_FRAC of
/// budget) and tail (TAIL_FRAC of budget). The middle is replaced by a
/// one-line elision marker.
fn truncate_with_budget(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let head_bytes = (budget as f64 * HEAD_FRAC) as usize;
    let tail_bytes = (budget as f64 * TAIL_FRAC) as usize;
    let elided = text.len() - head_bytes - tail_bytes;
    if elided == 0 {
        // Budget too small — just take the head.
        let mut out = text.chars().take(budget).collect::<String>();
        out.push('…');
        return out;
    }
    let head: String = text.chars().take(head_bytes).collect();
    let tail: String = text
        .chars()
        .rev()
        .take(tail_bytes)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!(
        "{head}\n⚠ Output truncated ({total} bytes, showing first {head_b} and last {tail_b}; {elided_b} bytes elided)\n{tail}",
        total = text.len(),
        head_b = head_bytes,
        tail_b = tail_bytes,
        elided_b = elided,
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
    fn hashline_stripping_removes_line_numbers() {
        let raw = "[src/main.rs#L1]\n1:fn main() {\n2:    println!(\"hi\");\n3:}\n";
        let out = strip_hashline_numbering("Read", raw);
        assert!(!out.contains("[src/main.rs#L1]"), "header removed");
        assert!(!out.contains("1:fn"), "line numbers stripped: {out}");
        assert!(out.contains("fn main()"), "code preserved: {out}");
    }

    #[test]
    fn bash_output_not_hashline_stripped() {
        let raw = "1:error: something failed\n2:  at line 42\n";
        let out = strip_hashline_numbering("Bash", raw);
        assert_eq!(out, raw); // Bash output preserved verbatim before budget truncation
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
    fn budgets_are_reasonable() {
        // Runtime guard: the constants are compile-time but the test documents the
        // invariant for human readers.
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
