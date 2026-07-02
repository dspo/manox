//! Markdown syntax auto-conversion glue.
//!
//! `RichTextState` (manox first-party `gpui-rich-text` crate) has no built-in
//! markdown recognition — typing `# ` inserts a literal `#`. This module runs
//! after every state change and, when the cursor sits right behind a
//! just-completed markdown prefix or inline delimiter, strips the literal
//! markers and flips the block kind / inline mark via `set_block_kind` /
//! `toggle_list` / `toggle_*_mark`.
//!
//! Conversion is anchored to the cursor's own row (via `cursor_line`), so it
//! works in multi-line documents — not just the first line.
//!
//! Known limitation: `cx.notify()` re-fires this observer, so undoing past a
//! conversion restores the literal `# ` text which then re-triggers conversion,
//! eating the undo. There is no public way to distinguish user input from undo.

use std::ops::Range;

use gpui::{Context, Window};
use gpui_rich_text::BlockKind;
use gpui_rich_text::RichTextState;

/// Inspect the line at the cursor; if it ends in a complete markdown prefix or
/// inline delimiter pair, strip the markers and apply the matching block/mark.
pub fn try_apply_markdown_shortcut(
    state: &mut RichTextState,
    window: &mut Window,
    cx: &mut Context<RichTextState>,
) {
    let (row, col, line) = state.cursor_line();
    let row_start = state.line_start_offset(row);

    if let Some((prefix_len, action)) = detect_block_prefix(&line)
        && col == prefix_len
    {
        state.replace_range(row_start..row_start + prefix_len, "", window, cx);
        apply_block_action(state, action, window, cx);
        return;
    }

    if let Some((inner, url)) = detect_link(&line, col) {
        // `[` sits one byte before the inner text; `](url)` spans from inner.end
        // to the cursor (col).
        let open_start = inner.start - 1;
        let close_start = inner.end;
        let close_len = col - inner.end;
        let inner_rope = (row_start + inner.start)..(row_start + inner.end);
        state.set_selection(inner_rope.start, inner_rope.end);
        state.toggle_link_mark(url, window, cx);
        state.replace_range(
            (row_start + close_start)..(row_start + close_start + close_len),
            "",
            window,
            cx,
        );
        state.replace_range(
            (row_start + open_start)..(row_start + open_start + 1),
            "",
            window,
            cx,
        );
        state.set_cursor(row_start + open_start + (inner.end - inner.start));
        return;
    }

    if let Some((mark, delim_len, inner)) = detect_inline(&line, col) {
        let open_start = inner.start - delim_len;
        let close_start = inner.end;
        let inner_rope = (row_start + inner.start)..(row_start + inner.end);
        state.set_selection(inner_rope.start, inner_rope.end);
        apply_inline_mark(state, mark, window, cx);
        state.replace_range(
            (row_start + close_start)..(row_start + close_start + delim_len),
            "",
            window,
            cx,
        );
        state.replace_range(
            (row_start + open_start)..(row_start + open_start + delim_len),
            "",
            window,
            cx,
        );
        state.set_cursor(row_start + open_start + (inner.end - inner.start));
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ConvertAction {
    Heading(u8),
    UnorderedList,
    OrderedList,
    BlockQuote,
    HorizontalRule,
    CodeBlock,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum InlineMark {
    Bold,
    Italic,
    Strike,
    Code,
}

/// Match a line against a complete markdown block prefix (trailing space
/// required, no leading whitespace). Returns the prefix length and the block
/// action. The caller gates on `col == prefix_len` so conversion fires only
/// when the cursor lands right after the prefix.
fn detect_block_prefix(line: &str) -> Option<(usize, ConvertAction)> {
    if line.starts_with(' ') {
        return None;
    }

    // Code fence: three or more backticks optionally followed by a language
    // tag. The whole line is the marker, so the cursor must be at EOL.
    if let Some(action) = detect_code_fence(line) {
        return Some((line.len(), action));
    }

    // Horizontal rule: three or more of `-`, `*`, or `_` and nothing else.
    if let Some(action) = detect_horizontal_rule(line) {
        return Some((line.len(), action));
    }

    // Longest headings first so `## ` is not shadowed by `# `.
    const HEADINGS: &[(&str, u8)] = &[
        ("###### ", 6),
        ("##### ", 5),
        ("#### ", 4),
        ("### ", 3),
        ("## ", 2),
        ("# ", 1),
    ];
    for (prefix, level) in HEADINGS {
        if line.starts_with(prefix) {
            return Some((prefix.len(), ConvertAction::Heading(*level)));
        }
    }
    for prefix in ["- ", "* "] {
        if line.starts_with(prefix) {
            return Some((prefix.len(), ConvertAction::UnorderedList));
        }
    }
    if line.starts_with("1. ") {
        return Some((3, ConvertAction::OrderedList));
    }
    if line.starts_with("> ") {
        return Some((2, ConvertAction::BlockQuote));
    }
    None
}

/// A code fence line is three or more backticks followed by an optional
/// language tag (word chars, `-`, `+`, `#`, or whitespace only).
fn detect_code_fence(line: &str) -> Option<ConvertAction> {
    let backticks = line.bytes().take_while(|&b| b == b'`').count();
    if backticks < 3 {
        return None;
    }
    let rest = &line[backticks..];
    let valid = rest.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'-' | b'+' | b'_' | b'#' | b' ' | b'\t')
    });
    valid.then_some(ConvertAction::CodeBlock)
}

