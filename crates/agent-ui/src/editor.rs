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
}
