//! Shared output bounding for tool results.
//!
//! Every tool result passes through [`truncate_result`] in
//! `Thread::run_tool_inner` before it is appended to the conversation, so no
//! tool — built-in or MCP — can flood the model context: a single match on a
//! megabyte-long minified line, or a multi-megabyte command dump, stays
//! bounded. The `Agent` tool is exempt (see [`should_cap_tool_result`]): its
//! JSON envelope is bounded at construction and byte-truncation would corrupt
//! the envelope the UI parses.
//!
//! Two independent caps, mirroring the pi/oh-my-pi truncation layer:
//! - a per-line cap, so one long line cannot eat the whole budget;
//! - a total cap keeping the head and tail with an elision marker, so both
//!   the leading context and the trailing (usually most relevant) output
//!   survive.

use std::borrow::Cow;

/// Total byte budget for a single tool result.
pub const MAX_OUTPUT_BYTES: usize = 50 * 1024;

/// Byte budget for a single line within a tool result.
pub const MAX_LINE_BYTES: usize = 500;

/// Fraction of [`MAX_OUTPUT_BYTES`] kept from the head / tail when the total
/// cap fires. Head+tail stay strictly below the budget so the marker and the
/// line caps always fit.
const HEAD_BUDGET: usize = MAX_OUTPUT_BYTES * 60 / 100;
const TAIL_BUDGET: usize = MAX_OUTPUT_BYTES * 25 / 100;

/// Cap one line at [`MAX_LINE_BYTES`], cutting on a char boundary. The
/// returned text stays valid UTF-8 regardless of where the budget lands.
pub fn truncate_line(line: &str) -> Cow<'_, str> {
    if line.len() <= MAX_LINE_BYTES {
        return Cow::Borrowed(line);
    }
    let mut end = MAX_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!(
        "{}… [+{} bytes truncated]",
        &line[..end],
        line.len() - end
    ))
}

/// Bound a tool result: per-line cap first, then the total cap. Returns the
/// input borrowed when nothing had to be cut, so the common small-result
/// path costs one scan and no allocation.
pub fn truncate_result(text: &str) -> Cow<'_, str> {
    // `split('\n')` rather than `lines()`: round-trips the input byte-exactly
    // (no `\r` stripping, trailing newline preserved).
    let needs_line_cap = text.split('\n').any(|l| l.len() > MAX_LINE_BYTES);
    if text.len() <= MAX_OUTPUT_BYTES && !needs_line_cap {
        return Cow::Borrowed(text);
    }
    let capped = if needs_line_cap {
        text.split('\n')
            .map(truncate_line)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text.to_string()
    };
    if capped.len() <= MAX_OUTPUT_BYTES {
        return Cow::Owned(capped);
    }

    let mut head = String::with_capacity(HEAD_BUDGET);
    for line in capped.split('\n') {
        if head.len() + line.len() + 1 > HEAD_BUDGET {
            break;
        }
        head.push_str(line);
        head.push('\n');
    }
    let mut tail_lines: Vec<&str> = Vec::new();
    let mut tail_len = 0usize;
    for line in capped.split('\n').rev() {
        if tail_len + line.len() + 1 > TAIL_BUDGET {
            break;
        }
        tail_lines.push(line);
        tail_len += line.len() + 1;
    }
    tail_lines.reverse();
    let tail = tail_lines.join("\n");

    let elided = capped.len().saturating_sub(head.len() + tail.len());
    Cow::Owned(format!(
        "{head}⚠ Output too long ({} bytes total, showing first {} and last {}; {elided} bytes elided). \
         Do not speculate about the elided content.\n{tail}",
        capped.len(),
        head.len(),
        tail.len(),
    ))
}

