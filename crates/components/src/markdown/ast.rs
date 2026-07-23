//! mdast → manox `Block` model. The sole file that touches the `markdown` crate.
//!
//! Inline children are flattened to a single `String` + a list of
//! `(Range<usize>, HighlightStyle)` overlays + a list of code-span byte ranges
//! at parse time, so per-frame rendering is just `RichText` — no re-walking
//! the AST. The model is purely structural: colors and radii are applied at
//! render time from `MdStyles`, so parsing needs no theme.

use std::ops::Range;

use gpui::{FontStyle, FontWeight, HighlightStyle};
use markdown::mdast::{AlignKind, Node};
use markdown::{ParseOptions, to_mdast};

/// The kind of a clickable link span.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    /// An explicit markdown `[text](url)` or auto-detected bare URL.
    Url,
    /// A filesystem path auto-detected in plain text.
    FilePath,
}

/// A clickable span embedded in inline text, carrying the display range and the
/// resolved link target.
#[derive(Clone, Debug)]
pub struct LinkSpan {
    /// Byte range (into `InlineRuns::text`) of the display text.
    pub range: Range<usize>,
    /// The link target: URL or path string.
    pub url: String,
    pub kind: LinkKind,
}

/// A contiguous run of inline text with style overlays on top of the base
/// font/color (which `RichText` inherits from `window.text_style()`).
/// `code_ranges` marks inline-code segments that get a rounded wash behind the
/// glyphs — the wash is painted by the renderer, not carried as a run
/// `background_color`, so it can be rounded and caller-customized.
#[derive(Clone, Debug, Default)]
pub struct InlineRuns {
    pub text: String,
    pub highlights: Vec<(Range<usize>, HighlightStyle)>,
    pub code_ranges: Vec<Range<usize>>,
    pub link_spans: Vec<LinkSpan>,
}

/// Column alignment for table cells. The manox-owned mirror of mdast's
/// `AlignKind` so the renderer does not depend on the `markdown` crate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TableAlign {
    #[default]
    None,
    Left,
    Center,
    Right,
}

/// A list item: its `checked` flag carries GFM task-list state (`[ ]`/`[x]`),
/// `None` for plain bullets/numbers.
#[derive(Clone, Debug)]
pub struct ListItem {
    pub checked: Option<bool>,
    pub blocks: Vec<Block>,
}

/// A block-level node in the manox-owned document model.
#[derive(Clone, Debug)]
pub enum Block {
    Paragraph(InlineRuns),
    Heading {
        depth: u8,
        runs: InlineRuns,
    },
    Code {
        lang: Option<String>,
        value: String,
    },
    /// Unified-diff blob routed by `lang == "diff"`. Rendered with per-line
    /// add/remove washes instead of a line-number gutter.
    Diff {
        value: String,
    },
    /// git merge-conflict blob routed by `<<<<<<<`/`>>>>>>>` markers in a fenced
    /// code run, regardless of declared language — the conflict structure is
    /// more specific than a diff or plain code, so it takes precedence over
    /// the `diff` language route. Rendered with per-section ours/base/theirs
    /// washes + colored left bars.
    Conflict {
        value: String,
    },
    Blockquote(Vec<Block>),
    List {
        ordered: bool,
        items: Vec<ListItem>,
    },
    Table {
        rows: Vec<Vec<InlineRuns>>,
        align: Vec<TableAlign>,
    },
    ThematicBreak,
}

/// Parse markdown source into manox blocks. GFM parse options so tables /
/// strikethrough round-trip through the AST even before they are rendered.
pub fn parse(src: &str) -> Vec<Block> {
    let opts = ParseOptions::gfm();
    match to_mdast(src, &opts) {
        Ok(Node::Root(root)) => root.children.iter().filter_map(block_of).collect(),
        _ => Vec::new(),
    }
}

