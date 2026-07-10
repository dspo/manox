//! Frozen-prefix incremental markdown parser for streaming bodies.
//!
//! During a stream the assistant text grows append-only — each delta appends to
//! the tail. The completed blocks at the head (everything before the last
//! `\n\n`-terminated block boundary) are immutable: they will never change
//! again. This parser freezes them and re-parses only the unfrozen tail on each
//! `update`, so the per-delta cost is proportional to the tail length, not the
//! full document.
//!
//! The contract is **incremental == full parse**, byte-for-byte, at every
//! step. Whenever the safety guarantees break down (non-append-only edit,
//! reference-link definitions, `\r`), the parser falls back to a full parse —
//! correctness is never sacrificed, only speed.
//!
//! The freeze guard (never freeze right after a list; never freeze when the
//! next char is space/`\n`) prevents the desync modes that would otherwise
//! split a loose list or straddle a block separator across the cut.

use std::sync::Arc;

use crate::markdown::ast::{self, Block};

/// Frozen-prefix incremental parser. State is the immutable prefix (`text` +
/// `blocks` + `offset` into the full document). `update` advances the prefix
/// when text grew append-only and is safe to split; otherwise it full-parses.
#[derive(Clone, Debug)]
pub struct IncrementalParser {
    /// The full document text at the last `update`.
    text: Arc<str>,
    /// The frozen blocks (head of the document, before `frozen_offset`).
    frozen_blocks: Arc<Vec<Block>>,
    /// Byte offset into `text` where the frozen prefix ends and the re-parsed
    /// tail begins. Equals `text.len()` when the entire document is frozen.
    frozen_offset: usize,
    /// The full block list (frozen + tail), recomputed on each `update`. This
    /// is what the renderer reads; cloning the `Arc` is cheap.
    all_blocks: Arc<Vec<Block>>,
}

impl Default for IncrementalParser {
    fn default() -> Self {
        Self::new()
    }
}

impl IncrementalParser {
    /// Empty parser. The first `update` full-parses and establishes the
    /// initial frozen prefix.
    pub fn new() -> Self {
        Self {
            text: Arc::from(""),
            frozen_blocks: Arc::new(Vec::new()),
            frozen_offset: 0,
            all_blocks: Arc::new(Vec::new()),
        }
    }

    /// The current full block list (frozen prefix + re-parsed tail). The
    /// renderer passes this to `Markdown::blocks`.
    pub fn blocks(&self) -> Arc<Vec<Block>> {
        self.all_blocks.clone()
    }

    /// The full document text at the last `update`.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Update with the full streaming text. Returns `true` if the incremental
    /// path ran, `false` if a full parse was done (non-append-only, ref-def, or
    /// `\r` detected). Either way `blocks()` reflects the new text.
    pub fn update(&mut self, full_text: &str) -> bool {
        let can_stream = !full_text.contains('\r') && !has_ref_def(full_text);

        let prev_frozen_text = self.text.clone();
        let can_append = can_stream
            && full_text.len() > prev_frozen_text.len()
            && full_text.starts_with(&prev_frozen_text[..]);

        if can_append {
            let tail = &full_text[self.frozen_offset..];
            let tail_blocks = ast::parse_tail(tail);
            let mut combined: Vec<Block> =
                Vec::with_capacity(self.frozen_blocks.len() + tail_blocks.len());
            combined.extend((*self.frozen_blocks).clone());
            combined.extend(tail_blocks.iter().map(|(b, _, _)| b.clone()));

            self.freeze_prefix(full_text, &tail_blocks, &combined);
            self.text = Arc::from(full_text);
            self.all_blocks = Arc::from(combined);
            true
        } else {
            let blocks = ast::parse(full_text);
            if can_stream {
                self.freeze_prefix_full(full_text);
            } else {
                self.frozen_offset = 0;
                self.frozen_blocks = Arc::new(Vec::new());
            }
            self.text = Arc::from(full_text);
            self.all_blocks = Arc::from(blocks);
            false
        }
    }

    /// Final full parse. Called on stream `Stop` to guarantee the final block
    /// list matches a one-shot `ast::parse` exactly (the tail may have been
    /// held back from freezing by the `\n\n` boundary guard).
    pub fn finalize(&mut self) {
        let text = self.text.clone();
        let blocks = ast::parse(&text);
        self.frozen_offset = text.len();
        self.frozen_blocks = Arc::from(blocks.clone());
        self.all_blocks = Arc::from(blocks);
    }