/// A horizontal rule line is three or more of the same character (`-`, `*`,
/// or `_`) with nothing else.
fn detect_horizontal_rule(line: &str) -> Option<ConvertAction> {
    if line.len() < 3 {
        return None;
    }
    let first = line.as_bytes()[0];
    if !matches!(first, b'-' | b'*' | b'_') {
        return None;
    }
    if line.bytes().all(|b| b == first) {
        Some(ConvertAction::HorizontalRule)
    } else {
        None
    }
}

/// If the cursor sits right after the closing `)` of a `[text](url)` link and
/// the `[` is not preceded by `!` (which would make it an image), return the
/// inner-text range (line-local byte offsets) and the URL.
fn detect_link(line: &str, col: usize) -> Option<(Range<usize>, String)> {
    let col = col.min(line.len());
    let prefix = &line[..col];
    if !prefix.ends_with(')') {
        return None;
    }

    let close_paren_pos = col - 1;
    let open_paren_pos = prefix[..close_paren_pos].rfind('(')?;
    let url = &prefix[open_paren_pos + 1..close_paren_pos];
    if url.is_empty() {
        return None;
    }

    let close_bracket_pos = open_paren_pos.checked_sub(1)?;
    if prefix.as_bytes()[close_bracket_pos] != b']' {
        return None;
    }

    let open_bracket_pos = prefix[..close_bracket_pos].rfind('[')?;
    let text_start = open_bracket_pos + 1;
    if text_start >= close_bracket_pos {
        return None;
    }

    // `[` preceded by `!` is an image, not a link — leave it as literal text.
    if open_bracket_pos > 0 && prefix.as_bytes()[open_bracket_pos - 1] == b'!' {
        return None;
    }

    Some((text_start..close_bracket_pos, url.to_string()))
}

/// If the cursor sits right after a closing inline delimiter, find the matching
/// opening delimiter and return the mark kind, delimiter length, and the
/// inner-text range (line-local byte offsets). Longer delimiters are tried
/// first so `**` is not mis-read as italic `*`.
fn detect_inline(line: &str, col: usize) -> Option<(InlineMark, usize, Range<usize>)> {
    let col = col.min(line.len());
    let prefix = &line[..col];

    for (mark, delim) in [
        (InlineMark::Bold, "**"),
        (InlineMark::Strike, "~~"),
        (InlineMark::Code, "`"),
    ] {
        if let Some(inner) = find_wrapped(prefix, delim) {
            return Some((mark, delim.len(), inner));
        }
    }

    // Italic uses a single `*`; reject `**` so bold is not mis-read as italic,
    // and require the inner text to contain no `*` to avoid crossing pairs.
    if prefix.ends_with('*')
        && !prefix.ends_with("**")
        && let Some(inner) = find_wrapped(prefix, "*")
        && !line[inner.clone()].contains('*')
    {
        return Some((InlineMark::Italic, 1, inner));
    }
    None
}

