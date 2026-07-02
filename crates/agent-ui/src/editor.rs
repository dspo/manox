//! Markdown syntax auto-conversion glue.
//!
//! `RichTextState` (gpui-component `crates/rich_text`) has no built-in markdown
//! recognition — typing `# ` inserts a literal `#`. This module watches the
//! editor state and, when the (single-line) document starts with a complete
//! markdown prefix, strips the prefix and flips the block kind via
//! `set_block_kind` / `toggle_list`.
//!
//! Known limitations, both forced by `gpui-rich-text`'s public API:
//!
//! - `set_value` resets the cursor to (0, 0) and the crate exposes no
//!   `set_cursor` / `move_to_end` (they are `pub(crate)`). So auto-conversion
//!   is gated to single-line documents only; in a multi-block document the
//!   block-kind call would land on row 0 instead of the edited row. Use the
//!   toolbar buttons for multi-line formatting.
//!
//! - `undo()` calls `cx.notify()`, which re-fires this observer. If a user
//!   undoes past the conversion point, the restored `# ` text re-triggers
//!   conversion and the undo is "eaten". There is no public way to distinguish
//!   user input from undo, so this cannot be fully fixed here. The toolbar
//!   buttons are undo-safe (they push their own snapshot and the post-click
//!   value carries no prefix, so the observer no-ops).

use gpui::{Context, Window};
use gpui_rich_text::BlockKind;
use gpui_rich_text::RichTextState;

/// Inspect the editor state; if the document is a single line starting with a
/// complete markdown prefix, strip the prefix and apply the matching block kind.
pub fn try_apply_markdown_shortcut(
    state: &mut RichTextState,
    window: &mut Window,
    cx: &mut Context<RichTextState>,
) {
    let value = state.value();

    // set_value resets the cursor to (0, 0); block-kind calls then land on
    // row 0. Restrict to single-line documents so the edited row IS row 0.
    if value.contains('\n') {
        return;
    }

    let Some((prefix_len, action)) = detect(&value) else {
        return;
    };

    let new_value = &value[prefix_len..];
    state.set_value(new_value, window, cx);
    apply(state, action, window, cx);
}

#[derive(Debug, PartialEq, Eq)]
enum ConvertAction {
    Heading(u8),
    UnorderedList,
    OrderedList,
}

/// Match a line against a complete markdown block prefix (trailing space
/// required, no leading whitespace). Returns the prefix length and the block
/// action to apply once the prefix is stripped.
fn detect(line: &str) -> Option<(usize, ConvertAction)> {
    if !line.ends_with(' ') || line.starts_with(' ') {
        return None;
    }

    const HEADINGS: &[(&str, u8)] = &[
        ("# ", 1),
        ("## ", 2),
        ("### ", 3),
        ("#### ", 4),
        ("##### ", 5),
        ("###### ", 6),
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
    None
}

fn apply(
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
    }
}

#[cfg(test)]
mod tests {
    use super::{ConvertAction, detect};

    fn heading(line: &str) -> Option<(usize, u8)> {
        detect(line).and_then(|(len, action)| match action {
            ConvertAction::Heading(l) => Some((len, l)),
            _ => None,
        })
    }

    #[test]
    fn detect_headings() {
        assert_eq!(heading("# "), Some((2, 1)));
        assert_eq!(heading("## "), Some((3, 2)));
        assert_eq!(heading("### "), Some((4, 3)));
        assert_eq!(heading("#### "), Some((5, 4)));
        assert_eq!(heading("##### "), Some((6, 5)));
        assert_eq!(heading("###### "), Some((7, 6)));
    }

    #[test]
    fn detect_lists() {
        assert_eq!(detect("- "), Some((2, ConvertAction::UnorderedList)));
        assert_eq!(detect("* "), Some((2, ConvertAction::UnorderedList)));
        assert_eq!(detect("1. "), Some((3, ConvertAction::OrderedList)));
    }

    #[test]
    fn detect_rejects_incomplete_prefix() {
        assert_eq!(detect("#"), None);
        assert_eq!(detect("##"), None);
        assert_eq!(detect("#hello"), None);
        assert_eq!(detect("##hello "), None);
    }

    #[test]
    fn detect_rejects_leading_whitespace() {
        assert_eq!(detect(" # "), None);
        assert_eq!(detect("  - "), None);
        assert_eq!(detect("\t1. "), None);
    }

    #[test]
    fn detect_rejects_plain_text() {
        assert_eq!(detect(""), None);
        assert_eq!(detect("hello "), None);
        assert_eq!(detect("hello world"), None);
        assert_eq!(detect("2. "), None);
        assert_eq!(detect("a. "), None);
    }
}