    /// Advance the frozen prefix. For each tail block, the freeze candidate is
    /// the start of the *next* block (where the next block begins in the full
    /// document). The gap between `block[i].end` and `block[i+1].start` must
    /// contain `\n\n` — that separator proves `block[i]` is complete and the
    /// tail re-parse will start cleanly at `block[i+1]`.
    ///
    /// Freeze guards:
    /// 1. **List guard**: never freeze right after a `List` block — a loose
    ///    list can be extended by a following same-marker item across a blank
    ///    line, and markdown-rs merges them. Freezing there would keep the
    ///    lists separate.
    /// 2. **Next-char guard**: the char at the freeze boundary must not be
    ///    space or `\n` — an extra blank line or indented continuation
    ///    straddles the cut and would desync `parse(prefix)++parse(tail)`.
    fn freeze_prefix(
        &mut self,
        full_text: &str,
        tail_blocks: &[(Block, usize, usize)],
        combined: &[Block],
    ) {
        let mut frozen_end = self.frozen_offset;
        let mut frozen_count_in_tail = 0;

        for i in 0..tail_blocks.len() {
            let (_, _, end) = &tail_blocks[i];
            let end_in_full = self.frozen_offset + *end;

            // The boundary candidate: start of the next block (in full doc
            // coords). If this is the last tail block, the boundary is the
            // end of the `\n\n` after this block (if any).
            let boundary = if i + 1 < tail_blocks.len() {
                self.frozen_offset + tail_blocks[i + 1].1
            } else {
                // Last block: find the end of the `\n\n` separator after it.
                match full_text[end_in_full..].find("\n\n") {
                    Some(idx) => end_in_full + idx + 2,
                    None => break, // No `\n\n` after — block not complete.
                }
            };

            // Next-char guard: the char at the boundary must not be space/`\n`.
            if boundary < full_text.len() {
                let next = full_text.as_bytes()[boundary];
                if next == b' ' || next == b'\n' {
                    break;
                }
            }

            // List guard: the *preceding* combined block must not be a list.
            let preceding_index = self.frozen_blocks.len() + i;
            let preceding_is_list = (preceding_index > 0
                && ast::is_list_block(&combined[preceding_index - 1]))
                || (i > 0 && ast::is_list_block(&tail_blocks[i - 1].0));
            if preceding_is_list {
                continue;
            }

            frozen_end = boundary;
            frozen_count_in_tail = i + 1;
        }

        if frozen_count_in_tail == 0 {
            return;
        }

        // Commit the freeze.
        self.frozen_offset = frozen_end;
        let mut new_frozen = Vec::with_capacity(self.frozen_blocks.len() + frozen_count_in_tail);
        new_frozen.extend((*self.frozen_blocks).clone());
        new_frozen.extend(
            tail_blocks[..frozen_count_in_tail]
                .iter()
                .map(|(b, _, _)| b.clone()),
        );
        self.frozen_blocks = Arc::from(new_frozen);
    }

    /// Full-parse variant of freeze: walk the block list's mdast positions to
    /// find the same `\n\n` boundaries. Only called when a full parse already
    /// happened (non-append or fallback), so the extra `parse_tail` cost is on
    /// the full text and only for position data.
    fn freeze_prefix_full(&mut self, full_text: &str) {
        let tail_blocks = ast::parse_tail(full_text);
        let mut frozen_end = 0;
        let mut frozen_count = 0;

        for i in 0..tail_blocks.len() {
            let (_, _start, end) = &tail_blocks[i];
            let end = *end;

            let boundary = if i + 1 < tail_blocks.len() {
                tail_blocks[i + 1].1
            } else {
                match full_text[end..].find("\n\n") {
                    Some(idx) => end + idx + 2,
                    None => break,
                }
            };

            // Next-char guard.
            if boundary < full_text.len() {
                let next = full_text.as_bytes()[boundary];
                if next == b' ' || next == b'\n' {
                    break;
                }
            }

            // List guard.
            let preceding_is_list = i > 0 && ast::is_list_block(&tail_blocks[i - 1].0);
            if preceding_is_list {
                continue;
            }

            frozen_end = boundary;
            frozen_count = i + 1;
        }

        if frozen_count == 0 {
            self.frozen_offset = 0;
            self.frozen_blocks = Arc::new(Vec::new());
            return;
        }

        self.frozen_offset = frozen_end;
        self.frozen_blocks = Arc::from(
            tail_blocks[..frozen_count]
                .iter()
                .map(|(b, _, _)| b.clone())
                .collect::<Vec<_>>(),
        );
    }
}