/// `prefix` is `line[..col]`. If it ends with `delim`, find the last `delim`
/// before the closing one and return the inner range (line-local byte offsets).
fn find_wrapped(prefix: &str, delim: &str) -> Option<Range<usize>> {
    if !prefix.ends_with(delim) {
        return None;
    }
    let inner_end = prefix.len() - delim.len();
    let open_pos = prefix[..inner_end].rfind(delim)?;
    let inner_start = open_pos + delim.len();
    if inner_start >= inner_end {
        return None;
    }
    Some(inner_start..inner_end)
}

fn apply_block_action(
    state: &mut RichTextState,
    action: ConvertAction,
    window: &mut Window,
    cx: &mut Context<RichTextState>,
) {
    match action {
        ConvertAction::Heading(level) => {
            state.set_block_kind(BlockKind::Heading { level }, window, cx);
        }
        ConvertAction::UnorderedList => {
            state.toggle_list(BlockKind::UnorderedListItem, window, cx);
        }
        ConvertAction::OrderedList => {
            state.toggle_list(BlockKind::OrderedListItem, window, cx);
        }
        ConvertAction::BlockQuote => {
            state.set_block_kind(BlockKind::BlockQuote, window, cx);
        }
        ConvertAction::HorizontalRule => {
            state.set_block_kind(BlockKind::HorizontalRule, window, cx);
        }
        ConvertAction::CodeBlock => {
            state.set_block_kind(BlockKind::CodeBlock, window, cx);
        }
    }
}