/// Parse a *tail* slice of a streaming document independently. The offsets are
/// relative to the tail (start at 0), not the full document — the incremental
/// parser stitches frozen prefix blocks + tail blocks together. Returns each
/// block alongside the mdast `position` start and end byte offsets of its
/// top-level node, so the incrementer can locate the `\n\n` separator *between*
/// blocks to advance the frozen boundary. Positions are populated by default in
/// markdown-rs; note that mdast `position` does not include trailing inter-block
/// whitespace, so the `\n\n` separator lives in the gap between one block's
/// `end` and the next block's `start`.
pub(crate) fn parse_tail(src: &str) -> Vec<(Block, usize, usize)> {
    let opts = ParseOptions::gfm();
    match to_mdast(src, &opts) {
        Ok(Node::Root(root)) => root
            .children
            .iter()
            .filter_map(|node| {
                let pos = node.position();
                let start = pos.map(|p| p.start.offset).unwrap_or(0);
                let end = pos.map(|p| p.end.offset).unwrap_or(src.len());
                block_of(node).map(|b| (b, start, end))
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether a block is a `List`. The freeze guard refuses to advance the frozen
/// boundary past a list — a loose list can be extended by a following
/// same-marker item across a blank line, and markdown-rs merges them into one
/// renumbered list. Freezing at that cut would keep the lists separate,
/// desyncing incremental from full parse.
pub(crate) fn is_list_block(b: &Block) -> bool {
    matches!(b, Block::List { .. })
}

pub(crate) fn block_of(node: &Node) -> Option<Block> {
    match node {
        Node::Paragraph(p) => Some(Block::Paragraph(inline_of(&p.children))),
        Node::Heading(h) => Some(Block::Heading {
            depth: h.depth,
            runs: inline_of(&h.children),
        }),
        Node::Code(c) => {
            if is_conflict(&c.value) {
                Some(Block::Conflict {
                    value: c.value.clone(),
                })
            } else if c.lang.as_deref() == Some("diff") {
                Some(Block::Diff {
                    value: c.value.clone(),
                })
            } else {
                Some(Block::Code {
                    lang: c.lang.clone(),
                    value: c.value.clone(),
                })
            }
        }
        Node::Blockquote(b) => Some(Block::Blockquote(
            b.children.iter().filter_map(block_of).collect(),
        )),
        Node::List(l) => {
            let items = l
                .children
                .iter()
                .filter_map(|n| match n {
                    Node::ListItem(li) => Some(ListItem {
                        checked: li.checked,
                        blocks: li.children.iter().filter_map(block_of).collect(),
                    }),
                    _ => None,
                })
                .collect();
            Some(Block::List {
                ordered: l.ordered,
                items,
            })
        }
        Node::ThematicBreak(_) => Some(Block::ThematicBreak),
        Node::Table(t) => {
            let rows = t
                .children
                .iter()
                .filter_map(|row| match row {
                    Node::TableRow(tr) => Some(
                        tr.children
                            .iter()
                            .map(|cell| match cell {
                                Node::TableCell(tc) => inline_of(&tc.children),
                                _ => InlineRuns::default(),
                            })
                            .collect(),
                    ),
                    _ => None,
                })
                .collect();
            Some(Block::Table {
                rows,
                align: t.align.iter().map(map_align).collect(),
            })
        }
        // Tables, HTML, math, and MDX nodes arrive in later steps.
        _ => None,
    }
}

fn map_align(a: &AlignKind) -> TableAlign {
    match a {
        AlignKind::Left => TableAlign::Left,
        AlignKind::Right => TableAlign::Right,
        AlignKind::Center => TableAlign::Center,
        AlignKind::None => TableAlign::None,
    }
}

/// A fenced code run is a git merge conflict when it carries both the opening
/// `<<<<<<<` and closing `>>>>>>>` markers. The pair (rather than just one) is
/// required so a stray `>>>>>>>` in commentary does not mis-route a real code
/// block into the conflict renderer.
fn is_conflict(value: &str) -> bool {
    let mut open = false;
    let mut close = false;
    for line in value.lines() {
        if line.starts_with("<<<<<<<") {
            open = true;
        } else if line.starts_with(">>>>>>>") {
            close = true;
        }
    }
    open && close
}

/// Inline emphasis (`**strong**`, `*em*`, `~~del~~`, `` `code` ``) is folded
/// into the segment highlights here; the heading's own weight/italic/underline
/// is applied by the renderer on the heading's div, so this collector stays
/// emphasis-only and never bakes a base weight into a heading's plain text.
fn inline_of(children: &[Node]) -> InlineRuns {
    let mut runs = InlineRuns::default();
    collect_inline(children, ActiveStyle::default(), &mut runs);
    linkify(&mut runs);
    runs
}

/// Scan plain text for bare URLs and file-system paths and record them as
/// `LinkSpan`s. Skips byte ranges already covered by explicit markdown links
/// (added by `collect_inline`) so auto-detection does not double-link text.
fn linkify(runs: &mut InlineRuns) {
    let text = &runs.text;
    let mut covered: Vec<Range<usize>> = runs.link_spans.iter().map(|s| s.range.clone()).collect();
    covered.sort_by_key(|r| r.start);

    // Collect new spans, then push them in one go so the immutable borrow on
    // `text` does not overlap with the mutable borrow on `runs.link_spans`.
    let mut new_spans: Vec<LinkSpan> = Vec::new();

    // --- URL detection: https?://... ---
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `http://` or `https://`
        let proto = if i + 7 < bytes.len() && &bytes[i..i + 7] == b"http://" {
            7
        } else if i + 8 < bytes.len() && &bytes[i..i + 8] == b"https://" {
            8
        } else {
            i += 1;
            continue;
        };

        let url_start = i;
        let mut url_end = i + proto;
        while url_end < bytes.len() {
            let b = bytes[url_end];
            if b.is_ascii_whitespace()
                || matches!(
                    b,
                    b'<' | b'>' | b'"' | b'{' | b'}' | b'|' | b'\\' | b'`' | b'^' | b'\''
                )
            {
                break;
            }
            url_end += 1;
        }
        // Backtrack trailing punctuation that is unlikely to be part of the URL.
        while url_end > i + proto
            && matches!(bytes[url_end - 1], b'.' | b',' | b';' | b':' | b')' | b'\'')
        {
            url_end -= 1;
        }

        if url_end > i + proto && !is_covered(url_start..url_end, &covered) {
            let url = &text[url_start..url_end];
            new_spans.push(LinkSpan {
                range: url_start..url_end,
                url: url.to_string(),
                kind: LinkKind::Url,
            });
        }
        i = url_end;
    }

    // --- File-path detection ---
    linkify_paths(text, &covered, &mut new_spans);

    runs.link_spans.extend(new_spans);
}

/// Whether `range` overlaps any span in `covered` (sorted, non-overlapping).
fn is_covered(range: Range<usize>, covered: &[Range<usize>]) -> bool {
    for cov in covered {
        if cov.start < range.end && range.start < cov.end {
            return true;
        }
    }
    false
}

/// Detect filesystem-path patterns in `text` and push matching `LinkSpan`s.
///
/// A path must:
/// - Contain at least one `/` directory separator.
/// - End with a recognised file extension (`.rs`, `.go`, …) or a line-number
///   suffix (`:42`, `:42-100`).
/// - Start at a word boundary: after whitespace, `(`, `[`, `"`, or at position 0.
fn linkify_paths(text: &str, covered: &[Range<usize>], out: &mut Vec<LinkSpan>) {
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        // Advance to a potential path start: after whitespace / delimiter.
        let is_boundary = i == 0
            || matches!(
                bytes[i - 1],
                b' ' | b'\t' | b'\n' | b'(' | b'[' | b'"' | b'\''
            );
        if !is_boundary {
            // Also allow paths starting with `/` or `./` or `../` even mid-text.
            if !(bytes[i] == b'/'
                || (i + 1 < len && bytes[i] == b'.' && bytes[i + 1] == b'/')
                || (i + 2 < len
                    && bytes[i] == b'.'
                    && bytes[i + 1] == b'.'
                    && bytes[i + 2] == b'/'))
            {
                i += 1;
                continue;
            }
        }

        // Collect the candidate span character by character.
        let maybe = collect_path_candidate(bytes, i);
        if maybe.is_none() {
            i += 1;
            continue;
        }
        let end = maybe.unwrap();

        if is_path_like(&text[i..end]) && !is_covered(i..end, covered) {
            out.push(LinkSpan {
                range: i..end,
                url: text[i..end].to_string(),
                kind: LinkKind::FilePath,
            });
        }
        i = end;
    }
}

/// Collect a maximal run of path-safe characters starting at `pos`.
/// Returns `Some(end)` or `None` if the candidate is trivially not a path.
fn collect_path_candidate(bytes: &[u8], pos: usize) -> Option<usize> {
    let mut end = pos;
    while end < bytes.len() {
        let b = bytes[end];
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'@' | b'/' | b':') {
            end += 1;
        } else {
            break;
        }
    }
    // Must span at least one `/` to be a path.
    if !bytes[pos..end].contains(&b'/') {
        return None;
    }
    Some(end)
}

/// Check whether a candidate string looks like a filesystem path: contains at
/// least one `/` and ends with either a known extension or a `:NN` line-number
/// suffix.
fn is_path_like(s: &str) -> bool {
    if !s.contains('/') {
        return false;
    }
    // Line-number suffix: `:42` or `:42-100`.
    if let Some(colon) = s.rfind(':') {
        let after = &s[colon + 1..];
        if after.chars().all(|c| c.is_ascii_digit() || c == '-')
            && after.contains(|c: char| c.is_ascii_digit())
        {
            return true;
        }
    }
    // Known file extension heuristic.
    if let Some(dot) = s.rfind('.') {
        let ext = &s[dot + 1..];
        let ext_len = ext.len();
        if (1..=10).contains(&ext_len) && ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return true;
        }
    }
    false
}