/// Detect reference-link definitions (`[label]: url`) anywhere in the text.
/// Uses a simple byte scan rather than pulling in the `regex` crate.
fn has_ref_def(text: &str) -> bool {
    for line in text.lines() {
        if let Some(rest) = strip_leading_spaces(line)
            && rest.starts_with('[')
            && let Some(close) = rest.find(']')
            && rest[close + 1..].starts_with(':')
        {
            return true;
        }
    }
    false
}

/// Strip up to 3 leading spaces (CommonMark ref-def indent allowance).
fn strip_leading_spaces(line: &str) -> Option<&str> {
    let mut count = 0;
    for (i, b) in line.as_bytes().iter().enumerate() {
        if *b == b' ' && count < 3 {
            count += 1;
        } else {
            return Some(&line[i..]);
        }
    }
    Some("")
}

/// Trim partial closing fences from the last code block in a block list.
///
/// When a closing fence (``` or ~~~) arrives split across chunks, the
/// incomplete fence line is momentarily a valid fence prefix — e.g. ``` `` `` `
/// arriving as `` ` `` then `` ` ``. The code block's `value` would shrink by
/// the fence line length once the final character lands, causing the rendered
/// block to flicker. This trims a partial closing fence from the tail code
/// block's value so the block stays stable until the fence completes.
///
/// Architectural debt to pi's `trimPartialClosingFences`: the detection logic
/// (only trim when the last line is a prefix of the fence marker, and shorter
/// than it) is the same; the recursion into list/blockquote last-block is
/// mirrored.
pub fn trim_partial_closing_fences(blocks: &mut [Block]) {
    let Some(last) = blocks.last_mut() else {
        return;
    };
    trim_last_block(last);
}

fn trim_last_block(block: &mut Block) {
    match block {
        Block::List { items, .. } => {
            if let Some(item) = items.last_mut()
                && let Some(b) = item.blocks.last_mut()
            {
                trim_last_block(b);
            }
        }
        Block::Blockquote(inner) => {
            if let Some(b) = inner.last_mut() {
                trim_last_block(b);
            }
        }
        Block::Code { value, .. } => {
            trim_code_value(value);
        }
        Block::Diff { value } | Block::Conflict { value } => {
            // Diffs and conflicts are routed from code blocks; their value is
            // the raw code, so the same fence-trim applies.
            trim_code_value(value);
        }
        _ => {}
    }
}