fn apply_inline_mark(
    state: &mut RichTextState,
    mark: InlineMark,
    window: &mut Window,
    cx: &mut Context<RichTextState>,
) {
    match mark {
        InlineMark::Bold => state.toggle_bold_mark(window, cx),
        InlineMark::Italic => state.toggle_italic_mark(window, cx),
        InlineMark::Strike => state.toggle_strikethrough_mark(window, cx),
        InlineMark::Code => state.toggle_code_mark(window, cx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_block_prefix_headings() {
        assert_eq!(
            detect_block_prefix("# "),
            Some((2, ConvertAction::Heading(1)))
        );
        assert_eq!(
            detect_block_prefix("## "),
            Some((3, ConvertAction::Heading(2)))
        );
        assert_eq!(
            detect_block_prefix("### "),
            Some((4, ConvertAction::Heading(3)))
        );
        assert_eq!(
            detect_block_prefix("#### "),
            Some((5, ConvertAction::Heading(4)))
        );
        assert_eq!(
            detect_block_prefix("###### "),
            Some((7, ConvertAction::Heading(6)))
        );
    }

    #[test]
    fn detect_block_prefix_lists_and_quote() {
        assert_eq!(
            detect_block_prefix("- "),
            Some((2, ConvertAction::UnorderedList))
        );
        assert_eq!(
            detect_block_prefix("* "),
            Some((2, ConvertAction::UnorderedList))
        );
        assert_eq!(
            detect_block_prefix("1. "),
            Some((3, ConvertAction::OrderedList))
        );
        assert_eq!(
            detect_block_prefix("> "),
            Some((2, ConvertAction::BlockQuote))
        );
    }

    #[test]
    fn detect_block_prefix_rejects_incomplete() {
        assert_eq!(detect_block_prefix("#"), None);
        assert_eq!(detect_block_prefix("##"), None);
        assert_eq!(detect_block_prefix("#hello"), None);
        assert_eq!(detect_block_prefix("##hello "), None);
        assert_eq!(detect_block_prefix(" # "), None);
        assert_eq!(detect_block_prefix("2. "), None);
        assert_eq!(detect_block_prefix("hello world"), None);
        assert_eq!(detect_block_prefix(""), None);
    }

    // Inline cases model the line state at the instant the closing delimiter
    // is typed: cursor right after the closing delim, no trailing text yet.

    #[test]
    fn detect_inline_bold() {
        // "a **b**" — cursor at col 7 (after closing **)
        let line = "a **b**";
        assert_eq!(
            detect_inline(line, line.len()),
            Some((InlineMark::Bold, 2, 4..5))
        );
    }

    #[test]
    fn detect_inline_italic() {
        let line = "a *b*";
        assert_eq!(
            detect_inline(line, line.len()),
            Some((InlineMark::Italic, 1, 3..4))
        );
    }

    #[test]
    fn detect_inline_code() {
        let line = "a `b`";
        assert_eq!(
            detect_inline(line, line.len()),
            Some((InlineMark::Code, 1, 3..4))
        );
    }

    #[test]
    fn detect_inline_strike() {
        let line = "a ~~b~~";
        assert_eq!(
            detect_inline(line, line.len()),
            Some((InlineMark::Strike, 2, 4..5))
        );
    }

    #[test]
    fn detect_inline_bold_not_italic() {
        // `**b**` must resolve to Bold, not Italic.
        let line = "**b**";
        assert_eq!(
            detect_inline(line, line.len()),
            Some((InlineMark::Bold, 2, 2..3))
        );
    }

    #[test]
    fn detect_inline_rejects_incomplete() {
        assert_eq!(detect_inline("a **b", 5), None); // no closing delim at cursor
        assert_eq!(detect_inline("a ****", 6), None); // empty inner
        assert_eq!(detect_inline("a **b**", 5), None); // cursor not after closing delim
    }

    #[test]
    fn detect_block_prefix_horizontal_rule() {
        assert_eq!(
            detect_block_prefix("---"),
            Some((3, ConvertAction::HorizontalRule))
        );
        assert_eq!(
            detect_block_prefix("----"),
            Some((4, ConvertAction::HorizontalRule))
        );
        assert_eq!(
            detect_block_prefix("***"),
            Some((3, ConvertAction::HorizontalRule))
        );
        assert_eq!(
            detect_block_prefix("___"),
            Some((3, ConvertAction::HorizontalRule))
        );
    }

    #[test]
    fn detect_horizontal_rule_rejects_incomplete() {
        assert_eq!(detect_horizontal_rule("--"), None);
        assert_eq!(detect_horizontal_rule("-a-"), None);
        assert_eq!(detect_horizontal_rule("--- "), None); // trailing space breaks all-same
        assert_eq!(detect_horizontal_rule("abc"), None);
        assert_eq!(detect_horizontal_rule(""), None);
    }

    #[test]
    fn detect_block_prefix_code_fence() {
        assert_eq!(
            detect_block_prefix("```"),
            Some((3, ConvertAction::CodeBlock))
        );
        assert_eq!(
            detect_block_prefix("```rust"),
            Some((7, ConvertAction::CodeBlock))
        );
        assert_eq!(
            detect_block_prefix("`````"),
            Some((5, ConvertAction::CodeBlock))
        );
    }

    #[test]
    fn detect_code_fence_rejects_incomplete() {
        assert_eq!(detect_code_fence("``"), None);
        assert_eq!(detect_code_fence("``a``"), None); // backtick after non-backtick run
        assert_eq!(detect_code_fence("`code`"), None);
    }

    #[test]
    fn detect_link_basic() {
        // "[b](u)" — cursor at col 7 (after closing `)`).
        let line = "[b](u)";
        let want = Some((1usize..2usize, "u".to_string()));
        assert_eq!(detect_link(line, line.len()), want);
    }

    #[test]
    fn detect_link_multichar() {
        let line = "[hello](https://example.com)";
        assert_eq!(
            detect_link(line, line.len()),
            Some((1usize..6usize, "https://example.com".to_string()))
        );
    }

    #[test]
    fn detect_link_rejects_image() {
        // `![alt](url)` is an image; the `[` is preceded by `!`.
        let line = "![alt](url)";
        assert_eq!(detect_link(line, line.len()), None);
    }

    #[test]
    fn detect_link_rejects_incomplete() {
        assert_eq!(detect_link("[b](u", 5), None); // no closing `)`
        assert_eq!(detect_link("[b]u)", 5), None); // no `(`
        assert_eq!(detect_link("[]()", 4), None); // empty text and url
        assert_eq!(detect_link("[b]", 3), None); // no `(url)`
    }
}
