//! Removes `<proposed_plan>…</proposed_plan>` review blocks from assistant
//! text so they never leak into the persisted message the model re-reads next
//! turn. Line-oriented, mirroring codex's block semantics: each tag occupies a
//! line of its own — leading and trailing whitespace on that line is
//! tolerated, but any other text disqualifies it (the line is then ordinary
//! visible text). A block left open at end-of-input drops the remainder.

const OPEN_TAG: &str = "<proposed_plan>";
const CLOSE_TAG: &str = "</proposed_plan>";

/// Concatenate the visible (non-plan) text, removing plan blocks. Used when
/// rebuilding a turn's visible text from a persisted message.
pub fn strip_proposed_plan_blocks(text: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        let slug = line.trim_start().trim_end();
        if !inside && slug == OPEN_TAG {
            inside = true;
        } else if inside && slug == CLOSE_TAG {
            inside = false;
        } else if !inside {
            out.push_str(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_block_between_visible_text() {
        let text = "before\n<proposed_plan>\n- step\n</proposed_plan>\nafter";
        assert_eq!(strip_proposed_plan_blocks(text), "before\nafter");
    }

    #[test]
    fn strip_keeps_plain_text_unchanged() {
        let text = "just text\nmore text";
        assert_eq!(strip_proposed_plan_blocks(text), text);
    }

    #[test]
    fn strip_tolerates_whitespace_around_tag_line() {
        let text = "  <proposed_plan>\t\n- step\n  </proposed_plan>\n";
        assert_eq!(strip_proposed_plan_blocks(text), "");
    }

    #[test]
    fn strip_keeps_tag_line_with_extra_text() {
        let text = "  <proposed_plan> extra\n";
        assert_eq!(strip_proposed_plan_blocks(text), text);
    }

    #[test]
    fn strip_drops_remainder_after_unterminated_block() {
        let text = "<proposed_plan>\n- step 1\n";
        assert_eq!(strip_proposed_plan_blocks(text), "");
    }
}