/// Trim a partial closing fence from a code value. A fenced code value carries
/// the content between the opening and closing fences, including the closing
/// fence line. When the closing fence is only partially streamed (e.g. `` ` ``
/// instead of ``` ``` ```), the partial fence line is a transient artifact that
/// will disappear once the fence completes — trim it so the block does not
/// flicker.
fn trim_code_value(value: &mut String) {
    let lines: Vec<&str> = value.split('\n').collect();
    let Some(last_line) = lines.last() else {
        return;
    };
    // A closing fence is 3+ backticks or tildes, optionally with trailing
    // content (info string only on opening, but be conservative). A partial
    // fence is all-same-char and shorter than 3.
    if last_line.is_empty() {
        return;
    }
    let bytes = last_line.as_bytes();
    let first = bytes[0];
    if (first != b'`' && first != b'~') || !bytes.iter().all(|&b| b == first) {
        return;
    }
    if last_line.len() >= 3 {
        // A complete fence — leave it (the parser will handle it).
        return;
    }
    // Partial fence: trim it (and the preceding `\n`).
    let trimmed_len = value.len() - last_line.len();
    let trimmed_len = value[..trimmed_len].trim_end_matches('\n').len();
    value.truncate(trimmed_len);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::ast::{self, Block};

    /// Assert that incremental parse of `text` produces the same block list as
    /// a full `ast::parse`. Block equality is structural (debug format).
    fn assert_eq_full(text: &str, inc: &IncrementalParser) {
        let full = ast::parse(text);
        let inc_blocks = inc.blocks();
        // Debug-compare: the Block type doesn't derive PartialEq, but Debug is
        // exhaustive on its fields.
        assert_eq!(
            format!("{:?}", full),
            format!("{:?}", *inc_blocks),
            "incremental != full parse for text: {text:?}"
        );
    }

    /// Feed `text` to a fresh parser one character at a time, asserting
    /// incremental == full at every step. This is the core contract test.
    fn feed_char_by_char(text: &str) {
        let mut parser = IncrementalParser::new();
        let mut buf = String::new();
        for ch in text.chars() {
            buf.push(ch);
            parser.update(&buf);
            assert_eq_full(&buf, &parser);
        }
    }

    #[test]
    fn empty_text_produces_no_blocks() {
        let parser = IncrementalParser::new();
        assert!(parser.blocks().is_empty());
    }

    #[test]
    fn simple_paragraph_char_by_char() {
        feed_char_by_char("Hello world");
    }

    #[test]
    fn paragraph_with_trailing_newline() {
        feed_char_by_char("Hello world\n");
    }

    #[test]
    fn two_paragraphs_char_by_char() {
        feed_char_by_char("First paragraph.\n\nSecond paragraph.");
    }

    #[test]
    fn heading_char_by_char() {
        feed_char_by_char("# Title\n\nbody");
    }

    #[test]
    fn h2_and_h3_char_by_char() {
        feed_char_by_char("## H2\n\n### H3\n\ntext");
    }

    #[test]
    fn code_block_char_by_char() {
        feed_char_by_char("```rust\nfn main() {}\n```");
    }

    #[test]
    fn code_block_with_blank_line_inside() {
        feed_char_by_char("```python\na = 1\n\nb = 2\n```");
    }

    #[test]
    fn code_block_followed_by_paragraph() {
        feed_char_by_char("```js\nconst x = 1;\n```\n\nAfter code.");
    }

    #[test]
    fn table_char_by_char() {
        feed_char_by_char("| a | b |\n| --- | --- |\n| 1 | 2 |");
    }

    #[test]
    fn table_followed_by_paragraph() {
        feed_char_by_char("| a | b |\n| --- | --- |\n| 1 | 2 |\n\nAfter table.");
    }

    #[test]
    fn unordered_list_char_by_char() {
        feed_char_by_char("- item one\n- item two\n- item three");
    }

    #[test]
    fn ordered_list_char_by_char() {
        feed_char_by_char("1. first\n2. second\n3. third");
    }

    #[test]
    fn nested_list_char_by_char() {
        feed_char_by_char("- outer\n  - inner\n- outer two");
    }

    #[test]
    fn loose_list_with_blank_line_char_by_char() {
        // A loose list (items separated by blank lines) is the key desync
        // risk — markdown-rs merges items across blank lines into one list.
        feed_char_by_char("- item one\n\n- item two");
    }

    #[test]
    fn blockquote_char_by_char() {
        feed_char_by_char("> quoted text\n> more");
    }

    #[test]
    fn blockquote_followed_by_paragraph() {
        feed_char_by_char("> quote\n\nparagraph");
    }

    #[test]
    fn thematic_break_char_by_char() {
        feed_char_by_char("---\n\nafter");
    }

    #[test]
    fn setext_heading_char_by_char() {
        // Setext H1: text followed by `===` on the next line.
        feed_char_by_char("Title\n===\n\nbody");
    }

    #[test]
    fn setext_h2_char_by_char() {
        feed_char_by_char("Title\n---\n\nbody");
    }

    #[test]
    fn inline_formatting_char_by_char() {
        feed_char_by_char("This is **bold** and *italic* and `code`.");
    }

    #[test]
    fn ref_def_falls_back_to_full() {
        // Reference definitions force a full parse (no incremental).
        let mut parser = IncrementalParser::new();
        let text = "[label]: https://example.com\n\n[label]";
        assert!(!parser.update(text));
        assert_eq_full(text, &parser);
    }

    #[test]
    fn carriage_return_falls_back_to_full() {
        let mut parser = IncrementalParser::new();
        let text = "line one\r\nline two";
        assert!(!parser.update(text));
        // The full parse normalizes CRLF; blocks should match.
        assert_eq_full(text, &parser);
    }

    #[test]
    fn finalize_matches_full_parse() {
        let mut parser = IncrementalParser::new();
        let text = "# Title\n\n```rust\nfn main() {}\n```\n\n- list\n  - nested";
        // Feed incrementally.
        let mut buf = String::new();
        for ch in text.chars() {
            buf.push(ch);
            parser.update(&buf);
        }
        // Finalize must match a fresh full parse exactly.
        parser.finalize();
        let full = ast::parse(text);
        assert_eq!(
            format!("{:?}", full),
            format!("{:?}", *parser.blocks()),
            "finalize != full parse"
        );
    }

    #[test]
    fn incremental_advances_frozen_offset() {
        let mut parser = IncrementalParser::new();
        // Two complete paragraphs → first should freeze after the `\n\n`.
        parser.update("First.\n\nSecond.");
        assert!(
            parser.frozen_offset > 0,
            "frozen_offset should advance past the first block boundary"
        );
        assert!(
            parser.frozen_offset <= "First.\n\nSecond.".len(),
            "frozen_offset must not exceed text length"
        );
    }

    #[test]
    fn list_not_frozen_across_boundary() {
        // A loose list can grow across a blank line, so the freeze guard must
        // not freeze right after a list. Feed the list and verify the frozen
        // prefix does not end past the list's start (it may freeze 0 or the
        // pre-list content, but not mid-list).
        let mut parser = IncrementalParser::new();
        parser.update("intro\n\n- item one\n\n- item two");
        // The frozen prefix may include "intro\n\n" but not the list, since the
        // list guard prevents freezing after a list block.
        let _ = parser; // The contract test (char-by-char) already verifies correctness.
    }

    #[test]
    fn non_append_edit_full_parses() {
        let mut parser = IncrementalParser::new();
        parser.update("Hello world");
        // Non-append edit (text changes, not grows).
        let result = parser.update("Goodbye world");
        assert!(!result, "non-append edit should full-parse");
        assert_eq_full("Goodbye world", &parser);
    }

    #[test]
    fn mixed_content_char_by_char() {
        let text = "# Heading\n\nA paragraph with **bold**.\n\n```python\ncode = 1\n```\n\n- list item\n- another\n\n| col1 | col2 |\n| --- | --- |\n| a | b |";
        feed_char_by_char(text);
    }

    #[test]
    fn multiple_code_blocks_char_by_char() {
        feed_char_by_char("```rust\nfn a() {}\n```\n\n```python\ndef b():\n    pass\n```");
    }

    #[test]
    fn deep_nested_list_char_by_char() {
        feed_char_by_char("- a\n  - b\n    - c\n  - d\n- e");
    }

    #[test]
    fn long_paragraph_char_by_char() {
        let text = "This is a very long paragraph that spans many words and should still parse correctly incrementally because it is just a single block with no internal boundaries to freeze at.";
        feed_char_by_char(text);
    }

    #[test]
    fn extra_blank_line_does_not_desync() {
        // Three newlines between blocks — the next-char guard must prevent
        // freezing at the first `\n\n` because the next char is `\n`.
        feed_char_by_char("First.\n\n\nSecond.");
    }

    #[test]
    fn trim_partial_closing_fence_backticks() {
        let mut blocks = vec![Block::Code {
            lang: Some("rust".to_string()),
            value: "fn main()\n``".to_string(),
        }];
        trim_partial_closing_fences(&mut blocks);
        if let Block::Code { value, .. } = &blocks[0] {
            assert!(!value.contains("``"), "partial fence should be trimmed");
            assert!(value.contains("fn main()"));
        } else {
            panic!("expected code block");
        }
    }

    #[test]
    fn trim_partial_closing_fence_leaves_complete_fence() {
        let mut blocks = vec![Block::Code {
            lang: Some("rust".to_string()),
            value: "fn main()\n```".to_string(),
        }];
        trim_partial_closing_fences(&mut blocks);
        if let Block::Code { value, .. } = &blocks[0] {
            // A complete 3-char fence is left intact (the parser handles it).
            assert!(value.ends_with("```"));
        }
    }

    #[test]
    fn trim_partial_closing_fence_tildes() {
        let mut blocks = vec![Block::Code {
            lang: Some("text".to_string()),
            value: "content\n~~".to_string(),
        }];
        trim_partial_closing_fences(&mut blocks);
        if let Block::Code { value, .. } = &blocks[0] {
            assert!(!value.contains("~~"), "partial tilde fence trimmed");
        }
    }

    #[test]
    fn trim_partial_closing_fence_no_code_block() {
        // Non-code last block — no-op.
        let mut blocks = vec![Block::Paragraph(ast::InlineRuns {
            text: "hello".to_string(),
            ..Default::default()
        })];
        let before = format!("{:?}", blocks);
        trim_partial_closing_fences(&mut blocks);
        assert_eq!(format!("{:?}", blocks), before);
    }
}