/// Cumulative inline formatting at the current recursion depth. One range is
/// emitted per `Text`/`InlineCode` segment carrying the *combined* style of
/// every active overlay, so the resulting ranges are non-overlapping and
/// sorted — the run builder requires both, and overlapping ranges (one per
/// formatting node) misalign run lengths and slice mid-codepoint.
#[derive(Clone, Copy, Default)]
struct ActiveStyle {
    bold: bool,
    italic: bool,
    strikethrough: bool,
}

impl ActiveStyle {
    /// Build the highlight for the current segment. Returns `None` when no
    /// overlay is active so plain text stays a single base-style run. Inline
    /// code contributes no highlight here — its wash is painted separately by
    /// the renderer from `InlineRuns::code_ranges`, so it can be rounded and
    /// caller-customized rather than a flat run background.
    fn highlight(&self) -> Option<HighlightStyle> {
        let hs = HighlightStyle {
            font_weight: self.bold.then_some(FontWeight::BOLD),
            font_style: self.italic.then_some(FontStyle::Italic),
            strikethrough: self
                .strikethrough
                .then_some(gpui::StrikethroughStyle::default()),
            ..Default::default()
        };
        (hs != HighlightStyle::default()).then_some(hs)
    }
}

fn collect_inline(children: &[Node], active: ActiveStyle, runs: &mut InlineRuns) {
    for node in children {
        match node {
            Node::Text(t) => {
                let start = runs.text.len();
                runs.text.push_str(&t.value);
                let end = runs.text.len();
                if let Some(hs) = active.highlight() {
                    runs.highlights.push((start..end, hs));
                }
            }
            Node::Strong(s) => {
                let mut a = active;
                a.bold = true;
                collect_inline(&s.children, a, runs);
            }
            Node::Emphasis(e) => {
                let mut a = active;
                a.italic = true;
                collect_inline(&e.children, a, runs);
            }
            Node::InlineCode(c) => {
                // `InlineCode.value` is literal text, not inline children; it
                // inherits any surrounding emphasis and records its byte range
                // for the rounded wash the renderer paints behind the glyphs.
                let start = runs.text.len();
                runs.text.push_str(&c.value);
                let end = runs.text.len();
                runs.code_ranges.push(start..end);
                if let Some(hs) = active.highlight() {
                    runs.highlights.push((start..end, hs));
                }
            }
            Node::Delete(d) => {
                let mut a = active;
                a.strikethrough = true;
                collect_inline(&d.children, a, runs);
            }
            Node::Link(l) => {
                let start = runs.text.len();
                collect_inline(&l.children, active, runs);
                let end = runs.text.len();
                if end > start {
                    runs.link_spans.push(LinkSpan {
                        range: start..end,
                        url: l.url.clone(),
                        kind: LinkKind::Url,
                    });
                }
            }
            Node::Image(i) => {
                runs.text.push_str(&i.alt);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_paragraph_and_heading() {
        let blocks = parse("# Title\n\nbody text");
        assert!(matches!(
            blocks.first(),
            Some(Block::Heading { depth: 1, .. })
        ));
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn parses_code_block_with_lang() {
        let blocks = parse("```rust\nfn main() {}\n```");
        let (lang, value) = match blocks.first() {
            Some(Block::Code { lang, value }) => (lang.clone(), value.clone()),
            _ => panic!("expected code block"),
        };
        assert_eq!(lang.as_deref(), Some("rust"));
        assert!(value.contains("fn main"));
    }

    #[test]
    fn parses_list_ordered_and_unordered() {
        assert!(matches!(
            parse("- a\n- b").first(),
            Some(Block::List { ordered: false, .. })
        ));
        assert!(matches!(
            parse("1. a\n2. b").first(),
            Some(Block::List { ordered: true, .. })
        ));
    }

    #[test]
    fn inline_collects_bold_italic_code_overlays() {
        let blocks = parse("plain **bold** *it* `code`");
        let runs = match blocks.first() {
            Some(Block::Paragraph(r)) => r.clone(),
            _ => panic!("expected paragraph"),
        };
        assert!(runs.text.contains("bold"));
        assert!(runs.text.contains("code"));
        assert!(!runs.highlights.is_empty());
        // The `code` segment is recorded as a code range, not a highlight bg.
        assert_eq!(runs.code_ranges.len(), 1);
    }

    #[test]
    fn blockquote_nests_children() {
        let blocks = parse("> quoted\n> more");
        match blocks.first() {
            Some(Block::Blockquote(inner)) => assert!(!inner.is_empty()),
            _ => panic!("expected blockquote"),
        }
    }

    #[test]
    fn diff_lang_routes_to_diff_block() {
        let blocks = parse("```diff\n+added\n-removed\n```");
        match blocks.first() {
            Some(Block::Diff { value }) => {
                assert!(value.contains("+added"));
                assert!(value.contains("-removed"));
            }
            _ => panic!("expected diff block"),
        }
    }

    #[test]
    fn conflict_markers_route_to_conflict_block() {
        let src =
            "```text\n<<<<<<< HEAD\nfn a() {}\n=======\nfn a() -> u32 { 0 }\n>>>>>>> main\n```";
        let blocks = parse(src);
        match blocks.first() {
            Some(Block::Conflict { value }) => {
                assert!(value.contains("<<<<<<< HEAD"));
                assert!(value.contains(">>>>>>> main"));
                assert!(value.contains("======="));
            }
            _ => panic!("expected conflict block"),
        }
    }

    #[test]
    fn conflict_takes_precedence_over_diff_lang() {
        // A ```diff block that actually contains conflict markers routes to
        // Conflict, not Diff — the conflict structure is more specific.
        let src = "```diff\n<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> main\n```";
        assert!(matches!(parse(src).first(), Some(Block::Conflict { .. })));
    }

    #[test]
    fn lone_marker_does_not_misroute_plain_code() {
        // A single stray `>>>>>>>` in commentary must not route a real code
        // block into the conflict renderer — both markers are required.
        let src = "```rust\n// >>>>>>> nothing here\nfn main() {}\n```";
        assert!(matches!(parse(src).first(), Some(Block::Code { .. })));
    }

    #[test]
    fn parses_table_rows() {
        let blocks = parse("| a | b |\n| --- | --- |\n| 1 | 2 |\n");
        match blocks.first() {
            // mdast consumes the delimiter row as alignment metadata, so
            // children are header + one body row.
            Some(Block::Table { rows, .. }) => assert_eq!(rows.len(), 2),
            _ => panic!("expected table"),
        }
    }

    #[test]
    fn task_list_carries_checked_state() {
        let blocks = parse("- [x] done\n- [ ] todo\n");
        match blocks.first() {
            Some(Block::List { items, .. }) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].checked, Some(true));
                assert_eq!(items[1].checked, Some(false));
            }
            _ => panic!("expected list"),
        }
    }

    /// Nested inline formatting (bold + strikethrough) must emit one combined,
    /// non-overlapping range per text segment. Overlapping ranges misalign run
    /// lengths and slice mid-codepoint — the `粗体中的删除` panic.
    #[test]
    fn nested_inline_formats_emit_non_overlapping_ranges() {
        let blocks = parse("**~~粗体中的删除~~**");
        let runs = match blocks.first() {
            Some(Block::Paragraph(r)) => r.clone(),
            _ => panic!("expected paragraph"),
        };
        assert_eq!(runs.text, "粗体中的删除");
        // Every range end must land on a UTF-8 boundary of the rendered text.
        for (range, _) in &runs.highlights {
            assert!(
                runs.text.is_char_boundary(range.start),
                "start {range:?} not a char boundary"
            );
            assert!(
                runs.text.is_char_boundary(range.end),
                "end {range:?} not a char boundary"
            );
        }
        // Ranges are sorted by start and strictly non-overlapping.
        let mut prev_end = 0;
        for (range, _) in &runs.highlights {
            assert!(
                range.start >= prev_end,
                "range {range:?} overlaps or is unsorted (prev_end={prev_end})"
            );
            prev_end = range.end;
        }
    }

    #[test]
    fn explicit_link_preserves_url() {
        let blocks = parse("see [manox](https://github.com/dspo/manox) repo");
        let runs = match blocks.first() {
            Some(Block::Paragraph(r)) => r.clone(),
            _ => panic!("expected paragraph"),
        };
        assert_eq!(runs.link_spans.len(), 1);
        assert_eq!(runs.link_spans[0].url, "https://github.com/dspo/manox");
        assert_eq!(runs.link_spans[0].kind, LinkKind::Url);
        assert_eq!(&runs.text[runs.link_spans[0].range.clone()], "manox");
    }

    #[test]
    fn auto_detects_bare_url() {
        let blocks = parse("Visit https://example.com/path for details.");
        let runs = match blocks.first() {
            Some(Block::Paragraph(r)) => r.clone(),
            _ => panic!("expected paragraph"),
        };
        assert_eq!(runs.link_spans.len(), 1);
        assert_eq!(runs.link_spans[0].url, "https://example.com/path");
        assert_eq!(runs.link_spans[0].kind, LinkKind::Url);
    }

    #[test]
    fn bare_urls_are_not_double_linked_with_explicit_links() {
        let blocks = parse("[click](https://a.com) and https://a.com again");
        let runs = match blocks.first() {
            Some(Block::Paragraph(r)) => r.clone(),
            _ => panic!("expected paragraph"),
        };
        // Explicit link "click" + bare URL "https://a.com" = 2 spans.
        // The bare URL portion of the explicit link's text is not double-linked.
        assert_eq!(runs.link_spans.len(), 2);
    }

    #[test]
    fn auto_detects_file_path_with_extension() {
        let blocks = parse("see crates/agent/src/thread.rs for details");
        let runs = match blocks.first() {
            Some(Block::Paragraph(r)) => r.clone(),
            _ => panic!("expected paragraph"),
        };
        assert_eq!(runs.link_spans.len(), 1);
        assert_eq!(runs.link_spans[0].url, "crates/agent/src/thread.rs");
        assert_eq!(runs.link_spans[0].kind, LinkKind::FilePath);
    }

    #[test]
    fn auto_detects_file_path_with_line_number() {
        let blocks = parse("see crates/agent/src/thread.rs:508 for the field");
        let runs = match blocks.first() {
            Some(Block::Paragraph(r)) => r.clone(),
            _ => panic!("expected paragraph"),
        };
        assert_eq!(runs.link_spans.len(), 1);
        assert_eq!(runs.link_spans[0].url, "crates/agent/src/thread.rs:508");
        assert_eq!(runs.link_spans[0].kind, LinkKind::FilePath);
    }

    #[test]
    fn absolute_path_detected() {
        let blocks = parse("file at /Users/me/project/src/main.rs is interesting");
        let runs = match blocks.first() {
            Some(Block::Paragraph(r)) => r.clone(),
            _ => panic!("expected paragraph"),
        };
        assert_eq!(runs.link_spans.len(), 1);
        assert_eq!(runs.link_spans[0].url, "/Users/me/project/src/main.rs");
        assert_eq!(runs.link_spans[0].kind, LinkKind::FilePath);
    }

    #[test]
    fn bare_filename_without_directory_is_not_a_path() {
        let blocks = parse("see README.md for instructions");
        let runs = match blocks.first() {
            Some(Block::Paragraph(r)) => r.clone(),
            _ => panic!("expected paragraph"),
        };
        // "README.md" has no `/`, so it is not detected as a path.
        assert!(runs.link_spans.is_empty());
    }
}