/// Whether a tool result passes through the output cap in
/// `Thread::run_tool_inner`. The `Agent` tool's JSON envelope is exempt: it
/// is bounded at construction (see `tools::agent::ENVELOPE_MESSAGES_BUDGET`)
/// and the UI parses it as JSON, which byte-truncation would corrupt. The
/// comparison matches `SpawnAgentTool::name()` — PascalCase `"Agent"`.
pub fn should_cap_tool_result(tool_name: &str) -> bool {
    tool_name != "Agent"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_envelope_is_exempt_from_the_cap() {
        // Regression for the PascalCase rename: the tool's `name()` returns
        // "Agent", so an exemption keyed on lowercase "agent" never fired and
        // envelopes were byte-truncated into unparseable JSON.
        assert!(!should_cap_tool_result("Agent"));
        assert!(should_cap_tool_result("Grep"));
        assert!(should_cap_tool_result("Bash"));
        assert!(should_cap_tool_result("mcp_server_tool"));
    }

    #[test]
    fn short_text_passes_through_borrowed() {
        let text = "hello\nworld\n";
        assert!(matches!(truncate_result(text), Cow::Borrowed(_)));
        assert_eq!(truncate_result(text), text);
    }

    #[test]
    fn empty_text_passes_through() {
        assert_eq!(truncate_result(""), "");
    }

    #[test]
    fn long_line_is_capped_with_marker() {
        let line = "x".repeat(MAX_LINE_BYTES + 100);
        let out = truncate_line(&line);
        assert!(out.starts_with(&"x".repeat(MAX_LINE_BYTES)));
        assert!(out.contains(&format!("[+{} bytes truncated]", 100)));
    }

    #[test]
    fn line_cap_respects_char_boundary() {
        // '界' is 3 bytes; a cut mid-codepoint must back off to the boundary.
        let mut line = "界".repeat(200); // 600 bytes
        line.push_str(&"a".repeat(100));
        let out = truncate_line(&line);
        let text_part = out.split('…').next().expect("marker separator");
        assert!(text_part.len() <= MAX_LINE_BYTES);
        assert!(text_part.chars().all(|c| c == '界'));
    }

    #[test]
    fn single_mega_line_only_gets_line_capped() {
        // The incident shape: a 2.4 MB single-line JSON dump must collapse to
        // one capped line, not eat the total budget.
        let blob = format!("{{\"data\":\"{}\"}}", "y".repeat(2 * 1024 * 1024));
        let out = truncate_result(&blob);
        assert!(out.len() < MAX_LINE_BYTES + 100);
        assert!(out.contains("bytes truncated"));
    }

    #[test]
    fn total_cap_keeps_head_and_tail() {
        let lines: Vec<String> = (0..3000)
            .map(|i| format!("line-{i:04}-{}", "z".repeat(40)))
            .collect();
        let text = lines.join("\n");
        assert!(text.len() > MAX_OUTPUT_BYTES);
        let out = truncate_result(&text);
        assert!(out.contains("line-0000-"), "head lines survive");
        assert!(out.contains("line-2999-"), "tail lines survive");
        assert!(!out.contains("line-1500-"), "middle is elided");
        assert!(out.contains("bytes elided"));
        assert!(out.len() <= MAX_OUTPUT_BYTES + 512);
    }

    #[test]
    fn per_line_cap_then_total_cap_compose() {
        // Long lines each get capped first; if the result still exceeds the
        // total budget, head/tail applies on the capped text.
        let lines: Vec<String> = (0..500)
            .map(|i| format!("{i}:{}", "w".repeat(2000)))
            .collect();
        let text = lines.join("\n");
        let out = truncate_result(&text);
        assert!(out.contains("bytes truncated"), "per-line markers present");
        assert!(out.len() <= MAX_OUTPUT_BYTES + 512);
    }

    #[test]
    fn carriage_returns_and_trailing_newline_survive() {
        let text = "a\r\nb\r\n";
        assert_eq!(truncate_result(text), text);
    }

    #[test]
    fn truncated_output_does_not_retrigger() {
        let lines: Vec<String> = (0..3000)
            .map(|i| format!("line-{i:04}-{}", "z".repeat(40)))
            .collect();
        let once = truncate_result(&lines.join("\n")).into_owned();
        let twice = truncate_result(&once);
        assert_eq!(once, twice, "truncation is idempotent");
    }
}
